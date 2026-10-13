//! Next-call input for an already-started Chat or Agent job, never a new turn.
//! Request identity is caller-owned; attempt identity prevents restart/retry replay.

use super::Session;
use eidetica::{Database, store::Table};
use serde::{Deserialize, Serialize};

const INPUTS: &str = "job_inputs";
const RECEIPTS: &str = "job_input_receipts";
const CLOSED: &str = "job_input_closed";
const CALLS: &str = "job_input_calls";

/// A validation refusal before any publication commit was attempted. The
/// composer can recover its text without retaining an uncertain request ID.
#[derive(Debug)]
pub struct InputRefused(pub &'static str);
impl std::fmt::Display for InputRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for InputRefused {}

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
    /// Structured host dispatch boundary exists; independent of response evidence.
    #[serde(default)]
    pub incorporated: bool,
}

impl Session {
    /// Only the latest generation of each request can receive steering. A fresh
    /// heartbeat from a superseded attempt cannot authorize replay into it.
    pub async fn steering_attempts(db: &Database) -> anyhow::Result<Vec<String>> {
        Self::steering_attempts_at(db, &db.snapshot().await?).await
    }

    pub async fn steering_attempts_at(
        db: &Database,
        snapshot: &eidetica::Snapshot,
    ) -> anyhow::Result<Vec<String>> {
        let txn = db.new_transaction_at(snapshot).await?;
        Ok(recent_states(&txn)
            .await?
            .into_values()
            .filter_map(|state| match state {
                super::TurnRequestState::InFlight { attempt_id } => Some(attempt_id),
                _ => None,
            })
            .collect())
    }

    /// Write using this DB handle's existing authenticated authority.
    /// Possession of the job ID or projection is no grant.
    pub async fn submit_job_input(&self, request: JobInputRequest) -> anyhow::Result<JobInput> {
        super::jobs::require_observer_write(&self.database).await?;
        anyhow::ensure!(
            super::jobs::read_accepted_job(&self.database)
                .await?
                .is_some(),
            InputRefused("job has not been accepted")
        );
        self.submit_turn_input(request).await
    }

