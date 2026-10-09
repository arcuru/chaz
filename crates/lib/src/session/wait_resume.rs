//! Restart continuation for one checkpointed `job_wait`.
//!
//! Before a turn observes an Agent job, the runtime records exactly which
//! call is waiting on which child, and until when. After an executor restart
//! that attempt is otherwise `Interrupted` and held for explicit retry. Only
//! when its newest checkpoint is the pending end of a strictly paired
//! transcript may the executor resume it: the same attempt observes the same
//! child until the original absolute deadline, and no earlier model response
//! or tool call runs again. Every other interrupted attempt stays held.

use super::{Session, TurnRequestId, TurnTranscriptMessage, TurnTranscriptRecord};
use crate::runtime::{JobWaitIntent, RuntimeMessage, RuntimeResume, ToolResultOutcome};
use chrono::{DateTime, Utc};
use eidetica::Snapshot;
use eidetica::store::Table;
use serde::{Deserialize, Serialize};

pub(crate) const JOB_WAIT_CHECKPOINTS_STORE: &str = "job_wait_checkpoints";
pub(crate) const JOB_WAIT_CHECKPOINT_VERSION: u32 = 1;

/// Durable record of one pending `job_wait` call in a parent session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobWaitCheckpoint {
    /// Readers hold, never resume, a version they do not know.
    pub version: u32,
    pub request_id: TurnRequestId,
    pub attempt_id: String,
    pub agent_name: String,
    pub model_sequence: u64,
    pub call_index: usize,
    pub call_id: String,
    /// The call's arguments after approval and `tool_call` hooks.
    pub arguments: String,
    pub child_session_db_id: String,
    /// Session state the turn's context was assembled from.
    pub context_snapshot: Snapshot,
    pub checkpointed_at: DateTime<Utc>,
    /// Absolute observation deadline; downtime counts against it.
    pub deadline: DateTime<Utc>,
    pub policy_timeout_seconds: u64,
    /// Set durably before result hooks run. The receipt is retained, but an
    /// unfinished result after this boundary is held for explicit retry.
    pub observation_finished: bool,
}

impl JobWaitCheckpoint {
    pub(crate) fn new(
        request_id: TurnRequestId,
        attempt_id: String,
        agent_name: String,
        context_snapshot: Snapshot,
        wait: JobWaitIntent,
    ) -> Self {
        Self {
            version: JOB_WAIT_CHECKPOINT_VERSION,
            request_id,
            attempt_id,
            agent_name,
            model_sequence: wait.model_sequence,
            call_index: wait.call_index,
            call_id: wait.call_id,
            arguments: wait.arguments,
            child_session_db_id: wait.child_session_db_id,
            context_snapshot,
            checkpointed_at: wait.checkpointed_at,
            deadline: wait.deadline,
            policy_timeout_seconds: wait.policy_timeout_seconds,
            observation_finished: false,
        }
    }

    /// Stable row key: rewriting the same call cannot add a second row.
    pub(crate) fn key(&self) -> String {
        format!(
            "{}:{:020}:{:010}",
            self.attempt_id, self.model_sequence, self.call_index
        )
    }

    pub(crate) fn policy_deadline(&self) -> Option<DateTime<Utc>> {
        let seconds = i64::try_from(self.policy_timeout_seconds).ok()?;
        self.checkpointed_at
            .checked_add_signed(chrono::Duration::try_seconds(seconds)?)
    }
}

/// A validated continuation: the checkpoint, the reconstructed runtime
/// state, and the next transcript sequence for the same attempt.
#[derive(Clone, Debug)]
pub struct ResumableWait {
    pub checkpoint: JobWaitCheckpoint,
    pub runtime: RuntimeResume,
    pub next_sequence: u64,
}

impl Session {
    pub(crate) async fn record_job_wait_checkpoint(
        &self,
        checkpoint: &JobWaitCheckpoint,
    ) -> anyhow::Result<()> {
        let txn = self.database().new_transaction().await?;
        txn.get_store::<Table<JobWaitCheckpoint>>(JOB_WAIT_CHECKPOINTS_STORE)
            .await?
            .set(checkpoint.key(), checkpoint.clone())
            .await?;
        txn.commit().await?;
        Ok(())
    }

