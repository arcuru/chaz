//! Non-runnable job admission staging. No agent turn may consume a staged row.
//!
//! Eidetica commits one database at a time. The parent intent is written first;
//! child creation publishes a catalog row before delegation and job metadata
//! can be committed. A retry may recover a fully staged child, but must hold
//! a catalog row without metadata as uncertain rather than creating another.

use super::{SessionIndex, SessionRegistry};
use crate::grants::Grants;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const INTENTS: &str = "job_intents";
const DEFINITION: &str = "job_definition";

/// Trusted-local broad-scope only. Workspace and private jobs are not admitted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StagedAgentDefinition {
    pub target: String,
    pub task: String,
    pub executor_pubkey: String,
    pub call_depth: usize,
    pub max_call_depth: usize,
    /// Concrete tool names, captured before any turn can run (not glob patterns).
    pub allowed_tools: Vec<String>,
    // Snapshot of the parent effective grant for each pinned tool name.
    // Target policy may narrow this, never replace or widen it.
    pub tool_ceilings: BTreeMap<String, Grants>,
    pub capability_ceiling: Grants,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Intent {
    definition: StagedAgentDefinition,
    child_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ChildDefinition {
    parent_id: String,
    request_hash: String,
    definition: StagedAgentDefinition,
}

/// A staged handle is NOT an accepted runnable job. Its DB has no Directive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedJob {
    pub session_db_id: String,
}

fn source(parent: &str, key: &str) -> String {
    // A catalog label is discovery only, not authority. Never put task text or
    // the caller's request key into this peer-local catalog.
    format!(
        "job-stage:{}",
        blake3::hash(format!("{parent}\0{key}").as_bytes()).to_hex()
    )
}

pub(crate) async fn is_staged_job(db: &eidetica::Database) -> bool {
    crate::db_kind::read_marker(db)
        .await
        .is_some_and(|(kind, name)| {
            kind == crate::db_kind::KIND_SESSION && name.starts_with("job-stage:")
        })
}

fn request_hash(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

async fn read_intent(parent: &eidetica::Database, key: &str) -> anyhow::Result<Option<Intent>> {
    let txn = parent.new_transaction().await?;
    let store = txn.get_store::<DocStore>(INTENTS).await?;
    let doc = store.get_all().await?;
    match doc.get(request_hash(key)) {
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(Some(serde_json::from_str(&json)?))
        }
        None => Ok(None),
    }
}

async fn write_intent(
    parent: &eidetica::Database,
    key: &str,
    intent: &Intent,
) -> anyhow::Result<()> {
    let txn = parent.new_transaction().await?;
    txn.get_store::<DocStore>(INTENTS)
        .await?
        .set_string(request_hash(key), serde_json::to_string(intent)?)
        .await?;
    txn.commit().await?;
    Ok(())
}

async fn read_child(db: &eidetica::Database) -> anyhow::Result<Option<ChildDefinition>> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(DEFINITION).await?;
    let doc = store.get_all().await?;
    match doc.get("v1") {
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(Some(serde_json::from_str(&json)?))
        }
        None => Ok(None),
    }
}

fn matching_rows<'a>(rows: &'a [SessionIndex], label: &str) -> Vec<&'a SessionIndex> {
    rows.iter()
        .filter(|row| row.source.as_deref() == Some(label))
        .collect()
}