    /// Shared authenticated steering publication. Ordinary idle Chat must create
    /// a Message instead; this API never changes attempts or starts work.
    pub async fn submit_turn_input(&self, request: JobInputRequest) -> anyhow::Result<JobInput> {
        super::jobs::require_observer_write(&self.database).await?;
        anyhow::ensure!(
            uuid::Uuid::parse_str(&request.id).is_ok() && !request.text.trim().is_empty(),
            InputRefused("input requires a stable UUID and nonempty text")
        );
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
                InputRefused("input ID reused with different payload")
            );
            return self
                .turn_inputs()
                .await?
                .into_iter()
                .find(|r| r.request.id == request.id)
                .ok_or_else(|| anyhow::anyhow!("input read outcome uncertain"));
        }
        anyhow::ensure!(
            super::jobs::read_job_result(&self.database)
                .await?
                .is_none(),
            InputRefused("finished jobs are view-only")
        );
        if let Some(accepted) = super::jobs::read_accepted_job(&self.database).await? {
            let attempt = self
                .read_turn_attempt(&request.attempt_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!(InputRefused("job attempt missing")))?;
            anyhow::ensure!(
                attempt.request_id.as_str() == accepted.directive_id,
                InputRefused("input must name the accepted job's attempt")
            );
        }
        anyhow::ensure!(
            Session::steering_attempts(&self.database)
                .await?
                .contains(&request.attempt_id),
            InputRefused("input must name a recent started attempt")
        );
        anyhow::ensure!(
            !txn.get_store::<Table<bool>>(CLOSED)
                .await?
                .search(|_: &bool| true)
                .await?
                .iter()
                .any(|(id, closed)| id == &request.attempt_id && *closed),
            InputRefused("executor closed input acceptance")
        );
        inputs.set(&request.id, request.clone()).await?;
        txn.commit().await?;
        Ok(JobInput {
            request,
            state: JobInputState::Queued,
            incorporated: false,
        })
    }

    /// Receipt plus canonical attempt/result evidence. Missing receipt never
    /// implies inclusion. In particular a restart does not consume old input.
    pub async fn job_inputs(&self) -> anyhow::Result<Vec<JobInput>> {
        self.turn_inputs().await
    }

    /// Receipt reads for both ordinary conversations and job observers.
    pub async fn turn_inputs(&self) -> anyhow::Result<Vec<JobInput>> {
        self.turn_inputs_at(&self.database.snapshot().await?).await
    }

    /// Coherent view for rendering: a receipt cannot hide text whose promoted
    /// transcript rows are absent from the same snapshot.
    pub async fn turn_inputs_at(
        &self,
        snapshot: &eidetica::Snapshot,
    ) -> anyhow::Result<Vec<JobInput>> {
        super::jobs::require_observer_write(&self.database).await?;
        let requests = ordered_inputs_at(&self.database, snapshot).await?;
        let txn = self.database.new_transaction_at(snapshot).await?;
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
        let current: Vec<_> = recent_states(&txn)
            .await?
            .into_values()
            .filter_map(|state| match state {
                super::TurnRequestState::InFlight { attempt_id } => Some(attempt_id),
                _ => None,
            })
            .collect();
        let attempts = txn
            .get_store::<Table<super::TurnAttempt>>(super::TURN_ATTEMPTS_STORE)
            .await?
            .search(|_| true)
            .await?;
        let states = super::index_attempt_states(attempts.clone(), &Default::default());
        let completed: Vec<_> = attempts
            .into_iter()
            .filter(|(_, attempt)| {
                matches!(
                    states.get(&attempt.request_id),
                    Some(super::TurnRequestState::Completed { .. })
                )
            })
            .map(|(_, attempt)| attempt.attempt_id)
            .collect();
        let boundaries = txn
            .get_store::<Table<super::TurnTranscriptRecord>>(super::TURN_TRANSCRIPT_STORE)
            .await?
            .search(|r| matches!(r.message, super::TurnTranscriptMessage::Steering { .. }))
            .await?;
        let mut inputs = Vec::new();
        for request in requests {
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
                } else if !current.contains(&request.attempt_id) {
                    if completed.contains(&request.attempt_id)
                        && !matches!(state, JobInputState::Dispatching { .. })
                    {
                        JobInputState::NotApplied {
                            reason: "attempt ended before input reached a model call".into(),
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
            let incorporated = boundaries.iter().any(|(_, r)| r.attempt_id == request.attempt_id
                && matches!(&r.message, super::TurnTranscriptMessage::Steering { inputs, .. } if inputs.iter().any(|i| i.id == request.id)));
            inputs.push(JobInput {
                request,
                state,
                incorporated,
            });
        }
        Ok(inputs)
    }
}

/// Native causal publication order, not random row keys or publisher clocks.
/// Concurrent incomparable commits use Eidetica's canonical DAG order.
async fn ordered_inputs(db: &Database) -> anyhow::Result<Vec<JobInputRequest>> {
    ordered_inputs_at(db, &db.snapshot().await?).await
}

async fn recent_states(
    txn: &eidetica::Transaction,
) -> anyhow::Result<std::collections::HashMap<super::TurnRequestId, super::TurnRequestState>> {
    let attempts = txn
        .get_store::<Table<super::TurnAttempt>>(super::TURN_ATTEMPTS_STORE)
        .await?
        .search(|_| true)
        .await?;
    let heartbeats = txn
        .get_store::<Table<chrono::DateTime<chrono::Utc>>>(super::TURN_ACTIVITY_STORE)
        .await?
        .search(|_| true)
        .await?
        .into_iter()
        .collect();
    let active: std::collections::HashSet<_> = super::visible_attempts(
        attempts.iter().map(|(_, a)| a.clone()).collect(),
        &heartbeats,
        chrono::Utc::now(),
    )
    .into_iter()
    .collect();
    // Select lifecycle precedence before considering activity. Otherwise the
    // index's live-first choice can revive an older generation's heartbeat.
    Ok(super::index_attempt_states(attempts, &Default::default())
        .into_iter()
        .map(|(request, state)| {
            let state = match state {
                super::TurnRequestState::Interrupted { attempt_id }
                    if active.contains(&attempt_id) =>
                {
                    super::TurnRequestState::InFlight { attempt_id }
                }
                state => state,
            };
            (request, state)
        })
        .collect())
}

async fn ordered_inputs_at(
    db: &Database,
    snapshot: &eidetica::Snapshot,
) -> anyhow::Result<Vec<JobInputRequest>> {
    let entries = super::execution::store_entries_at(db, INPUTS, snapshot).await?;
    let mut seen = std::collections::HashSet::new();
    let mut ordered = Vec::new();
    for entry in entries {
        let txn = db
            .new_transaction_at(&eidetica::Snapshot::from(vec![entry.id()]))
            .await?;
        let added: Vec<_> = txn
            .get_store::<Table<JobInputRequest>>(INPUTS)
            .await?
            .search(|_: &JobInputRequest| true)
            .await?
            .into_iter()
            .filter(|(id, _)| !seen.contains(id))
            .collect();
        anyhow::ensure!(
            added.len() <= 1,
            "ambiguous input publication; one input per commit required"
        );
        for (id, request) in added {
            anyhow::ensure!(id == request.id, "input identity mismatch");
            seen.insert(id);
            ordered.push(request);
        }
    }
    Ok(ordered)
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
    let pending = ordered_inputs(db).await?;
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
    let calls = txn.get_store::<Table<u64>>(CALLS).await?;
    let mut messages = Vec::new();
    for request in pending.into_iter().filter(|r| r.attempt_id == attempt_id) {
        if previous.iter().any(|(id, _)| id == &request.id) {
            continue;
        }
        receipts.set(&request.id, JobInputState::Accepted).await?;
        calls.set(&request.id, model_sequence).await?;
        messages.push((request.id, request.text));
    }
    if messages.is_empty() && closing {
        closed.set(attempt_id, true).await?;
    }
    // Selection is not incorporation. Required projection and budgets still
    // run before the recorder promotes these inputs at dispatch.
    txn.commit().await?;
    Ok(messages.into_iter().map(|(_, text)| text).collect())
}

pub(crate) async fn dispatch_inputs(
    db: &Database,
    attempt_id: &str,
    model_sequence: u64,
    messages: &[crate::runtime::RuntimeMessage],
    baseline: &[crate::runtime::RuntimeMessage],
) -> anyhow::Result<Vec<JobInputRequest>> {
    let ordered = ordered_inputs(db).await?;
    let txn = db.new_transaction().await?;
    let calls = txn
        .get_store::<Table<u64>>(CALLS)
        .await?
        .search(|seq| *seq == model_sequence)
        .await?;
    let accepted = txn
        .get_store::<Table<JobInputState>>(RECEIPTS)
        .await?
        .search(|s| *s == JobInputState::Accepted)
        .await?;
    let selected: Vec<_> = ordered
        .into_iter()
        .filter(|r| {
            r.attempt_id == attempt_id
                && calls.iter().any(|(id, _)| id == &r.id)
                && accepted.iter().any(|(id, _)| id == &r.id)
        })
        .collect();
    // The projector must not erase/reorder selected steering then claim it was
    // incorporated. Refuse before dispatch; the original text remains pending.
    if selected.is_empty() {
        return Ok(selected);
    }
    let is_selected = |message: &crate::runtime::RuntimeMessage| {
        selected.iter().any(
            |input| matches!(message, crate::runtime::RuntimeMessage::User(t) if t == &input.text),
        )
    };
    // Use the last matching occurrences, not an older identical message that
    // could hide reordering. Projectors may still append distinct context.
    let end = messages
        .iter()
        .rposition(is_selected)
        .ok_or_else(|| anyhow::anyhow!("projection removed steering input"))?
        + 1;
    let start = end
        .checked_sub(selected.len())
        .ok_or_else(|| anyhow::anyhow!("projection removed steering input"))?;
    anyhow::ensure!(
        selected
            .iter()
            .zip(&messages[start..end])
            .all(|(input, message)| {
                matches!(message, crate::runtime::RuntimeMessage::User(t) if t == &input.text)
            })
            && !messages[start..].iter().any(|message| matches!(
                message,
                crate::runtime::RuntimeMessage::Assistant(_)
                    | crate::runtime::RuntimeMessage::AssistantWithExtra { .. }
                    | crate::runtime::RuntimeMessage::AssistantToolCalls { .. }
                    | crate::runtime::RuntimeMessage::ToolResult { .. }
            )),
        "projection removed or reordered steering boundary"
    );
    for input in &selected {
        let occurrences = |messages: &[crate::runtime::RuntimeMessage]| {
            messages
                .iter()
                .filter(
                    |m| matches!(m, crate::runtime::RuntimeMessage::User(t) if t == &input.text),
                )
                .count()
        };
        // Text-only provider vectors cannot distinguish identical occurrences;
        // refuse an ambiguous deletion rather than promote an older match.
        anyhow::ensure!(
            occurrences(messages) >= occurrences(baseline),
            "projection ambiguously removed steering input"
        );
    }
    Ok(selected)
}

pub(crate) async fn dispatch_in_txn(
    txn: &eidetica::Transaction,
    sequence: u64,
    inputs: &[JobInputRequest],
) -> anyhow::Result<()> {
    let receipts = txn.get_store::<Table<JobInputState>>(RECEIPTS).await?;
    for input in inputs {
        receipts
            .set(
                &input.id,
                JobInputState::Dispatching {
                    model_sequence: sequence,
                },
            )
            .await?;
    }
    Ok(())
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

/// Known host boundaries must be unambiguous. Unknown transcript variants are
/// refused by the enclosing readers, never partially replayed.
pub(crate) fn validate_boundary(
    inputs: &[JobInputRequest],
    attempt: &str,
    seen: &mut std::collections::HashSet<String>,
) -> bool {
    !inputs.is_empty()
        && inputs.iter().all(|input| {
            input.attempt_id == attempt
                && uuid::Uuid::parse_str(&input.id).is_ok()
                && !input.text.trim().is_empty()
                && seen.insert(input.id.clone())
        })
}
