//! Persistence for the runtime's awaited recording seam. The executor still
//! schedules calls, tool effects and publication; this recorder writes the
//! selected history format without choosing when to run them.
use super::{
    EntryType, Session, SessionEntry, TurnAttempt, TurnCallIdentity, TurnRequestId,
    TurnTranscriptMessage, TurnTranscriptRecord, execution, jobs, steering, usage, wait_resume,
};
use crate::runtime;
use chrono::Utc;
use eidetica::{Database, Snapshot};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::sync::Mutex;

mod fenced;
#[cfg(test)]
mod fenced_tests;

/// The writer bound to one attempt and one execution branch.
pub(crate) struct TurnRecorder {
    session: Arc<Mutex<Session>>,
    attempt: TurnAttempt,
    identity: TurnCallIdentity,
    sequence: AtomicU32,
    terminal: std::sync::Mutex<Option<TurnTranscriptRecord>>,
    explicit_bridge_send: AtomicBool,
    /// The session state this attempt's context was assembled from. Without
    /// it a wait cannot be reconstructed, so none is checkpointed.
    context_snapshot: Option<Snapshot>,
    can_suspend: bool,
    #[cfg(test)]
    fail_after: Option<Arc<std::sync::Mutex<Option<usize>>>>,
    fenced: Option<Mutex<fenced::FencedTurn>>,
    fenced_admitted: AtomicBool,
    fenced_compaction: bool,
}

#[derive(Default)]
pub(crate) struct TurnRecorderOptions {
    pub next_sequence: u64,
    pub explicit_bridge_send: bool,
    pub context_snapshot: Option<Snapshot>,
    pub can_suspend: bool,
    #[cfg(test)]
    pub fail_after: Option<Arc<std::sync::Mutex<Option<usize>>>>,
}

impl Session {
    pub(crate) fn turn_recorder(
        session: Arc<Mutex<Self>>,
        attempt: &TurnAttempt,
        identity: TurnCallIdentity,
        options: TurnRecorderOptions,
    ) -> TurnRecorder {
        TurnRecorder {
            session,
            attempt: attempt.clone(),
            identity,
            sequence: AtomicU32::new(options.next_sequence as u32),
            terminal: Default::default(),
            explicit_bridge_send: AtomicBool::new(options.explicit_bridge_send),
            context_snapshot: options.context_snapshot,
            can_suspend: options.can_suspend,
            #[cfg(test)]
            fail_after: options.fail_after,
            fenced: None,
            fenced_admitted: AtomicBool::new(false),
            fenced_compaction: false,
        }
    }
}

impl TurnRecorder {
    pub(crate) fn has_explicit_bridge_send(&self) -> bool {
        self.explicit_bridge_send.load(Ordering::Acquire)
    }

    /// Settle using the caller's existing lock on this recorder's Session,
    /// held across any reply preflight and the commit.
    pub(crate) async fn complete(
        &self,
        session: &mut Session,
        final_entry: Option<SessionEntry>,
        outbound: Option<SessionEntry>,
    ) -> anyhow::Result<()> {
        if let Some(fenced) = &self.fenced {
            return Box::pin(self.fenced_complete(fenced, session, final_entry, outbound)).await;
        }
        let transcript = self.terminal.lock().unwrap().clone();
        session
            .complete_turn_attempt_for_branch(
                &self.attempt,
                final_entry,
                outbound,
                transcript,
                self.identity.execution_id.as_deref(),
            )
            .await
    }