impl SessionRegistry {
    /// Stage a definition without writing a runnable Directive. The caller's
    /// stable key is scoped to the parent session. A pending intent whose child
    /// cannot be proven complete is *uncertain* and requires reconciliation;
    /// it is never replaced by another child. This API is not job acceptance.
    ///
    /// Single-executor staging only: this serializes competing callers in
    /// one registry; it is not a cross-process or cross-peer fence.
    pub async fn stage_agent_job(
        &self,
        parent_id: &str,
        request_key: &str,
        definition: StagedAgentDefinition,
    ) -> anyhow::Result<StagedJob> {
        anyhow::ensure!(!request_key.is_empty(), "job request key must not be empty");
        // coding: global lock; use per-key locks if staging throughput matters.
        let _guard = self.job_stage_lock.lock().await;
        let label = source(parent_id, request_key);
        anyhow::ensure!(
            !definition.target.is_empty() && !definition.task.is_empty(),
            "job target and task are required"
        );
        anyhow::ensure!(
            !definition.executor_pubkey.is_empty(),
            "designated executor is required"
        );
        anyhow::ensure!(
            definition.call_depth <= definition.max_call_depth,
            "spawn depth exceeds ceiling"
        );
        anyhow::ensure!(
            definition.tool_ceilings.len() == definition.allowed_tools.len()
                && definition.allowed_tools.iter().all(|name| {
                    !name.is_empty()
                        && name != "spawn_worker"
                        && definition.tool_ceilings.contains_key(name)
                }),
            "job tools require one pinned grant ceiling per permitted name; spawn_worker is not a durable job child"
        );
        let (_, parent) = self.open_session(parent_id).await?;
        let prior = read_intent(&parent, request_key).await?;
        if let Some(ref intent) = prior {
            anyhow::ensure!(
                intent.definition == definition,
                "job request key reused with a different definition"
            );
        }
        // A catalog row may be visible before delegation/metadata exists. Do
        // not trust it as a job or repair an incomplete child by inference.
        let rows = self.list_sessions().await?;
        let matches = matching_rows(&rows, &label);
        anyhow::ensure!(
            matches.len() <= 1,
            "uncertain job submission: multiple catalog children"
        );
        if let Some(row) = matches.first() {
            anyhow::ensure!(
                prior.is_some(),
                "uncertain job submission: child has no parent intent"
            );
            let (_, child) = self.open_session(&row.session_db_id).await?;
            anyhow::ensure!(
                read_child(&child).await?
                    == Some(ChildDefinition {
                        parent_id: parent_id.to_string(),
                        request_hash: request_hash(request_key),
                        definition: definition.clone()
                    }),
                "uncertain job submission: child has no matching definition"
            );
            if prior.as_ref().and_then(|i| i.child_id.as_deref()).is_none() {
                write_intent(
                    &parent,
                    request_key,
                    &Intent {
                        definition,
                        child_id: Some(row.session_db_id.clone()),
                    },
                )
                .await?;
            } else {
                anyhow::ensure!(
                    prior.as_ref().and_then(|i| i.child_id.as_deref())
                        == Some(row.session_db_id.as_str()),
                    "uncertain job submission: parent and catalog disagree"
                );
            }
            return Ok(StagedJob {
                session_db_id: row.session_db_id.clone(),
            });
        }
        anyhow::ensure!(
            prior.is_none(),
            "uncertain job submission: intent exists but child is not discoverable"
        );
        // A failed intent commit may have succeeded; return uncertainty rather
        // than treating an error as proof the key is free.
        write_intent(
            &parent,
            request_key,
            &Intent {
                definition: definition.clone(),
                child_id: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("uncertain job submission for request key: {e}"))?;
        let (id, child) = self
            .create_child_session(parent_id, Some(&label))
            .await
            .map_err(|e| anyhow::anyhow!("uncertain job submission for request key: {e}"))?;
        let txn = child.new_transaction().await?;
        txn.get_store::<DocStore>(DEFINITION)
            .await?
            .set_string(
                "v1",
                serde_json::to_string(&ChildDefinition {
                    parent_id: parent_id.to_string(),
                    request_hash: request_hash(request_key),
                    definition: definition.clone(),
                })?,
            )
            .await?;
        txn.commit()
            .await
            .map_err(|e| anyhow::anyhow!("uncertain job submission for request key: {e}"))?;
        write_intent(
            &parent,
            request_key,
            &Intent {
                definition,
                child_id: Some(id.0.clone()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("uncertain job submission for request key: {e}"))?;
        Ok(StagedJob {
            session_db_id: id.0,
        })
    }
}

/// The executor's accepted command binds a staged definition to exactly one
/// runnable Directive. A parent command receipt may lag this child commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct AcceptedAgentJob {
    pub parent_id: String,
    pub request_key_hash: String,
    pub definition: StagedAgentDefinition,
    pub directive_id: String,
}

const ACCEPTANCE: &str = "job_acceptance";

pub(crate) async fn read_accepted_job(
    db: &eidetica::Database,
) -> anyhow::Result<Option<AcceptedAgentJob>> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(ACCEPTANCE).await?;
    let doc = store.get_all().await?;
    match doc.get("v1") {
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(Some(serde_json::from_str(&json)?))
        }
        None => Ok(None),
    }
}

