//! One explicitly provisioned resource call on a disposable root session.
//!
//! Nothing here registers a tool, starts a watcher, or enables writers on
//! existing sessions. A wakeup is only a reason to reconcile native records.
//! The serving adapter owns resource-local authorization and safe output.

mod journal;

use super::{
    Session, TURN_ATTEMPTS_STORE, TURN_TRANSCRIPT_STORE, TurnCallIdentity, TurnRequestId,
    TurnRequestState, TurnTranscriptMessage, TurnTranscriptRecord, index_attempt_states,
    turn_transcript_key,
    wait_resume::{ExactWait, plan_exact_wait},
    worker_links::WorkerCallSite,
};
use crate::runtime::ToolResultOutcome;
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, TimeDelta, Utc};
use eidetica::{
    AccountSession, Database, Entry, PublicKey, Snapshot, Transaction,
    auth::{
        crypto::verify_entry_signature,
        types::{AuthKey, Permission, SigKey},
    },
    crdt::Doc,
    entry::ID,
    service::protocol::ReadScope,
    store::{DocStore, SettingsStore, Table},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, future::Future, path::Path};

const ROOT_MARKER: &str = "fixed_call_root_v1";
const SETTINGS: &str = "_settings";
const POLICY: &str = "fixed_call_binding_v1";
const CONTROL: &str = "fixed_call_control_v1";
const RECORD: &str = "record";
const MAX_WAIT_SECONDS: i64 = 60;
const MAX_ARGUMENT_BYTES: usize = 2048;
const MAX_RESULT_BYTES: usize = 16 * 1024;
// Includes JSON escaping of the bounded arguments/result and call metadata.
const MAX_RECORD_BYTES: usize = 128 * 1024;

/// Exact placement, as configured independently on the requesting and serving
/// side. The model does not select a login, room, or fallback operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub transport: String,
    pub login: String,
    pub channel: String,
    pub operation: String,
}

/// Pin this on both peers during provisioning; do not learn a target or key
/// from a request or take the current settings as the local adapter config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    session: ID,
    generation: String,
    resource: Resource,
    requester: PublicKey,
    fulfiller: PublicKey,
}

impl Binding {
    /// Allocate a fresh private journal on the serving peer *before* enabling
    /// the binding. A lost journal may not be recreated at its old generation.
    pub fn provision(
        path: &Path,
        session: ID,
        resource: Resource,
        requester: PublicKey,
        fulfiller: PublicKey,
    ) -> Result<Self> {
        let binding = Self {
            session,
            generation: uuid::Uuid::new_v4().to_string(),
            resource,
            requester,
            fulfiller,
        };
        binding.validate()?;
        journal::provision(path, &binding)?;
        Ok(binding)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.requester != self.fulfiller,
            "operators must be distinct"
        );
        uuid::Uuid::parse_str(&self.generation)?;
        let Resource {
            transport,
            login,
            channel,
            operation,
        } = &self.resource;
        for field in [transport, login, channel, operation] {
            ensure!(
                !field.trim().is_empty()
                    && field.len() <= 256
                    && !field.chars().any(char::is_control),
                "invalid fixed resource field"
            );
        }
        Ok(())
    }
}

/// A disposable session is allocated explicitly by its Admin. In particular,
/// a normal Worker, Agent job, or legacy session cannot be retrofitted by
/// setting a mutable `kind`/parent flag. The marker must be in genesis.
pub async fn allocate_root(user: &mut AccountSession, admin: &PublicKey) -> Result<Database> {
    let mut settings = Doc::new();
    settings.set(ROOT_MARKER, true);
    let db = user.create_database(settings, admin).await?;
    Session::initialize_turn_schema(&db).await?;
    Ok(db)
}