    /// Keep late job evidence without writing a receipt or completing the attempt.
    pub(crate) async fn preserve_undelivered(
        &self,
        db: &Database,
        terminal: jobs::JobTerminal,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.fenced.is_none(), "fenced turn cannot settle a job");
        let branch =
            self.identity.execution_id.as_ref().ok_or_else(|| {
                anyhow::anyhow!("undelivered outcome requires an execution branch")
            })?;
        let transcript = self.terminal.lock().unwrap().take();
        execution::record_undelivered(
            db,
            &execution::ExecutionOutcome {
                execution_id: branch.clone(),
                result: jobs::JobResult {
                    attempt_id: self.attempt.attempt_id.clone(),
                    terminal,
                    execution_id: Some(branch.clone()),
                },
                delivered: false,
            },
            transcript.as_ref(),
        )
        .await
    }

    async fn write_session(&self) -> Result<tokio::sync::MutexGuard<'_, Session>, String> {
        let mut session = self.session.lock().await;
        session
            .refresh_writer()
            .await
            .map_err(|error| format!("runtime persistence failed: writer refresh: {error}"))?;
        Ok(session)
    }
}

impl runtime::RuntimeRecorder for TurnRecorder {
    fn context_compaction(&self) -> Option<&dyn crate::session::ContextCompaction> {
        self.fenced_compaction.then_some(self)
    }

    fn begin_turn(&self, resuming: bool) -> Result<(), String> {
        if self.fenced.is_some() && (resuming || self.fenced_admitted.swap(true, Ordering::AcqRel))
        {
            return Err("fenced attempt cannot execute twice or resume".into());
        }
        Ok(())
    }

