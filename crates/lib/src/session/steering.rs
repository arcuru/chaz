//! Next-call input for an already-started Agent job, never an ordinary turn.
//! Request identity is caller-owned; attempt identity prevents restart/retry replay.

use super::{Session, TurnRequestId, TurnRequestState};
use eidetica::{Database, store::Table};
use serde::{Deserialize, Serialize};

const INPUTS: &str = "job_inputs";
const RECEIPTS: &str = "job_input_receipts";
const CLOSED: &str = "job_input_closed";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobInputRequest {
    pub id: String,
    pub attempt_id: String,
    pub text: String,
}

impl JobInputRequest {
    pub fn new(attempt_id: String, text: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            attempt_id,
            text,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobInputState {
    Queued,
    Accepted,
    /// Persisted dispatch intent, not proof the backend received the request.
    Dispatching {
        model_sequence: u64,
    },
    /// A response to the model request containing this input was observed.
    Included {
        model_sequence: u64,
    },
    NotApplied {
        reason: String,
    },
    Uncertain {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInput {
    pub request: JobInputRequest,
    pub state: JobInputState,
}

impl Session {
    /// Write using this DB handle's existing authenticated authority.
    /// Possession of the job ID or projection is no grant.
    pub async fn submit_job_input(&self, request: JobInputRequest) -> anyhow::Result<JobInput> {
        super::jobs::require_observer_write(&self.database).await?;
        anyhow::ensure!(
            uuid::Uuid::parse_str(&request.id).is_ok() && !request.text.trim().is_empty(),
            "input requires a stable UUID and nonempty text"
        );
        let accepted = super::jobs::read_accepted_job(&self.database)
            .await?
            .ok_or_else(|| anyhow::anyhow!("job has not been accepted"))?;
        let txn = self.database.new_transaction().await?;
        let inputs = txn.get_store::<Table<JobInputRequest>>(INPUTS).await?;
        if let Some((_, existing)) = inputs
            .search(|r: &JobInputRequest| r.id == request.id)
            .await?
            .into_iter()
            .next()
        {
            anyhow::ensure!(
                existing == request,
                "input ID reused with different payload"
            );
            return self
                .job_inputs()
                .await?
                .into_iter()
                .find(|r| r.request.id == request.id)
                .ok_or_else(|| anyhow::anyhow!("input read outcome uncertain"));
        }
        anyhow::ensure!(
            super::jobs::read_job_result(&self.database)
                .await?
                .is_none(),
            "finished jobs are view-only"
        );
        let state = self
            .turn_state_for_entry(
                &TurnRequestId::parse(accepted.directive_id),
                &Default::default(),
            )
            .await?;
        anyhow::ensure!(
            matches!(state, TurnRequestState::Interrupted { ref attempt_id } if attempt_id == &request.attempt_id),
            "input must name the current started job attempt"
        );
        anyhow::ensure!(
            !txn.get_store::<Table<bool>>(CLOSED)
                .await?
                .search(|_: &bool| true)
                .await?
                .iter()
                .any(|(id, closed)| id == &request.attempt_id && *closed),
            "executor closed input acceptance"
        );
        inputs.set(&request.id, request.clone()).await?;
        txn.commit().await?;
        Ok(JobInput {
            request,
            state: JobInputState::Queued,
        })
    }

    /// Receipt plus canonical attempt/result evidence. Missing receipt never
    /// implies inclusion. In particular a restart does not consume old input.
    pub async fn job_inputs(&self) -> anyhow::Result<Vec<JobInput>> {
        super::jobs::require_observer_write(&self.database).await?;
        let txn = self.database.new_transaction().await?;
        let requests = txn
            .get_store::<Table<JobInputRequest>>(INPUTS)
            .await?
            .search(|_: &JobInputRequest| true)
            .await?;
        let receipts = txn
            .get_store::<Table<JobInputState>>(RECEIPTS)
            .await?
            .search(|_: &JobInputState| true)
            .await?;
        let closed = txn
            .get_store::<Table<bool>>(CLOSED)
            .await?
            .search(|_: &bool| true)
            .await?;
        let status =
            super::jobs::status_from_db(&self.database, &Default::default(), false).await?;
        let current = match &status.state {
            super::jobs::JobState::Running { attempt_id }
            | super::jobs::JobState::StartedUnknown {
                attempt_id,
                activity_recent: true,
            } => Some(attempt_id.as_str()),
            _ => None,
        };
        let mut inputs = Vec::new();
        for (_, request) in requests {
            let mut state = receipts
                .iter()
                .find(|(id, _)| id == &request.id)
                .map(|(_, state)| state.clone())
                .unwrap_or(JobInputState::Queued);
            if matches!(
                state,
                JobInputState::Queued | JobInputState::Accepted | JobInputState::Dispatching { .. }
            ) {
                let is_closed = closed
                    .iter()
                    .any(|(id, closed)| id == &request.attempt_id && *closed);
                state = if matches!(state, JobInputState::Queued) && is_closed {
                    JobInputState::NotApplied {
                        reason: "not accepted before executor closed input".into(),
                    }
                } else if current != Some(request.attempt_id.as_str()) {
                    if status.state.is_terminal()
                        && !matches!(state, JobInputState::Dispatching { .. })
                    {
                        JobInputState::NotApplied {
                            reason: "job ended before input reached a model call".into(),
                        }
                    } else {
                        JobInputState::Uncertain {
                            reason: "attempt is not currently observed; no automatic replay".into(),
                        }
                    }
                } else {
                    state
                };
            }
            inputs.push(JobInput { request, state });
        }
        inputs.sort_by(|a, b| a.request.id.cmp(&b.request.id));
        Ok(inputs)
    }
}

/// Executor-only call boundary, after the entire native tool exchange. The
/// close marker distinguishes concurrent late publication from acceptance;
/// it is best-effort local serialization, not distributed fencing.
pub(crate) async fn input_boundary(
    db: &Database,
    attempt_id: &str,
    model_sequence: u64,
    closing: bool,
) -> anyhow::Result<Vec<String>> {
    let txn = db.new_transaction().await?;
    let inputs = txn.get_store::<Table<JobInputRequest>>(INPUTS).await?;
    let receipts = txn.get_store::<Table<JobInputState>>(RECEIPTS).await?;
    let previous = receipts.search(|_: &JobInputState| true).await?;
    let closed = txn.get_store::<Table<bool>>(CLOSED).await?;
    if closed
        .search(|_: &bool| true)
        .await?
        .iter()
        .any(|(id, v)| id == attempt_id && *v)
    {
        return Ok(Vec::new());
    }
    let mut pending = inputs
        .search(|r: &JobInputRequest| r.attempt_id == attempt_id)
        .await?;
    pending.sort_by(|a, b| a.0.cmp(&b.0));
    let mut messages = Vec::new();
    for (_, request) in pending {
        if previous.iter().any(|(id, _)| id == &request.id) {
            continue;
        }
        receipts.set(&request.id, JobInputState::Accepted).await?;
        messages.push((request.id, request.text));
    }
    if messages.is_empty() && closing {
        closed.set(attempt_id, true).await?;
    }
    // Acceptance and dispatch are separate durable facts; losing the second
    // write leaves accepted-but-not-applied evidence, never a claimed call.
    txn.commit().await?;
    if !messages.is_empty() {
        let txn = db.new_transaction().await?;
        let receipts = txn.get_store::<Table<JobInputState>>(RECEIPTS).await?;
        for (id, _) in &messages {
            receipts
                .set(id, JobInputState::Dispatching { model_sequence })
                .await?;
        }
        txn.commit().await?;
    }
    Ok(messages.into_iter().map(|(_, text)| text).collect())
}

pub(crate) async fn input_call_observed(
    db: &Database,
    attempt_id: &str,
    model_sequence: u64,
) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let receipts = txn.get_store::<Table<JobInputState>>(RECEIPTS).await?;
    let rows = receipts.search(|state: &JobInputState| matches!(state, JobInputState::Dispatching { model_sequence: seq } if *seq == model_sequence)).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let inputs = txn
        .get_store::<Table<JobInputRequest>>(INPUTS)
        .await?
        .search(|r: &JobInputRequest| r.attempt_id == attempt_id)
        .await?;
    for (id, _) in rows {
        if inputs.iter().any(|(request_id, _)| request_id == &id) {
            receipts
                .set(id, JobInputState::Included { model_sequence })
                .await?;
        }
    }
    txn.commit().await?;
    Ok(())
}