    pub(crate) async fn finish_job_wait_checkpoint(
        &self,
        attempt_id: &str,
        model_sequence: u64,
        call_index: usize,
    ) -> anyhow::Result<()> {
        let txn = self.database().new_transaction().await?;
        let store = txn
            .get_store::<Table<JobWaitCheckpoint>>(JOB_WAIT_CHECKPOINTS_STORE)
            .await?;
        let rows = store
            .search(|checkpoint| {
                checkpoint.attempt_id == attempt_id
                    && checkpoint.model_sequence == model_sequence
                    && checkpoint.call_index == call_index
                    && !checkpoint.observation_finished
            })
            .await?;
        if !rows.is_empty() {
            for (key, mut checkpoint) in rows {
                checkpoint.observation_finished = true;
                store.set(key, checkpoint).await?;
            }
            txn.commit().await?;
        }
        Ok(())
    }

    /// The newest checkpoint for an attempt. An earlier wait in the same
    /// attempt already has its result, so only the newest can be pending.
    pub(crate) async fn latest_job_wait_checkpoint(
        &self,
        attempt_id: &str,
    ) -> anyhow::Result<Option<JobWaitCheckpoint>> {
        Self::latest_job_wait_checkpoint_in(self.database(), attempt_id).await
    }

    pub(crate) async fn latest_job_wait_checkpoint_in(
        database: &eidetica::Database,
        attempt_id: &str,
    ) -> anyhow::Result<Option<JobWaitCheckpoint>> {
        let txn = database.new_transaction().await?;
        Ok(txn
            .get_store::<Table<JobWaitCheckpoint>>(JOB_WAIT_CHECKPOINTS_STORE)
            .await?
            .search(|checkpoint| checkpoint.attempt_id == attempt_id)
            .await?
            .into_iter()
            .map(|(_, checkpoint)| checkpoint)
            .max_by_key(|checkpoint| (checkpoint.model_sequence, checkpoint.call_index)))
    }

    /// Validate that the checkpoint's attempt has exactly that wait pending
    /// for `request_id`. `Ok(Err(reason))` means the attempt stays held.
    pub(crate) async fn resumable_job_wait(
        &self,
        request_id: &TurnRequestId,
        checkpoint: JobWaitCheckpoint,
    ) -> anyhow::Result<Result<ResumableWait, &'static str>> {
        if &checkpoint.request_id != request_id {
            return Ok(Err("checkpoint belongs to another request"));
        }
        let records = self.turn_transcript(&checkpoint.attempt_id).await?;
        Ok(
            plan_job_wait_resume(&checkpoint, &records).map(|(runtime, next_sequence)| {
                ResumableWait {
                    checkpoint,
                    runtime,
                    next_sequence,
                }
            }),
        )
    }

    /// Whether any attempt without a completion has a wait checkpoint, so
    /// a newly watched session needs an initial reconciliation.
    pub(crate) async fn has_job_wait_checkpoint_for(
        database: &eidetica::Database,
        attempt_ids: &std::collections::HashSet<String>,
    ) -> anyhow::Result<bool> {
        if attempt_ids.is_empty() {
            return Ok(false);
        }
        let txn = database.new_transaction().await?;
        Ok(!txn
            .get_store::<Table<JobWaitCheckpoint>>(JOB_WAIT_CHECKPOINTS_STORE)
            .await?
            .search(|checkpoint| attempt_ids.contains(&checkpoint.attempt_id))
            .await?
            .is_empty())
    }
}

