//! Durable Agent job state and attempt/claim tracking.

use super::SessionRegistry;
use crate::grants::Grants;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
pub(crate) struct AcceptedAgentJob {
    pub parent_id: String,
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

/// Prevent ordinary runtime registration for any job-marked DB, including
/// obsolete staged rows that have no submitter-owned acceptance path.
pub async fn is_job_session(db: &eidetica::Database) -> bool {
    crate::db_kind::read_marker(db)
        .await
        .is_some_and(|(kind, name)| {
            kind == crate::db_kind::KIND_SESSION && name.starts_with("job-stage:")
        })
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Pending,
    Rejected {
        message: String,
    },
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
        matches!(
            self,
            Self::Succeeded { .. } | Self::Failed { .. } | Self::Rejected { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobStatus {
    pub session_db_id: String,
    pub state: JobState,
}

const JOB_REJECTION: &str = "job_rejection";

pub(crate) async fn read_job_rejection(db: &eidetica::Database) -> anyhow::Result<Option<String>> {
    let txn = db.new_transaction().await?;
    let doc = txn
        .get_store::<DocStore>(JOB_REJECTION)
        .await?
        .get_all()
        .await?;
    doc.get("v1")
        .map(|v| v.try_into().map_err(Into::into))
        .transpose()
}

pub(crate) async fn reject_job(db: &eidetica::Database, message: &str) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    txn.get_store::<DocStore>(JOB_REJECTION)
        .await?
        .set_string("v1", message)
        .await?;
    txn.commit().await?;
    Ok(())
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
    let accepted = match read_accepted_job(db).await? {
        Some(accepted) => accepted,
        None if crate::db_kind::read_marker(db)
            .await
            .is_some_and(|(kind, name)| {
                kind == crate::db_kind::KIND_SESSION && name.starts_with("job-stage:submitter:")
            })
            || read_job_rejection(db).await?.is_some() =>
        {
            return Ok(JobStatus {
                session_db_id: db.root_id().to_string(),
                state: match read_job_rejection(db).await? {
                    Some(message) => JobState::Rejected { message },
                    None => JobState::Pending,
                },
            });
        }
        None => anyhow::bail!("session has no accepted agent job"),
    };
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
/// Local-v1 observation needs write-capable credentials because the current
/// service materializes native record caches while reading. This checks existing
/// authority only; it neither grants permission nor acquires execution.
pub async fn require_observer_write(
    db: &eidetica::Database,
) -> anyhow::Result<eidetica::auth::types::Permission> {
    // The pinned native resolver needs a local backend for delegated handles.
    // Do not panic, guess authority, or substitute another key on service reads.
    anyhow::ensure!(
        !(db.instance()?.remote_connection().is_some()
            && matches!(
                db.auth_identity(),
                Some(eidetica::auth::SigKey::Delegation { .. })
            )),
        "unavailable: cannot verify existing Write authority for this delegated service session identity; native permission query is unsupported"
    );
    let permission = db.current_permission().await?;
    anyhow::ensure!(
        permission.can_write(),
        "insufficient permission: local-v1 job observation requires existing Write authority (Admin allowed); {permission:?} is unsupported"
    );
    Ok(permission)
}

/// Client evidence without a local live-attempt set: never promises liveness.
pub async fn observer_status(db: &eidetica::Database) -> anyhow::Result<JobStatus> {
    require_observer_write(db).await?;
    status_from_db(db, &Default::default(), false).await
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
                .is_some_and(|s| s.starts_with("job-stage:submitter:")),
            "session is not a locally published job"
        );
        let root = eidetica::entry::ID::parse(session_db_id)?;
        let mut user = self.user.lock().await;
        let key = user.get_default_key()?;
        user.map_key(&key, &root, eidetica::auth::SigKey::from_pubkey(&key))
            .await?;
        let db = user.open_database_with_key(&root, &key).await.map_err(|error| {
            anyhow::anyhow!("job unavailable or insufficient permission: local-v1 observation requires existing Write authority (Admin allowed): {error}")
        })?;
        require_observer_write(&db).await?;
        self.local_client_sessions
            .lock()
            .await
            .insert(session_db_id.to_string(), db.clone());
        Ok((super::ConversationId(session_db_id.to_string()), db))
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

/// An actual entered job_wait, not a model's proposed dependency. A persisted
/// unfinished row is recorded activity, never proof a process is still waiting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobWait {
    pub attempt_id: String,
    pub child_id: String,
    pub deadline_at: chrono::DateTime<chrono::Utc>,
    pub finished: bool,
}

const JOB_WAITS: &str = "job_waits";

pub async fn job_waits(db: &eidetica::Database) -> anyhow::Result<Vec<JobWait>> {
    let txn = db.new_transaction().await?;
    Ok(txn
        .get_store::<eidetica::store::Table<JobWait>>(JOB_WAITS)
        .await?
        .search(|_: &JobWait| true)
        .await?
        .into_iter()
        .map(|(_, row)| row)
        .collect())
}

impl super::Session {
    pub(crate) async fn start_job_wait(
        &self,
        request_id: &super::TurnRequestId,
        tool_call_key: &str,
        child_id: &str,
        seconds: u64,
    ) -> anyhow::Result<Option<(String, JobWait)>> {
        let Some(accepted) = read_accepted_job(self.database()).await? else {
            return Ok(None);
        };
        anyhow::ensure!(
            accepted.directive_id == request_id.as_str(),
            "wait must belong to original job Directive"
        );
        let attempt_id = match self
            .turn_state_for_entry(request_id, &Default::default())
            .await?
        {
            super::TurnRequestState::Interrupted { attempt_id } => attempt_id,
            _ => anyhow::bail!("job wait has no started attempt"),
        };
        let key = format!("{attempt_id}:{tool_call_key}");
        let row = JobWait {
            attempt_id,
            child_id: child_id.into(),
            deadline_at: chrono::Utc::now() + chrono::Duration::seconds(seconds as i64),
            finished: false,
        };
        write_job_wait(self.database(), &key, row.clone()).await?;
        Ok(Some((key, row)))
    }
}

pub(crate) async fn write_job_wait(
    db: &eidetica::Database,
    key: &str,
    row: JobWait,
) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    txn.get_store::<eidetica::store::Table<JobWait>>(JOB_WAITS)
        .await?
        .set(key, row)
        .await?;
    txn.commit().await?;
    Ok(())
}