pub(crate) async fn acceptance_matches_staging(
    db: &eidetica::Database,
    accepted: &AcceptedAgentJob,
) -> anyhow::Result<bool> {
    Ok(read_child(db).await?
        == Some(ChildDefinition {
            parent_id: accepted.parent_id.clone(),
            request_hash: accepted.request_key_hash.clone(),
            definition: accepted.definition.clone(),
        }))
}

impl SessionRegistry {
    /// Caller must be the executor after validating the parent command, target,
    /// inherited authority, and child metadata. No direct client uses this API.
    /// Recovery reuses the same accepted handle or holds a conflicting record.
    pub(crate) async fn accept_staged_agent_job(
        &self,
        staged: &StagedJob,
        parent_id: &str,
        request_key: &str,
        definition: &StagedAgentDefinition,
        sender: &str,
    ) -> anyhow::Result<AcceptedAgentJob> {
        let (id, child) = self.open_session(&staged.session_db_id).await?;
        let expected = ChildDefinition {
            parent_id: parent_id.to_string(),
            request_hash: request_hash(request_key),
            definition: definition.clone(),
        };
        anyhow::ensure!(
            read_child(&child).await? == Some(expected),
            "uncertain job acceptance: staged child definition differs"
        );
        if let Some(existing) = read_accepted_job(&child).await? {
            anyhow::ensure!(
                existing.parent_id == parent_id
                    && existing.request_key_hash == request_hash(request_key)
                    && existing.definition == *definition,
                "uncertain job acceptance: accepted record differs"
            );
            return Ok(existing);
        }
        let txn = child.new_transaction().await?;
        let entries = txn
            .get_store::<eidetica::store::Table<super::SessionEntry>>("entries")
            .await?;
        anyhow::ensure!(
            entries.search(|_| true).await?.is_empty(),
            "uncertain job acceptance: child contains entries before admission"
        );
        let directive_id = entries
            .insert(super::SessionEntry {
                sender: sender.to_string(),
                content: definition.task.clone(),
                timestamp: chrono::Utc::now(),
                entry_type: super::EntryType::Directive,
                metadata: None,
                routing: None,
            })
            .await?;
        let accepted = AcceptedAgentJob {
            parent_id: parent_id.to_string(),
            request_key_hash: request_hash(request_key),
            definition: definition.clone(),
            directive_id,
        };
        txn.get_store::<DocStore>(ACCEPTANCE)
            .await?
            .set_string("v1", serde_json::to_string(&accepted)?)
            .await?;
        txn.commit()
            .await
            .map_err(|e| anyhow::anyhow!("uncertain job acceptance for session {}: {e}", id.0))?;
        // The initial catalog notification may predate this commit. Trigger a
        // new scan; startup performs the same scan without relying on events.
        if self
            .new_session_tx
            .send(super::registry::NewSessionEvent {
                session_db_id: id.0.clone(),
                source: None,
            })
            .await
            .is_err()
        {
            tracing::warn!(session_db_id = %id.0, "Accepted job will be picked up on executor restart");
        }
        Ok(accepted)
    }
}