/// Rebuild the attempt's runtime state from its paired transcript, ending at
/// the checkpointed wait. Any gap, mismatch, completed or advanced turn,
/// unknown record, or wait that is not the last call of its batch refuses.
pub(crate) fn plan_job_wait_resume(
    checkpoint: &JobWaitCheckpoint,
    records: &[TurnTranscriptRecord],
) -> Result<(RuntimeResume, u64), &'static str> {
    if checkpoint.version != JOB_WAIT_CHECKPOINT_VERSION {
        return Err("unknown checkpoint version");
    }
    if checkpoint.observation_finished {
        return Err("wait observation finished before interruption");
    }
    let window = checkpoint.deadline - checkpoint.checkpointed_at;
    if window <= chrono::Duration::zero() || window > chrono::Duration::seconds(240) {
        return Err("checkpoint deadline is outside the job_wait bound");
    }
    if checkpoint.policy_timeout_seconds == 0 {
        return Err("checkpoint has no policy timeout");
    }
    let policy_deadline = checkpoint
        .policy_deadline()
        .ok_or("checkpoint policy deadline overflows")?;
    let arguments: serde_json::Value =
        serde_json::from_str(&checkpoint.arguments).map_err(|_| "checkpoint arguments")?;
    match crate::tools::jobs::job_wait_arguments(&arguments) {
        Ok((child, _)) if child == checkpoint.child_session_db_id => {}
        _ => return Err("checkpoint arguments do not name its child"),
    }

    let mut sorted = records.to_vec();
    sorted.sort_by_key(|record| record.sequence);
    for (expected, record) in sorted.iter().enumerate() {
        if record.sequence != expected as u64
            || record.attempt_id != checkpoint.attempt_id
            || record.request_id != checkpoint.request_id
        {
            return Err("transcript is not one contiguous attempt");
        }
    }

    let mut messages = Vec::new();
    let mut metadata = Vec::new();
    let mut matrix_send_queued = false;
    let mut rate_limited_calls = Vec::new();
    let mut records = sorted.iter().peekable();
    let mut model_sequence = 0;
    let pending_call = loop {
        let Some(record) = records.next() else {
            return Err("transcript ends before the checkpointed call");
        };
        let TurnTranscriptMessage::ModelResponse {
            model_sequence: sequence,
            content,
            tool_calls,
            provider_extra,
            metadata: response_metadata,
            terminal,
        } = &record.message
        else {
            return Err("expected a model response");
        };
        if *sequence != model_sequence || *terminal || tool_calls.is_empty() {
            return Err("model response is out of order, terminal, or has no calls");
        }
        if *sequence > checkpoint.model_sequence {
            return Err("transcript advanced past the checkpoint");
        }
        let pending = *sequence == checkpoint.model_sequence;
        if pending
            && (checkpoint.call_index.checked_add(1) != Some(tool_calls.len())
                || tool_calls[checkpoint.call_index].id != checkpoint.call_id
                || tool_calls[checkpoint.call_index].name != crate::runtime::JOB_WAIT_TOOL)
        {
            return Err("checkpointed call is not the final job_wait of its batch");
        }
        metadata.extend(response_metadata.clone());
        messages.push(RuntimeMessage::AssistantToolCalls {
            content: content.clone(),
            tool_calls: tool_calls.clone(),
            provider_extra: provider_extra.clone(),
        });
        let settled = if pending {
            checkpoint.call_index
        } else {
            tool_calls.len()
        };
        for (call_index, call) in tool_calls.iter().take(settled).enumerate() {
            let Some(result) = records.next() else {
                return Err("tool call has no recorded result");
            };
            let TurnTranscriptMessage::ToolResult {
                model_sequence: result_sequence,
                call_index: index,
                call_id,
                name,
                output,
                outcome,
            } = &result.message
            else {
                return Err("expected a tool result");
            };
            if *result_sequence != *sequence
                || *index != call_index
                || call_id != &call.id
                || name != &call.name
            {
                return Err("tool result is not paired with its call");
            }
            // The exact content the live runtime appended for this outcome.
            let content = match outcome {
                ToolResultOutcome::Unknown { .. } => return Err("unknown tool outcome"),
                ToolResultOutcome::Denied | ToolResultOutcome::ApprovalTimedOut => output.clone(),
                _ => crate::runtime::wrap_tool_output(name, output),
            };
            if !matches!(
                outcome,
                ToolResultOutcome::RateLimited | ToolResultOutcome::Unavailable
            ) {
                rate_limited_calls.push((name.clone(), result.timestamp));
            }
            matrix_send_queued |= name == "matrix__send" && *outcome == ToolResultOutcome::Success;
            messages.push(RuntimeMessage::ToolResult {
                call_id: call.id.clone(),
                content,
            });
        }
        if pending {
            break tool_calls[checkpoint.call_index].clone();
        }
        model_sequence += 1;
    };
    if records.next().is_some() {
        return Err("checkpointed wait already has a result");
    }
    rate_limited_calls.push((pending_call.name.clone(), checkpoint.checkpointed_at));
    Ok((
        RuntimeResume {
            prior_messages: messages,
            model_sequence: checkpoint.model_sequence,
            call_index: checkpoint.call_index,
            call: crate::runtime::ToolCallRequest {
                arguments: checkpoint.arguments.clone(),
                ..pending_call
            },
            deadline: checkpoint.deadline,
            policy_deadline,
            policy_timeout_seconds: checkpoint.policy_timeout_seconds,
            metadata,
            matrix_send_queued,
            rate_limited_calls,
        },
        sorted.len() as u64,
    ))
}

#[cfg(test)]
#[path = "wait_resume_tests.rs"]
mod tests;