    fn prepare_call<'a>(
        &'a self,
        call: runtime::CallPreparation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(fenced) = &self.fenced {
                return self
                    .fenced_prepare(fenced, call)
                    .await
                    .map_err(|err| err.to_string());
            }
            self.input_dispatch(
                call.observation.model_sequence,
                call.messages,
                call.baseline,
            )
            .await?;
            self.observe_call(call.observation).await
        })
    }

    fn can_suspend_job_wait(&self) -> bool {
        self.can_suspend
    }

    fn input_boundary<'a>(
        &'a self,
        model_sequence: u64,
        closing: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<runtime::RuntimeMessage>, String>> + Send + 'a>>
    {
        Box::pin(async move {
            if self.fenced.is_some() {
                return Ok(Vec::new()); // no steering or job activity on this fixture
            }
            let session = self.write_session().await?;
            let mut messages: Vec<runtime::RuntimeMessage> = steering::input_boundary(
                session.database(),
                &self.attempt.attempt_id,
                model_sequence,
                closing,
            )
            .await
            .map(|texts| {
                texts
                    .into_iter()
                    .map(runtime::RuntimeMessage::User)
                    .collect()
            })
            .map_err(|error| format!("runtime persistence failed: job input boundary: {error}"))?;
            // Parallel-execution notices join at the same safe boundary;
            // a closing boundary never demands another model call for them.
            if let Some(branch) = &self.identity.execution_id {
                if let Some(yielded) = execution::observe(session.database(), branch)
                    .await
                    .map_err(|error| format!("execution observation failed: {error}"))?
                {
                    return Err(execution_yield_error(&yielded));
                }
                messages.extend(
                    execution::take_notices(session.database(), branch, closing)
                        .await
                        .map_err(|error| format!("execution notice delivery failed: {error}"))?
                        .into_iter()
                        .map(runtime::RuntimeMessage::User),
                );
            }
            Ok(messages)
        })
    }

    fn input_dispatch<'a>(
        &'a self,
        model_sequence: u64,
        messages: &'a [runtime::RuntimeMessage],
        baseline: &'a [runtime::RuntimeMessage],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let inputs = {
                let session = self.write_session().await?;
                steering::dispatch_inputs(
                    session.database(),
                    &self.attempt.attempt_id,
                    model_sequence,
                    messages,
                    baseline,
                )
                .await
                .map_err(|e| format!("steering dispatch refused: {e}"))?
            };
            if !inputs.is_empty() {
                self.record(runtime::RuntimeRecord::Steering {
                    model_sequence,
                    inputs,
                })
                .await?;
            }
            Ok(())
        })
    }

    fn record<'a>(
        &'a self,
        message: runtime::RuntimeRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(fenced) = &self.fenced {
                return self
                    .fenced_record(fenced, message)
                    .await
                    .map_err(|err| err.to_string());
            }
            #[cfg(test)]
            if let Some(counter) = &self.fail_after {
                let mut remaining = counter.lock().unwrap();
                if let Some(value) = remaining.as_mut() {
                    if *value == 0 {
                        return Err("injected transcript failure".to_string());
                    }
                    *value -= 1;
                }
            }
            if let runtime::RuntimeRecord::ModelResponse { model_sequence, .. } = &message {
                let session = self.write_session().await?;
                steering::input_call_observed(
                    session.database(),
                    &self.attempt.attempt_id,
                    *model_sequence,
                )
                .await
                .map_err(|error| format!("job input receipt: {error}"))?;
            }
            let explicit_send = matches!(
                &message,
                runtime::RuntimeRecord::ToolResult {
                    name,
                    outcome: runtime::ToolResultOutcome::Success,
                    ..
                } if name == "matrix__send"
            );
            let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) as u64;
            let record = transcript_record(
                self.attempt.request_id.clone(),
                self.attempt.attempt_id.clone(),
                self.identity.clone(),
                sequence,
                message,
            );
            if matches!(
                record.message,
                TurnTranscriptMessage::ModelResponse { terminal: true, .. }
                    | TurnTranscriptMessage::Cancelled
            ) {
                // Usage is observable before response hooks or reply preflight
                // finish. No final Message, completion or outbox is published.
                self.write_session()
                    .await?
                    .append_turn_transcript_for_branch(
                        record.clone(),
                        Vec::new(),
                        self.identity.execution_id.as_deref(),
                    )
                    .await
                    .map_err(|error| format!("failed to persist terminal telemetry: {error}"))?;
                *self.terminal.lock().unwrap() = Some(record);
                return Ok(());
            }
            let display_entries = display_entries_for_record(&self.identity.agent, &record);
            self.write_session()
                .await?
                .append_turn_transcript_for_branch(
                    record,
                    display_entries,
                    self.identity.execution_id.as_deref(),
                )
                .await
                .map_err(|error| format!("failed to persist runtime transcript: {error}"))?;
            if explicit_send {
                self.explicit_bridge_send.store(true, Ordering::Release);
            }
            Ok(())
        })
    }

    fn observe_call<'a>(
        &'a self,
        call: usage::CallObservation,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(fenced) = &self.fenced {
                return self
                    .fenced_observe(fenced, call)
                    .await
                    .map_err(|err| err.to_string());
            }
            let session = self.write_session().await?;
            usage::persist_call(
                session.database(),
                &self.attempt.attempt_id,
                self.identity.execution_id.as_deref(),
                &self.identity.agent,
                call,
            )
            .await
        })
    }

    fn checkpoint_job_wait<'a>(
        &'a self,
        wait: runtime::JobWaitIntent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if self.fenced.is_some() {
                return Err("fenced turn cannot checkpoint a job wait".into());
            }
            let Some(snapshot) = self.context_snapshot.clone() else {
                return Err("turn context has no recorded snapshot".into());
            };
            let checkpoint = wait_resume::JobWaitCheckpoint::new(
                self.attempt.request_id.clone(),
                self.attempt.attempt_id.clone(),
                self.identity.agent.clone(),
                snapshot,
                wait,
            );
            let session = self.write_session().await?;
            session
                .record_job_wait_checkpoint(&checkpoint)
                .await
                .map_err(|error| format!("failed to persist job wait checkpoint: {error}"))?;
            if self.can_suspend {
                // The tool will not enter until readmission. Keep the same
                // bounded monitor activity that a resident wait records.
                session
                    .start_job_wait(
                        &self.attempt.request_id,
                        &format!(
                            "{}:{}:{}",
                            checkpoint.model_sequence, checkpoint.call_index, checkpoint.call_id
                        ),
                        &checkpoint.child_session_db_id,
                        checkpoint.deadline,
                    )
                    .await
                    .map_err(|error| format!("failed to record entered job wait: {error}"))?;
            }
            Ok(())
        })
    }

    fn finish_job_wait<'a>(
        &'a self,
        model_sequence: u64,
        call_index: usize,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.write_session()
                .await?
                .finish_job_wait_checkpoint(&self.attempt.attempt_id, model_sequence, call_index)
                .await
                .map_err(|error| format!("failed to retire job wait checkpoint: {error}"))
        })
    }
}