/// Install the policy and the two direct non-Admin grants atomically. No
/// metadata-tree delegation, ambient/global access, or change to other keys.
/// Recovery can read back the same installed binding, never retarget it.
pub async fn install(db: &Database, binding: &Binding) -> Result<()> {
    binding.validate()?;
    let is_direct_admin = db.auth_identity().is_some_and(|identity| {
        matches!(identity, SigKey::Direct { .. })
            && identity
                .hint()
                .pubkey
                .as_ref()
                .is_some_and(|pk| *identity == SigKey::from_pubkey(pk))
    });
    ensure!(
        is_direct_admin && db.current_permission().await?.can_admin(),
        "binding requires current direct session Admin"
    );
    require_root(db, binding).await?;
    let (txn, _) = verified_transaction(db).await?;
    let settings = txn.get_settings()?;
    let auth = closed_auth(&settings).await?;
    if let Some(installed) = read_binding(&settings).await? {
        ensure!(
            installed == *binding,
            "cannot replace an existing fixed binding"
        );
        require_writers(&settings, binding).await?;
        return Ok(());
    }
    for key in [&binding.requester, &binding.fulfiller] {
        match auth.get_key_by_pubkey(key) {
            Ok(grant) => ensure!(is_writer(&grant), "would change an existing operator grant"),
            Err(eidetica::Error::Auth(error))
                if matches!(
                    error.as_ref(),
                    eidetica::auth::AuthError::KeyNotFound { .. }
                ) =>
            {
                settings
                    .set_auth_key(key, AuthKey::active(None, Permission::Write(10)))
                    .await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    txn.get_store::<DocStore>(SETTINGS)
        .await?
        .set_string(POLICY, serde_json::to_string(binding)?)
        .await?;
    txn.commit().await?;
    Ok(())
}

/// Arguments must be safe to replicate to the session's readers. `context`
/// names the original context frontier; downtime counts against `deadline`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    binding: Binding,
    call: WorkerCallSite,
    arguments: String,
    context: Snapshot,
    transcript_digest: String,
    issued_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
}

impl Request {
    fn validate(&self) -> Result<()> {
        let window = self.deadline - self.issued_at;
        ensure!(
            self.issued_at <= Utc::now()
                && window > TimeDelta::zero()
                && window <= TimeDelta::seconds(MAX_WAIT_SECONDS),
            "invalid fixed call deadline"
        );
        ensure!(
            self.arguments.len() <= MAX_ARGUMENT_BYTES,
            "fixed call arguments exceed bound"
        );
        ensure!(
            serde_json::from_str::<serde_json::Value>(&self.arguments)?.is_object(),
            "fixed call arguments must be an object"
        );
        Ok(())
    }
}

/// Only a screened, bounded reply may reach the carrier. `Unknown` means
/// there is no provable completion after the durable start, not a retry cue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Answer {
    Reply(String),
    Unavailable,
    Unknown,
}

impl Answer {
    fn validate(&self) -> Result<()> {
        ensure!(
            !matches!(self, Self::Reply(body) if body.len() > MAX_RESULT_BYTES),
            "fixed result exceeds bound"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    // One native signed entry is both the issued request AND its wait intent.
    Request(Box<Request>),
    Answer { request: ID, answer: Answer },
    Cancel { request: ID },
    Settled { request: ID },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Requester,
    Fulfiller,
}

/// Handle opened under one configured direct operator. Callers serialize
/// executor admission/resumption on the owning peer as for other turns.
pub struct FixedCall {
    db: Database,
    binding: Binding,
    role: Role,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Wait {
    Pending { deadline: DateTime<Utc> },
    Paired(Answer),
    Expired,
    Cancelled,
    Settled,
}

struct Observation {
    txn: Transaction,
    request: Option<(ID, Request)>,
    answer: Option<Answer>,
    cancelled: bool,
    settled: bool,
}

impl FixedCall {
    /// Open as one exact account-held key. In service mode this binds the
    /// read channel as well as the signing key to the same native identity.
    /// Never fall back to the login key or a delegated writer.
    pub async fn open(user: &AccountSession, binding: Binding, key: &PublicKey) -> Result<Self> {
        ensure!(
            key == &binding.requester || key == &binding.fulfiller,
            "key is not a bound operator"
        );
        let db = user.open_database_with_key(&binding.session, key).await?;
        Self::open_bound(db, binding).await
    }

    async fn open_bound(db: Database, binding: Binding) -> Result<Self> {
        binding.validate()?;
        require_root(&db, &binding).await?;
        let role = if db.auth_identity() == Some(&SigKey::from_pubkey(&binding.requester)) {
            Role::Requester
        } else if db.auth_identity() == Some(&SigKey::from_pubkey(&binding.fulfiller)) {
            Role::Fulfiller
        } else {
            bail!("handle is not a bound direct operator");
        };
        let endpoint = Self { db, binding, role };
        endpoint.transaction().await?;
        Ok(endpoint)
    }

    /// Publish only an already admitted, final pending call, with its original
    /// effective arguments. On uncertain publication, observe; do not reissue.
    pub async fn issue(
        &mut self,
        call: WorkerCallSite,
        arguments: String,
        context: Snapshot,
        deadline: DateTime<Utc>,
    ) -> Result<ID> {
        self.require_role(Role::Requester)?;
        let observation = self.observe().await?;
        ensure!(
            observation.request.is_none(),
            "binding already issued a call"
        );
        ensure!(!context.is_empty(), "missing original context frontier");
        Session::require_accepted_history_frontier(&self.db, &context).await?;
        let mut request = Request {
            binding: self.binding.clone(),
            call,
            arguments,
            context,
            transcript_digest: String::new(),
            issued_at: Utc::now(),
            deadline,
        };
        request.validate()?;
        let records = pending_records(&observation.txn, &request.call).await?;
        plan(&request, &records)?;
        request.transcript_digest = digest(&records)?;
        ensure!(
            Utc::now() < deadline,
            "fixed call expired before publication"
        );
        write_record(&observation.txn, &Record::Request(Box::new(request))).await?;
        Ok(observation.txn.commit().await?)
    }

    /// One verified reconciliation on the serving peer. The adapter callback
    /// must use only this resource, enforce its local permissions and limits,
    /// and screen the reply before returning it. Never pass a raw tool host.
    /// Returns `None` for no runnable call, including duplicate deliveries of
    /// an already-published result. Journal ownership is peer-local, not a lease.
    pub async fn fulfill<F, Fut>(&self, journal_path: &Path, effect: F) -> Result<Option<Answer>>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Answer>,
    {
        self.require_role(Role::Fulfiller)?;
        let mut journal = journal::Journal::open(journal_path, &self.binding)?;
        let current = self.observe().await?;
        let Some((id, request)) = current.request else {
            return Ok(None);
        };
        if current.cancelled
            || current.settled
            || current.answer.is_some()
            || Utc::now() >= request.deadline
        {
            return Ok(None);
        }
        let state = journal.state(&id)?;
        let answer = match state {
            journal::State::Saved(answer) => answer,
            journal::State::Started => {
                let answer = Answer::Unknown;
                journal.save(&id, &answer)?;
                answer
            }
            journal::State::Empty => {
                validate_pending(&current.txn, &request).await?;
                journal.start(&id)?;
                // Starting is a durable, conservative fence. Even if authority
                // changes before we reach the effect, restart cannot clear it.
                let before = self.observe().await?;
                if !before.is_pending(&id) || Utc::now() >= request.deadline {
                    return Ok(None);
                }
                validate_pending(&before.txn, &request).await?;
                let remaining = (request.deadline - Utc::now()).to_std().unwrap_or_default();
                if remaining.is_zero() {
                    return Ok(None);
                }
                let answer = tokio::time::timeout(remaining, async {
                    effect(request.arguments.clone()).await
                })
                .await
                .unwrap_or(Answer::Unknown);
                // Oversized output is never put in either journal or session.
                let answer = if answer.validate().is_ok() {
                    answer
                } else {
                    Answer::Unavailable
                };
                journal.save(&id, &answer)?;
                answer
            }
        };
        let fresh = self.observe().await?;
        if !fresh.is_pending(&id) || Utc::now() >= request.deadline {
            return Ok(None);
        }
        validate_pending(&fresh.txn, &request).await?;
        if Utc::now() >= request.deadline {
            return Ok(None);
        }
        write_record(
            &fresh.txn,
            &Record::Answer {
                request: id,
                answer: answer.clone(),
            },
        )
        .await?;
        fresh.txn.commit().await?;
        Ok(Some(answer))
    }

    pub async fn cancel(&mut self, call: &WorkerCallSite) -> Result<()> {
        self.require_role(Role::Requester)?;
        let observation = self.observe().await?;
        let (id, _) = observation.exact(call)?;
        if !observation.cancelled && !observation.settled {
            write_record(
                &observation.txn,
                &Record::Cancel {
                    request: id.clone(),
                },
            )
            .await?;
            observation.txn.commit().await?;
        }
        Ok(())
    }

    /// Recover the exact call of an already-selected interrupted attempt.
    /// This read does not grant execution admission or publish a request.
    pub async fn pending_call(
        &self,
        request_id: &TurnRequestId,
        attempt_id: &str,
    ) -> Result<Option<WorkerCallSite>> {
        self.require_role(Role::Requester)?;
        let observation = self.observe().await?;
        let Some((_, request)) = observation.request else {
            return Ok(None);
        };
        if observation.settled
            || &request.call.request_id != request_id
            || request.call.attempt_id != attempt_id
        {
            return Ok(None);
        }
        validate_pending(&observation.txn, &request).await?;
        Ok(Some(request.call))
    }

    /// Reload only the original wait. A wrong invocation, missing checkpoint
    /// or advanced/changed transcript holds; none of these paths submits work.
    /// Pair the native result and retire the wait in the same transaction. This
    /// pilot has no result hooks or automatic next-model dispatch.
    pub async fn resume(&mut self, call: &WorkerCallSite) -> Result<Wait> {
        self.require_role(Role::Requester)?;
        let observation = self.observe().await?;
        let (id, request) = observation.exact(call)?;
        let id = id.clone();
        if observation.settled {
            return Ok(Wait::Settled);
        }
        Session::require_accepted_history_frontier(&self.db, &request.context).await?;
        let (sequence, identity) = validate_pending(&observation.txn, request).await?;
        let (wait, outcome, output) = if observation.cancelled {
            (
                Wait::Cancelled,
                ToolResultOutcome::Cancelled,
                "Fixed resource wait cancelled".into(),
            )
        } else if Utc::now() >= request.deadline {
            (
                Wait::Expired,
                ToolResultOutcome::TimedOut,
                "Fixed resource deadline expired; execution is not confirmed".into(),
            )
        } else {
            let Some(answer) = observation.answer else {
                return Ok(Wait::Pending {
                    deadline: request.deadline,
                });
            };
            let (outcome, output) = match &answer {
                Answer::Reply(result) => (ToolResultOutcome::Success, result.clone()),
                Answer::Unavailable => (
                    ToolResultOutcome::Unavailable,
                    "Fixed resource unavailable".into(),
                ),
                Answer::Unknown => (
                    ToolResultOutcome::Error,
                    "Fixed resource outcome unknown".into(),
                ),
            };
            (Wait::Paired(answer), outcome, output)
        };
        let record = TurnTranscriptRecord {
            request_id: call.request_id.clone(),
            attempt_id: call.attempt_id.clone(),
            sequence,
            timestamp: Utc::now(),
            identity: Some(identity),
            message: TurnTranscriptMessage::ToolResult {
                model_sequence: call.model_sequence,
                call_index: call.call_index,
                call_id: call.call_id.clone(),
                name: self.binding.resource.operation.clone(),
                output,
                outcome,
            },
        };
        observation
            .txn
            .get_store::<Table<TurnTranscriptRecord>>(TURN_TRANSCRIPT_STORE)
            .await?
            .set(turn_transcript_key(&record), record)
            .await?;
        write_record(&observation.txn, &Record::Settled { request: id }).await?;
        observation.txn.commit().await?;
        Ok(wait)
    }

    fn require_role(&self, role: Role) -> Result<()> {
        ensure!(self.role == role, "wrong fixed call role");
        Ok(())
    }

    async fn transaction(&self) -> Result<(Transaction, Vec<ID>)> {
        // Current native verdict on this actual signing handle, separate from
        // historical Entry validation and direct grants of the other operator.
        ensure!(
            matches!(self.db.current_permission().await?, Permission::Write(_)),
            "native operator Write is not current"
        );
        let (txn, tips) = verified_transaction(&self.db).await?;
        let settings = txn.get_settings()?;
        ensure!(
            read_binding(&settings).await?.as_ref() == Some(&self.binding),
            "fixed binding is absent or changed"
        );
        require_writers(&settings, &self.binding).await?;
        Ok((txn, tips))
    }

    async fn observe(&self) -> Result<Observation> {
        let (txn, tips) = self.transaction().await?;
        let entries = self
            .db
            .get_store_entries(CONTROL, &tips, ReadScope::Verified)
            .await?;
        let mut observation = Observation {
            txn,
            request: None,
            answer: None,
            cancelled: false,
            settled: false,
        };
        let mut others = Vec::new();
        for entry in entries {
            let Ok(record) = decode_record(&entry) else {
                continue;
            };
            let role = if matches!(record, Record::Answer { .. }) {
                Role::Fulfiller
            } else {
                Role::Requester
            };
            if !self.issued_by(&entry, role).await? {
                continue;
            }
            match record {
                Record::Request(request) => {
                    if request.binding != self.binding || request.validate().is_err() {
                        continue;
                    }
                    let at_issue = self
                        .db
                        .new_transaction_at(&Snapshot::from(entry.parents()?))
                        .await?;
                    let settings = at_issue.get_settings()?;
                    ensure!(
                        read_binding(&settings).await?.as_ref() == Some(&self.binding),
                        "request was not issued under its binding"
                    );
                    require_writers(&settings, &self.binding).await?;
                    ensure!(
                        observation.request.is_none(),
                        "multiple requests for the same fixed binding"
                    );
                    observation.request = Some((entry.id(), *request));
                }
                other => others.push(other),
            }
        }
        if let Some((id, _)) = &observation.request {
            for record in others {
                match record {
                    Record::Answer { request, answer }
                        if request == *id && answer.validate().is_ok() =>
                    {
                        ensure!(
                            observation
                                .answer
                                .as_ref()
                                .is_none_or(|previous| *previous == answer),
                            "conflicting fixed results"
                        );
                        observation.answer = Some(answer);
                    }
                    Record::Cancel { request } if request == *id => observation.cancelled = true,
                    Record::Settled { request } if request == *id => observation.settled = true,
                    _ => {}
                }
            }
        }
        Ok(observation)
    }

    async fn issued_by(&self, entry: &Entry, role: Role) -> Result<bool> {
        let expected = match role {
            Role::Requester => &self.binding.requester,
            Role::Fulfiller => &self.binding.fulfiller,
        };
        // Native verification is historical. Match the concrete public key
        // as well: a valid signature by an ordinary session writer is not it.
        Ok(entry.root().as_ref() == Some(&self.binding.session)
            && entry.auth().key == SigKey::from_pubkey(expected)
            && verify_entry_signature(entry, expected).is_ok()
            && self.db.verify_entry_signature(entry.id()).await?)
    }
}

impl Observation {
    fn exact(&self, call: &WorkerCallSite) -> Result<(&ID, &Request)> {
        let (id, request) = self
            .request
            .as_ref()
            .context("exact fixed request/checkpoint unavailable")?;
        ensure!(
            request.call == *call,
            "fixed wait belongs to a different invocation"
        );
        Ok((id, request))
    }

    fn is_pending(&self, id: &ID) -> bool {
        self.request.as_ref().is_some_and(|(seen, _)| seen == id)
            && self.answer.is_none()
            && !self.cancelled
            && !self.settled
    }
}

async fn verified_transaction(db: &Database) -> Result<(Transaction, Vec<ID>)> {
    // As in the native Provider-record proof: ReadScope does NOT sanitize
    // caller-provided store tips. Get both frontiers from a verified read.
    let context = db
        .transaction_context(&[CONTROL.into()], ReadScope::Verified)
        .await?;
    let tips = context.subtree_parents[CONTROL]
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    let parents = Snapshot::from(
        context
            .main_parents
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
    );
    Session::history_at(db, &parents).await?.require_legacy()?;
    Ok((db.new_transaction_at(&parents).await?, tips))
}

async fn require_root(db: &Database, binding: &Binding) -> Result<()> {
    ensure!(*db.root_id() == binding.session, "wrong session root");
    let root = db.get_entry(db.root_id()).await?;
    let genesis: Doc = serde_json::from_slice(root.data(SETTINGS)?)?;
    ensure!(
        root.is_root()
            && genesis.get_as::<bool>(ROOT_MARKER) == Some(true)
            && db.verify_entry_signature(root.id()).await?,
        "not a provisioned fixed-call root"
    );
    Ok(())
}

async fn read_binding(settings: &SettingsStore) -> Result<Option<Binding>> {
    match settings.get_string(POLICY).await {
        Ok(raw) if raw.len() <= MAX_RECORD_BYTES => Ok(Some(serde_json::from_str(&raw)?)),
        Ok(_) => bail!("fixed binding exceeds bound"),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn closed_auth(settings: &SettingsStore) -> Result<eidetica::auth::settings::AuthSettings> {
    let doc = settings.get_all().await?;
    ensure!(
        doc.get("auth.global").is_none() && doc.get("auth.delegations").is_none(),
        "global/delegated session authority is outside the fixed-call proof"
    );
    Ok(settings.auth_snapshot().await?)
}

fn is_writer(key: &AuthKey) -> bool {
    key.is_active() && matches!(key.permissions(), Permission::Write(_))
}

async fn require_writers(settings: &SettingsStore, binding: &Binding) -> Result<()> {
    let auth = closed_auth(settings).await?;
    for key in [&binding.requester, &binding.fulfiller] {
        ensure!(
            auth.get_key_by_pubkey(key)
                .is_ok_and(|grant| is_writer(&grant)),
            "fixed operator lacks current direct Write"
        );
    }
    Ok(())
}

fn decode_record(entry: &Entry) -> Result<Record> {
    let data = entry.data(CONTROL)?;
    ensure!(
        data.len() <= 2 * MAX_RECORD_BYTES,
        "fixed call entry exceeds bound"
    );
    let doc: Doc = serde_json::from_slice(data)?;
    let raw = doc.get_as::<&str>(RECORD).context("no fixed call record")?;
    ensure!(
        raw.len() <= MAX_RECORD_BYTES,
        "fixed call record exceeds bound"
    );
    Ok(serde_json::from_str(raw)?)
}

async fn write_record(txn: &Transaction, record: &Record) -> Result<()> {
    let raw = serde_json::to_string(record)?;
    ensure!(
        raw.len() <= MAX_RECORD_BYTES,
        "fixed call record exceeds bound"
    );
    txn.get_store::<DocStore>(CONTROL)
        .await?
        .set_string(RECORD, raw)
        .await?;
    Ok(())
}

async fn pending_records(
    txn: &Transaction,
    call: &WorkerCallSite,
) -> Result<Vec<TurnTranscriptRecord>> {
    let attempts = txn
        .get_store::<Table<super::TurnAttempt>>(TURN_ATTEMPTS_STORE)
        .await?
        .search(|a| a.request_id == call.request_id)
        .await?;
    ensure!(
        matches!(index_attempt_states(attempts, &HashSet::new()).remove(&call.request_id),
        Some(TurnRequestState::Interrupted { attempt_id }) if attempt_id == call.attempt_id),
        "the original attempt is no longer pending"
    );
    let mut records: Vec<_> = txn
        .get_store::<Table<TurnTranscriptRecord>>(TURN_TRANSCRIPT_STORE)
        .await?
        .search(|record| record.attempt_id == call.attempt_id)
        .await?
        .into_iter()
        .map(|(_, record)| record)
        .collect();
    records.sort_by_key(|r| r.sequence);
    ensure!(
        records.iter().all(|r| r
            .identity
            .as_ref()
            .is_some_and(|i| i.agent == call.agent && i.execution_id == call.execution_id)),
        "fixed wait execution branch changed"
    );
    Ok(records)
}

fn plan(request: &Request, records: &[TurnTranscriptRecord]) -> Result<(u64, TurnCallIdentity)> {
    let call = &request.call;
    let (_, sequence) = plan_exact_wait(
        ExactWait {
            request_id: &call.request_id,
            attempt_id: &call.attempt_id,
            model_sequence: call.model_sequence,
            call_index: call.call_index,
            call_id: &call.call_id,
            tool: &request.binding.resource.operation,
            arguments: &request.arguments,
            checkpointed_at: request.issued_at,
            deadline: request.deadline,
            policy_deadline: request.deadline,
            policy_timeout_seconds: MAX_WAIT_SECONDS as u64,
        },
        records,
    )
    .map_err(anyhow::Error::msg)?;
    let identity = records
        .last()
        .and_then(|r| r.identity.clone())
        .context("no pending call identity")?;
    Ok((sequence, identity))
}

fn digest(records: &[TurnTranscriptRecord]) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(records)?)
        .to_hex()
        .to_string())
}

async fn validate_pending(txn: &Transaction, request: &Request) -> Result<(u64, TurnCallIdentity)> {
    let records = pending_records(txn, &request.call).await?;
    ensure!(
        digest(&records)? == request.transcript_digest,
        "checkpointed transcript changed"
    );
    plan(request, &records)
}

#[cfg(test)]
mod tests;