/// A typed result, committed with the matching turn completion rather than
/// inferred from whichever chat entry happens to be last.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobTerminal {
    Success { text: Option<String> },
    Failure { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    pub attempt_id: String,
    pub terminal: JobTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum JobState {
    Queued,
    Running {
        attempt_id: String,
    },
    StartedUnknown {
        attempt_id: String,
        activity_recent: bool,
    },
    Interrupted {
        attempt_id: String,
    },
    Succeeded {
        text: Option<String>,
    },
    Failed {
        message: String,
    },
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded { .. } | Self::Failed { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobStatus {
    pub session_db_id: String,
    pub state: JobState,
}

const JOB_RESULT: &str = "job_result";

pub(crate) async fn read_job_result(db: &eidetica::Database) -> anyhow::Result<Option<JobResult>> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(JOB_RESULT).await?;
    let doc = store.get_all().await?;
    match doc.get("v1") {
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(Some(serde_json::from_str(&json)?))
        }
        None => Ok(None),
    }
}

pub(crate) async fn write_job_result_in_txn(
    txn: &eidetica::transaction::Transaction,
    result: &JobResult,
) -> anyhow::Result<()> {
    txn.get_store::<DocStore>(JOB_RESULT)
        .await?
        .set_string("v1", serde_json::to_string(result)?)
        .await?;
    Ok(())
}

pub(crate) async fn status_from_db(
    db: &eidetica::Database,
    live_attempts: &std::collections::HashSet<String>,
    executor_observer: bool,
) -> anyhow::Result<JobStatus> {
    let accepted = read_accepted_job(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session has no accepted agent job"))?;
    let session_db_id = db.root_id().to_string();
    let session =
        super::Session::new(super::ConversationId(session_db_id.clone()), db.clone()).await;
    let request_id = super::TurnRequestId::parse(accepted.directive_id);
    let request = session
        .turn_request(&request_id, |_| false, live_attempts)
        .await?
        .ok_or_else(|| anyhow::anyhow!("accepted job Directive is missing"))?;
    let state = match request.state {
        super::TurnRequestState::Queued => JobState::Queued,
        super::TurnRequestState::InFlight { attempt_id } => JobState::Running { attempt_id },
        super::TurnRequestState::Interrupted { attempt_id } => {
            if executor_observer {
                JobState::Interrupted { attempt_id }
            } else {
                let activity_recent = super::Session::active_turn_attempts(db)
                    .await?
                    .contains(&attempt_id);
                JobState::StartedUnknown {
                    attempt_id,
                    activity_recent,
                }
            }
        }
        super::TurnRequestState::Completed { attempt_id } => {
            let receipt = read_job_result(db)
                .await?
                .ok_or_else(|| anyhow::anyhow!("completed job lacks terminal receipt"))?;
            anyhow::ensure!(
                receipt.attempt_id == attempt_id,
                "job receipt attempt differs"
            );
            match receipt.terminal {
                JobTerminal::Success { text } => JobState::Succeeded { text },
                JobTerminal::Failure { message } => JobState::Failed { message },
            }
        }
    };
    Ok(JobStatus {
        session_db_id,
        state,
    })
}
impl SessionRegistry {
    /// Open a locally indexed job created by another same-login service
    /// process after this client's User key-map snapshot was loaded.
    /// Mapping a key is only a lookup hint; the service still verifies the
    /// key against the child DB's auth settings on every operation.
    pub async fn open_job_session(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<(super::ConversationId, eidetica::Database)> {
        let row = self
            .list_sessions()
            .await?
            .into_iter()
            .find(|row| row.session_db_id == session_db_id)
            .ok_or_else(|| anyhow::anyhow!("unknown job session"))?;
        anyhow::ensure!(
            row.source
                .as_deref()
                .is_some_and(|s| s.starts_with("job-stage:") || s.starts_with("workflow-stage:")),
            "session is not a locally staged job"
        );
        let root = eidetica::entry::ID::parse(session_db_id)?;
        let mut user = self.user.lock().await;
        let key = user.get_default_key()?;
        user.map_key(&key, &root, eidetica::auth::SigKey::from_pubkey(&key))
            .await?;
        let db = user.open_database_with_key(&root, &key).await?;
        self.local_client_sessions
            .lock()
            .await
            .insert(session_db_id.to_string(), db.clone());
        Ok((super::ConversationId(session_db_id.to_string()), db))
    }
}

impl SessionRegistry {
    /// Resolve a lost acknowledgement from the peer-local catalog and the
    /// accepted child record. An incomplete stage remains uncertain, not a
    /// free key on which to create a duplicate child.
    pub async fn lookup_job_by_key(
        &self,
        parent_id: &str,
        request_key: &str,
    ) -> anyhow::Result<Option<StagedJob>> {
        anyhow::ensure!(!request_key.is_empty(), "job request key must not be empty");
        let label = source(parent_id, request_key);
        let rows = self.list_sessions().await?;
        let matches = matching_rows(&rows, &label);
        anyhow::ensure!(
            matches.len() <= 1,
            "uncertain job request: multiple catalog children"
        );
        let Some(row) = matches.first() else {
            let (_, parent) = self.open_session(parent_id).await?;
            anyhow::ensure!(
                read_intent(&parent, request_key).await?.is_none(),
                "uncertain job request: parent intent has no discoverable child"
            );
            return Ok(None);
        };
        let (_, db) = self.open_job_session(&row.session_db_id).await?;
        let accepted = read_accepted_job(&db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("uncertain job request: child is not accepted"))?;
        anyhow::ensure!(
            accepted.parent_id == parent_id
                && accepted.request_key_hash == request_hash(request_key)
                && acceptance_matches_staging(&db, &accepted).await?,
            "uncertain job request: child acceptance differs from parent key"
        );
        Ok(Some(StagedJob {
            session_db_id: row.session_db_id.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, test_helpers::make_registry};

    fn definition() -> StagedAgentDefinition {
        StagedAgentDefinition {
            target: "default".into(),
            task: "hello".into(),
            executor_pubkey: "ed25519:local".into(),
            call_depth: 1,
            max_call_depth: 3,
            allowed_tools: vec!["calculate".into()],
            tool_ceilings: BTreeMap::from([("calculate".into(), Grants::default())]),
            capability_ceiling: Grants::default(),
        }
    }

    #[tokio::test]
    async fn competing_submitters_share_one_staged_handle() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let results = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            futures::future::join_all((0..24).map(|_| {
                let registry = registry.clone();
                let pid = pid.clone();
                async move {
                    registry
                        .stage_agent_job(&pid, "same.key", definition())
                        .await
                }
            })),
        )
        .await
        .expect("concurrent staging must terminate");
        let ids: std::collections::HashSet<_> = results
            .into_iter()
            .map(|result| result.unwrap().session_db_id)
            .collect();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            matching_rows(
                &registry.list_sessions().await.unwrap(),
                &source(&pid, "same.key")
            )
            .len(),
            1
        );
        let id = ids.into_iter().next().unwrap();
        let (id, child) = registry.open_session(&id).await.unwrap();
        assert!(Session::new(id, child).await.entries().is_empty());
    }

    #[tokio::test]
    async fn competing_mismatched_definitions_never_create_two_children() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let mut other = definition();
        other.capability_ceiling = Grants {
            shell: Some(crate::grants::ShellGrant {
                allow: crate::grants::Allowlist::Only(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (a, b) = tokio::join!(
            registry.stage_agent_job(&pid, "collision", definition()),
            registry.stage_agent_job(&pid, "collision", other)
        );
        assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1);
        let error = a.err().or_else(|| b.err()).unwrap();
        assert!(error.to_string().contains("different definition"));
        assert_eq!(
            matching_rows(
                &registry.list_sessions().await.unwrap(),
                &source(&pid, "collision")
            )
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn staged_job_is_inert_and_lost_ack_retry_returns_same_child() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let first = registry
            .stage_agent_job(&pid, "stable.key", definition())
            .await
            .unwrap();
        let second = registry
            .stage_agent_job(&pid, "stable.key", definition())
            .await
            .unwrap();
        assert_eq!(first, second);
        let (id, child) = registry.open_session(&first.session_db_id).await.unwrap();
        assert!(Session::new(id, child.clone()).await.entries().is_empty());
        assert!(is_staged_job(&child).await);
        assert_eq!(
            read_child(&child).await.unwrap().unwrap().definition,
            definition()
        );
        let mut changed = definition();
        changed.task = "different".into();
        assert!(
            registry
                .stage_agent_job(&pid, "stable.key", changed)
                .await
                .unwrap_err()
                .to_string()
                .contains("different definition")
        );
        assert_eq!(
            matching_rows(
                &registry.list_sessions().await.unwrap(),
                &source(&pid, "stable.key")
            )
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn partial_creation_is_held_without_duplicate() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        write_intent(
            &parent,
            "key",
            &Intent {
                definition: definition(),
                child_id: None,
            },
        )
        .await
        .unwrap();
        let (_, partial) = registry
            .create_child_session(&pid, Some(&source(&pid, "key")))
            .await
            .unwrap();
        let err = registry
            .stage_agent_job(&pid, "key", definition())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("uncertain"));
        assert_eq!(
            matching_rows(
                &registry.list_sessions().await.unwrap(),
                &source(&pid, "key")
            )
            .len(),
            1
        );
        assert!(read_child(&partial).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn recover_child_after_parent_link_write_is_lost() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let key = "lost.ack";
        write_intent(
            &parent,
            key,
            &Intent {
                definition: definition(),
                child_id: None,
            },
        )
        .await
        .unwrap();
        let (_, child) = registry
            .create_child_session(&pid, Some(&source(&pid, key)))
            .await
            .unwrap();
        let child_id = child.root_id().to_string();
        let txn = child.new_transaction().await.unwrap();
        txn.get_store::<DocStore>(DEFINITION)
            .await
            .unwrap()
            .set_string(
                "v1",
                serde_json::to_string(&ChildDefinition {
                    parent_id: pid.clone(),
                    request_hash: request_hash(key),
                    definition: definition(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let staged = registry
            .stage_agent_job(&pid, key, definition())
            .await
            .unwrap();
        assert_eq!(staged.session_db_id, child_id);
        assert_eq!(
            read_intent(&parent, key)
                .await
                .unwrap()
                .unwrap()
                .child_id
                .as_deref(),
            Some(child_id.as_str())
        );
    }

    #[tokio::test]
    async fn staging_refuses_missing_or_untracked_tool_ceilings() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let mut missing = definition();
        missing.tool_ceilings.clear();
        assert!(
            registry
                .stage_agent_job(&pid, "missing", missing)
                .await
                .unwrap_err()
                .to_string()
                .contains("pinned grant ceiling")
        );
        let mut worker = definition();
        worker.allowed_tools = vec!["spawn_worker".into()];
        worker.tool_ceilings = BTreeMap::from([("spawn_worker".into(), Grants::default())]);
        assert!(
            registry
                .stage_agent_job(&pid, "worker", worker)
                .await
                .unwrap_err()
                .to_string()
                .contains("spawn_worker")
        );
        assert!(registry.list_sessions().await.unwrap().iter().all(|s| {
            !s.source
                .as_deref()
                .is_some_and(|source| source.starts_with("job-stage:"))
        }));
    }

    #[tokio::test]
    async fn child_snapshot_pins_only_current_parent_tools_and_effective_grants() {
        use crate::grants::{Allowlist, ShellGrant};
        use crate::test_support::{fresh_session, tool_context};
        use crate::tool::{ScopedTools, ToolPolicyRegistry, ToolRegistry};
        use std::sync::Arc;

        let (_instance, session) = fresh_session().await;
        let registry = Arc::new(ToolRegistry::new());
        registry.register(crate::tools::Calculate);
        registry.register(crate::tools::GetTime);
        let mut ctx = tool_context(session, registry.clone());
        ctx.tools = ScopedTools::new(registry, Some(vec!["calculate".into()]));
        let bound = Grants {
            shell: Some(ShellGrant {
                allow: Allowlist::Only(vec!["cat".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.agent_grants.insert("calculate".into(), bound.clone());
        let snapshot = ctx.delegable_tool_ceilings(&ToolPolicyRegistry::empty());
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.get("calculate"), Some(&bound));
        assert!(!snapshot.contains_key("get_time"));
    }
    #[tokio::test]
    async fn acceptance_commits_one_directive_and_reuses_the_handle() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let staged = registry
            .stage_agent_job(&pid, "stable-command", definition())
            .await
            .unwrap();
        let accepted = registry
            .accept_staged_agent_job(&staged, &pid, "stable-command", &definition(), "caller")
            .await
            .unwrap();
        let again = registry
            .accept_staged_agent_job(&staged, &pid, "stable-command", &definition(), "caller")
            .await
            .unwrap();
        assert_eq!(accepted, again);
        assert_eq!(
            registry
                .lookup_job_by_key(&pid, "stable-command")
                .await
                .unwrap(),
            Some(staged.clone())
        );
        let (id, db) = registry.open_session(&staged.session_db_id).await.unwrap();
        assert_eq!(
            read_accepted_job(&db).await.unwrap(),
            Some(accepted.clone())
        );
        let session = Session::new(id, db).await;
        assert_eq!(session.entries().len(), 1);
        let requests = session
            .turn_requests(|_| false, &Default::default())
            .await
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].id.as_str(), accepted.directive_id);
        assert_eq!(requests[0].state, super::super::TurnRequestState::Queued);
    }

    #[tokio::test]
    async fn forged_child_entry_blocks_later_acceptance() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let staged = registry
            .stage_agent_job(&pid, "spoofed", definition())
            .await
            .unwrap();
        let (id, child) = registry.open_session(&staged.session_db_id).await.unwrap();
        let mut session = Session::new(id, child.clone()).await;
        session
            .add_entry(super::super::SessionEntry {
                sender: "client".into(),
                content: "untrusted".into(),
                timestamp: chrono::Utc::now(),
                entry_type: super::super::EntryType::Directive,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        assert!(
            registry
                .accept_staged_agent_job(&staged, &pid, "spoofed", &definition(), "caller")
                .await
                .unwrap_err()
                .to_string()
                .contains("entries before admission")
        );
        assert!(read_accepted_job(&child).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn parent_submit_command_retries_by_key_without_requiring_original_timestamp() {
        let (_, registry) = make_registry().await;
        let (id, db) = registry.create_session(Some("cli")).await.unwrap();
        let session = Session::new(id, db).await;
        let request_id = super::super::TurnRequestId::parse("stable-job-key");
        let original = super::super::SessionCommandRequest {
            command_id: request_id.clone(),
            sender: "client".into(),
            created_at: chrono::Utc::now(),
            command: super::super::SessionCommand::SubmitAgent {
                agent_ref: "researcher".into(),
                task: "check it".into(),
            },
        };
        session.submit_command(original.clone()).await.unwrap();
        let mut retry = original.clone();
        retry.created_at += chrono::Duration::seconds(3);
        session.submit_command(retry.clone()).await.unwrap();
        assert_eq!(
            session
                .command_requests(&Default::default())
                .await
                .unwrap()
                .len(),
            1
        );
        retry.command = super::super::SessionCommand::SubmitAgent {
            agent_ref: "researcher".into(),
            task: "different task".into(),
        };
        assert!(
            session
                .submit_command(retry)
                .await
                .unwrap_err()
                .to_string()
                .contains("different payload")
        );
    }
}

// A session-DB last-writer-wins claim is deliberately not a fence. Claim
// collisions and delayed observations may overlap model/tool side effects.
const JOB_OWNER: &str = "job_owner";
const JOB_STOPS: &str = "job_stops";
const CLAIM_TTL: chrono::Duration = chrono::Duration::seconds(4);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JobOwner {
    pub peer: String,
    pub agent_db_id: String,
    pub incarnation: String,
    pub refreshed_at: chrono::DateTime<chrono::Utc>,
}

impl JobOwner {
    pub fn is_live(&self) -> bool {
        self.refreshed_at > chrono::Utc::now() - CLAIM_TTL
    }
}

pub(crate) async fn read_job_owner(db: &eidetica::Database) -> anyhow::Result<Option<JobOwner>> {
    let txn = db.new_transaction().await?;
    let doc = txn
        .get_store::<DocStore>(JOB_OWNER)
        .await?
        .get_all()
        .await?;
    doc.get("v1")
        .map(|value| -> anyhow::Result<JobOwner> {
            let json: String = value.try_into()?;
            Ok(serde_json::from_str(&json)?)
        })
        .transpose()
}

/// Caller checks role and hosted Agent/home identity before writing.
pub(crate) async fn claim_job(
    db: &eidetica::Database,
    peer: &str,
    agent_db_id: &str,
    incarnation: &str,
) -> anyhow::Result<bool> {
    let current = read_job_owner(db).await?;
    if current
        .as_ref()
        .is_some_and(|owner| owner.is_live() && owner.incarnation != incarnation)
    {
        return Ok(false);
    }
    let owner = JobOwner {
        peer: peer.to_string(),
        agent_db_id: agent_db_id.to_string(),
        incarnation: incarnation.to_string(),
        refreshed_at: chrono::Utc::now(),
    };
    let txn = db.new_transaction().await?;
    txn.get_store::<DocStore>(JOB_OWNER)
        .await?
        .set_string("v1", serde_json::to_string(&owner)?)
        .await?;
    txn.commit().await?;
    owns_job(db, incarnation).await
}

/// Stable marker key: retrying after an uncertain write doesn't add another.
pub(crate) async fn record_job_stop(
    db: &eidetica::Database,
    attempt_id: &str,
    loser: &str,
    winner: &JobOwner,
) -> anyhow::Result<()> {
    let key = format!("{loser}:{attempt_id}");
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(JOB_STOPS).await?;
    if store.get_all().await?.get(&key).is_none() {
        store
            .set_string(
                &key,
                serde_json::to_string(&serde_json::json!({
                    "attempt_id": attempt_id, "loser": loser, "winner": winner.incarnation,
                    "observed_at": chrono::Utc::now(),
                }))?,
            )
            .await?;
        txn.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn job_stop_count(db: &eidetica::Database) -> anyhow::Result<usize> {
    let txn = db.new_transaction().await?;
    Ok(txn
        .get_store::<DocStore>(JOB_STOPS)
        .await?
        .get_all()
        .await?
        .iter()
        .count())
}

pub(crate) async fn owns_job(db: &eidetica::Database, incarnation: &str) -> anyhow::Result<bool> {
    Ok(read_job_owner(db)
        .await?
        .is_some_and(|owner| owner.incarnation == incarnation && owner.is_live()))
}

/// Watch writes, with a bounded poll in case the callback misses one.
pub(crate) async fn wait_for_claim_loss(
    db: &eidetica::Database,
    incarnation: &str,
) -> anyhow::Result<JobOwner> {
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let signal = notify.clone();
    let tips = db.snapshot().await?;
    let _callback = db
        .on_write_at_tips(tips, move |_, _| {
            let signal = signal.clone();
            async move {
                signal.notify_one();
                Ok(())
            }
        })
        .await?;
    loop {
        if let Some(owner) = read_job_owner(db).await?
            && owner.incarnation != incarnation
        {
            return Ok(owner);
        }
        tokio::select! {
            _ = notify.notified() => {},
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {},
        }
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::session::test_helpers::make_registry;

    #[tokio::test]
    async fn live_claim_refuses_rival_and_expired_claim_can_be_replaced() {
        let (_, registry) = make_registry().await;
        let (_, db) = registry.create_session(Some("claim-test")).await.unwrap();
        assert!(claim_job(&db, "peer", "agent", "process-a").await.unwrap());
        assert!(!claim_job(&db, "peer", "agent", "process-b").await.unwrap());
        assert_eq!(
            read_job_owner(&db).await.unwrap().unwrap().incarnation,
            "process-a"
        );
        let txn = db.new_transaction().await.unwrap();
        let stale = JobOwner {
            peer: "peer".into(),
            agent_db_id: "agent".into(),
            incarnation: "process-a".into(),
            refreshed_at: chrono::Utc::now() - CLAIM_TTL - chrono::Duration::seconds(1),
        };
        txn.get_store::<DocStore>(JOB_OWNER)
            .await
            .unwrap()
            .set_string("v1", serde_json::to_string(&stale).unwrap())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert!(claim_job(&db, "peer", "agent", "process-b").await.unwrap());
        assert!(!owns_job(&db, "process-a").await.unwrap());
        let winner = read_job_owner(&db).await.unwrap().unwrap();
        record_job_stop(&db, "attempt-1", "process-a", &winner)
            .await
            .unwrap();
        record_job_stop(&db, "attempt-1", "process-a", &winner)
            .await
            .unwrap();
        assert_eq!(job_stop_count(&db).await.unwrap(), 1);
    }
}