pub(crate) const EXECUTION_YIELDED: &str = "execution yielded to an earlier branch";

fn execution_yield_error(yielded: &execution::ExecutionYield) -> String {
    format!(
        "{EXECUTION_YIELDED}: {} (entry {})",
        yielded.earlier_execution, yielded.earlier_entry
    )
}

pub(crate) fn transcript_record(
    request_id: TurnRequestId,
    attempt_id: String,
    identity: TurnCallIdentity,
    sequence: u64,
    message: runtime::RuntimeRecord,
) -> TurnTranscriptRecord {
    use TurnTranscriptMessage as T;
    use runtime::RuntimeRecord as R;
    let message = match message {
        R::Steering {
            model_sequence,
            inputs,
        } => T::Steering {
            model_sequence,
            inputs,
        },
        R::Cancelled => T::Cancelled,
        R::ModelResponse {
            model_sequence,
            content,
            tool_calls,
            provider_extra,
            metadata,
            terminal,
        } => T::ModelResponse {
            model_sequence,
            content,
            tool_calls,
            provider_extra,
            metadata,
            terminal,
        },
        R::ToolResult {
            model_sequence,
            call_index,
            call_id,
            name,
            output,
            outcome,
        } => T::ToolResult {
            model_sequence,
            call_index,
            call_id,
            name,
            output,
            outcome,
        },
    };
    TurnTranscriptRecord {
        request_id,
        attempt_id,
        sequence,
        timestamp: Utc::now(),
        identity: Some(identity),
        message,
    }
}

pub(crate) fn display_entries_for_record(
    agent_name: &str,
    record: &TurnTranscriptRecord,
) -> Vec<SessionEntry> {
    match &record.message {
        TurnTranscriptMessage::Cancelled
        | TurnTranscriptMessage::Unknown { .. }
        | TurnTranscriptMessage::ToolResult {
            outcome: runtime::ToolResultOutcome::Unknown { .. },
            ..
        } => Vec::new(),
        TurnTranscriptMessage::Steering { inputs, .. } => inputs
            .iter()
            .map(|input| SessionEntry {
                sender: "user".into(),
                content: input.text.clone(),
                timestamp: record.timestamp,
                entry_type: EntryType::TurnContext,
                metadata: None,
                routing: None,
            })
            .collect(),
        TurnTranscriptMessage::ModelResponse {
            content,
            tool_calls,
            ..
        } => content
            .iter()
            .filter(|t| !t.is_empty())
            .map(|text| SessionEntry {
                sender: agent_name.into(),
                content: text.clone(),
                timestamp: record.timestamp,
                entry_type: EntryType::TurnContext,
                metadata: None,
                routing: None,
            })
            .chain(tool_calls.iter().map(|call| SessionEntry {
                sender: agent_name.to_string(),
                content: format!("{}({})", call.name, call.arguments),
                timestamp: record.timestamp,
                entry_type: EntryType::ToolCall,
                metadata: None,
                routing: None,
            }))
            .collect(),
        TurnTranscriptMessage::ToolResult {
            name,
            output,
            outcome,
            ..
        } => {
            let output = if *outcome != runtime::ToolResultOutcome::Success {
                format!("{name}: ERROR: {output}")
            } else {
                let truncated = crate::util::truncate_chars(output, 500);
                let suffix = if truncated.len() < output.len() {
                    "…"
                } else {
                    ""
                };
                format!("{name}: {truncated}{suffix}")
            };
            vec![SessionEntry {
                sender: agent_name.to_string(),
                content: output,
                timestamp: record.timestamp,
                entry_type: EntryType::ToolResult,
                metadata: None,
                routing: None,
            }]
        }
    }
}
