//! The v1 persistence branch of the attempt-bound recorder. Admission exists
//! only in tests: no config, descriptor, reopened database or retry command
//! can grant one. All commits are owned here, outside the Store.
use super::*;
mod compaction;
#[cfg(test)]
use crate::session::{
    TurnRequestId,
    conversation::staging::{AcceptedInput, stage_descriptor},
};
use crate::{
    extension::projection::ContextSource,
    runtime::{CallPreparation, RuntimeMessage, RuntimeRecord, TurnCancel},
    session::{
        TURN_ATTEMPTS_STORE, TurnAttemptStatus,
        conversation::{
            CallId, CallInput, CallMessageBinding, FactId, Fidelity, RepresentationId, staging::*,
        },
        history::HistoryBody,
    },
};
use anyhow::{Context, ensure};
use eidetica::{Transaction, store::Table};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub(in crate::session) enum FailCommit {
    Response,
    Completion,
}

pub(super) struct FencedTurn {
    // Only this recorder's commits advance the selected lineage.
    frontier: Snapshot,
    source: Snapshot,
    compaction: Option<Box<compaction::State>>,
    cancel: Option<TurnCancel>,
    input: RepresentationId,
    next_model: u64,
    pending: Option<usage::CallObservation>,
    responses: HashMap<u64, FactId>,
    // Slot ownership comes from the host's baseline indices and the recorder's
    // own native exchange IDs. Comparing values below only checks the binding.
    additions: BTreeMap<usize, FactId>,
    tools: Vec<FactId>,
    exchanges: HashMap<u64, Vec<RepresentationId>>,
    terminal: Option<FactId>,
    #[cfg(test)]
    fail: Option<FailCommit>,
}

#[cfg(test)]
impl Session {
    pub(in crate::session) async fn start_fenced_turn(
        session: Arc<Mutex<Self>>,
        branch: String,
        agent: String,
        entry: SessionEntry,
        cancel: TurnCancel,
        fail: Option<FailCommit>,
    ) -> anyhow::Result<TurnRecorder> {
        Self::fenced_fixture(session, branch, agent, entry, Some(cancel), fail, false).await
    }

    pub(in crate::session) async fn start_compacting_fixture(
        session: Arc<Mutex<Self>>,
        branch: String,
        agent: String,
        entry: SessionEntry,
    ) -> anyhow::Result<TurnRecorder> {
        Self::fenced_fixture(session, branch, agent, entry, None, None, true).await
    }

    async fn fenced_fixture(
        session: Arc<Mutex<Self>>,
        branch: String,
        agent: String,
        entry: SessionEntry,
        cancel: Option<TurnCancel>,
        fail: Option<FailCommit>,
        compaction: bool,
    ) -> anyhow::Result<TurnRecorder> {
        let mut session_guard = session.lock().await;
        session_guard.refresh_writer().await?;
        let db = session_guard.database();
        let boundary = Snapshot::from([db.root_id().clone()]);
        let read = Session::history(db).await?;
        ensure!(
            !branch.is_empty()
                && !agent.is_empty()
                && entry.entry_type == EntryType::Message
                && !entry.sender.is_empty()
                && !entry.content.is_empty()
                && entry.metadata.is_none()
                && entry.routing.is_none(),
            "fixture requires an unaddressed local message"
        );
        let active = if read.snapshot() == &boundary && read.entries().await?.is_empty() {
            let txn = db.new_transaction_at(&boundary).await?;
            stage_descriptor(&txn, db, &boundary, compaction).await?;
            Snapshot::from([txn.commit().await?])
        } else if compaction {
            let at = read.snapshot().clone();
            let txn = db.new_transaction_at(&at).await?;
            let staged = Staging::at(db, &at, &txn, &branch).await?;
            ensure!(staged.supports_compaction(), "not a compaction fixture");
            let attempts = txn
                .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
                .await?;
            ensure!(
                attempts
                    .search(|attempt| attempt.status != TurnAttemptStatus::Completed)
                    .await?
                    .is_empty(),
                "fixture cannot replay an interrupted attempt"
            );
            ensure!(
                !staged.has_branch(&branch),
                "fixture branch already accepted"
            );
            at
        } else {
            anyhow::bail!("fixture requires a fresh session");
        };
        let (frontier, input, attempt) = accept_fixture_input(db, &active, &branch, entry).await?;
        let identity = TurnCallIdentity {
            agent,
            requested_model: String::new(),
            execution_id: Some(branch),
        };
        drop(session_guard);
        let mut recorder = Self::turn_recorder(session, &attempt, identity, Default::default());
        recorder.fenced_compaction = compaction;
        recorder.fenced = Some(Mutex::new(FencedTurn {
            source: frontier.clone(),
            frontier,
            compaction: compaction.then(Box::default),
            cancel,
            input,
            next_model: 0,
            pending: None,
            responses: Default::default(),
            additions: Default::default(),
            tools: Vec::new(),
            exchanges: Default::default(),
            terminal: None,
            fail,
        }));
        Ok(recorder)
    }
}

#[cfg(test)]
pub(super) async fn accept_fixture_input(
    db: &Database,
    active: &Snapshot,
    branch: &str,
    entry: SessionEntry,
) -> anyhow::Result<(Snapshot, RepresentationId, TurnAttempt)> {
    let txn = db.new_transaction_at(active).await?;
    let staged = Staging::at(db, active, &txn, branch).await?;
    let input = staged
        .append(vec![(
            Payload::Input {
                sender: entry.sender,
                text: entry.content,
            },
            View::Message(MessageRole::User),
        )])
        .await?;
    let accepted = AcceptedInput {
        fact: input.fact_id(0),
        branch: branch.into(),
        attempt: uuid::Uuid::new_v4().to_string(),
    };
    let attempt = TurnAttempt {
        attempt_id: accepted.attempt.clone(),
        request_id: TurnRequestId::parse(accepted.request()),
        started_at: entry.timestamp,
        generation: 1,
        status: TurnAttemptStatus::Started,
        completed_at: None,
    };
    let input = accepted.representation();
    stage_observation(&txn, Observation::InputAccepted { input: accepted }).await?;
    stage_attempt(&txn, &attempt).await?;
    let frontier = Snapshot::from([txn.commit().await?]);
    Ok((frontier, input, attempt))
}

async fn stage_attempt(txn: &Transaction, attempt: &TurnAttempt) -> anyhow::Result<()> {
    txn.get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
        .await?
        .set(&attempt.attempt_id, attempt.clone())
        .await?;
    Ok(())
}

impl TurnRecorder {
    fn call_id(&self, model_sequence: u64) -> CallId {
        CallId {
            request: self.attempt.request_id.as_str().into(),
            branch: self
                .identity
                .execution_id
                .clone()
                .expect("fenced recorder has a branch"),
            model_sequence,
        }
    }

    pub(super) async fn fenced_prepare(
        &self,
        fenced: &Mutex<FencedTurn>,
        call: CallPreparation<'_>,
    ) -> anyhow::Result<()> {
        let session = self.write_session().await.map_err(anyhow::Error::msg)?;
        let db = session.database();
        let mut state = fenced.lock().await;
        let sequence = call.observation.model_sequence;
        let api = call
            .api
            .filter(|api| *api == crate::config::ModelApi::ChatCompletions)
            .context("fenced turn only accepts the Chat Completions serializer")?;
        ensure!(
            call.max_retries == 0
                && sequence == state.next_model
                && state.pending.is_none()
                && state.terminal.is_none(),
            "fenced call cannot retry, overtake or extend a terminal response"
        );
        let sources = call
            .sources
            .context("fenced turn requires signed sources after context selection")?;
        ensure!(
            sources.len() == call.messages.len(),
            "missing model input sources"
        );
        let id = self.call_id(sequence);
        let txn = state.transaction(db).await?;
        let staged = Staging::at(db, &state.frontier, &txn, &id.branch).await?;
        let groups = staged.context()?.groups;
        let rendered: BTreeMap<_, _> = groups
            .into_iter()
            .filter_map(|group| match group.body {
                HistoryBody::Recorded { messages, .. } => Some(messages),
                HistoryBody::Entry(_) => None,
            })
            .flatten()
            .map(|(msg, id)| (id, msg))
            .collect();
        let mut representations = Vec::new();
        let mut offsets = HashMap::new();
        let mut messages = Vec::new();
        let mut additions = Vec::new();
        let mut new_slots = Vec::new();
        let mut new_values = Vec::new();
        let mut addition_slot = 0;
        for (index, (source, message)) in sources.iter().zip(call.messages).enumerate() {
            let representation = match source {
                ContextSource::ConversationRepresentation { id } => id,
                ContextSource::CurrentAttempt { model_sequence } => {
                    let offset = offsets.entry(*model_sequence).or_insert(0usize);
                    let id = state
                        .exchanges
                        .get(model_sequence)
                        .and_then(|ids| ids.get(*offset))
                        .context("model slot has no native exchange member")?;
                    *offset += 1;
                    id
                }
                ContextSource::Instructions | ContextSource::TurnInjection => {
                    let value = serde_json::to_value(match (source, message) {
                        (ContextSource::Instructions, RuntimeMessage::System(text)) => {
                            AcceptedValue::Instruction(text.clone())
                        }
                        (ContextSource::TurnInjection, RuntimeMessage::System(text)) => {
                            AcceptedValue::BeforeAgent(text.clone())
                        }
                        _ => anyhow::bail!("unsupported synthetic hook or instruction value"),
                    })?;
                    let slot = addition_slot;
                    addition_slot += 1;
                    if let Some(id) = state.additions.get(&slot) {
                        ensure!(
                            staged.payload(id)? == &(Payload::ControlObservation { value }),
                            "accepted addition changed between calls"
                        );
                        additions.push(id.clone());
                        messages.push(Some(CallMessageBinding::AcceptedAddition(id.clone())));
                    } else {
                        ensure!(sequence == 0, "new addition after first call");
                        new_slots.push((slot, index));
                        new_values.push((Payload::ControlObservation { value }, View::AuditOnly));
                        messages.push(None);
                    }
                    continue;
                }
                _ => anyhow::bail!("unsupported model-facing source on fenced turn"),
            };
            ensure!(
                rendered.get(representation) == Some(message),
                "model slot differs from its signed representation"
            );
            representations.push(representation.clone());
            messages.push(Some(CallMessageBinding::Representation(
                representation.clone(),
            )));
        }
        ensure!(
            representations.contains(&state.input),
            "model request omits its accepted input"
        );
        let original_tools = call.tools.iter().map(|tool| Payload::ControlObservation {
            value: serde_json::json!(AcceptedValue::Tool(tool.clone())),
        });
        if sequence == 0 {
            new_values.extend(original_tools.map(|payload| (payload, View::AuditOnly)));
        } else {
            ensure!(
                state.tools.len() == call.tools.len(),
                "tool declarations changed between calls"
            );
            for (id, payload) in state.tools.iter().zip(original_tools) {
                ensure!(
                    staged.payload(id)? == &payload,
                    "accepted tool declaration changed"
                );
            }
        }
        if !new_values.is_empty() {
            let op = staged.append(new_values).await?;
            for (i, (slot, index)) in new_slots.into_iter().enumerate() {
                let id = op.fact_id(i.try_into()?);
                state.additions.insert(slot, id.clone());
                additions.push(id.clone());
                messages[index] = Some(CallMessageBinding::AcceptedAddition(id));
            }
            state.tools = (0..call.tools.len())
                .map(|i| {
                    op.outputs[op.outputs.len() - call.tools.len() + i]
                        .fact
                        .clone()
                })
                .collect();
        }
        additions.extend(state.tools.iter().cloned());
        let input = CallInput {
            call: id.clone(),
            attempt: self.attempt.attempt_id.clone(),
            observed: staged.observed.clone(),
            representations,
            renderer_version: 1,
            context_version: 1,
            requested_model: call.observation.requested_model.clone(),
            api: api.as_str().into(),
            effective_nonsecret_settings: [("cache".into(), serde_json::to_value(call.cache)?)]
                .into(),
            sources: Vec::new(),
            accepted_additions: additions,
            messages: messages
                .into_iter()
                .collect::<Option<_>>()
                .context("missing prepared slot")?,
            tools: state.tools.clone(),
            fidelity: Fidelity::Approximate,
            reasons: vec!["Accepted inputs and declarations, not serializer byte evidence".into()],
        };
        staged.validate_preparation(&input)?;
        if let Some(compaction) = &mut state.compaction {
            compaction.prepared = Some(input.clone());
        }
        stage_observation(
            &txn,
            Observation::Prepared {
                input: Box::new(input),
            },
        )
        .await?;
        Box::pin(state.commit(&session, &self.attempt, txn)).await?;
        let intent = state.transaction(db).await?;
        stage_observation(
            &intent,
            Observation::DispatchIntent {
                call: id,
                attempt: self.attempt.attempt_id.clone(),
            },
        )
        .await?;
        Box::pin(state.commit(&session, &self.attempt, intent)).await?;
        state.next_model += 1;
        state.pending = Some(call.observation);
        Ok(())
    }

    pub(super) async fn fenced_observe(
        &self,
        fenced: &Mutex<FencedTurn>,
        call: usage::CallObservation,
    ) -> anyhow::Result<()> {
        let mut state = fenced.lock().await;
        ensure!(
            call.responded
                && state
                    .pending
                    .as_ref()
                    .is_some_and(
                        |prior| prior.model_sequence == call.model_sequence && !prior.responded
                    ),
            "unmatched fenced model return"
        );
        // Stage accounting with the native response through record(), never as
        // an independently committed telemetry row.
        state.pending = Some(call);
        Ok(())
    }

    pub(super) async fn fenced_record(
        &self,
        fenced: &Mutex<FencedTurn>,
        message: RuntimeRecord,
    ) -> anyhow::Result<()> {
        if matches!(message, RuntimeRecord::Cancelled)
            && let Some(cancel) = &fenced.lock().await.cancel
        {
            ensure!(cancel.is_cancelled(), "unmatched fenced stop");
            return Ok(());
        }
        let session = self.write_session().await.map_err(anyhow::Error::msg)?;
        let db = session.database();
        let mut state = fenced.lock().await;
        let txn = state.transaction(db).await?;
        let branch = self
            .identity
            .execution_id
            .as_deref()
            .context("missing fenced branch")?;
        let staged = Staging::at(db, &state.frontier, &txn, branch).await?;
        match message {
            RuntimeRecord::ModelResponse {
                model_sequence,
                content,
                tool_calls,
                provider_extra,
                metadata,
                terminal,
            } => {
                let call = self.call_id(model_sequence);
                let observed = state
                    .pending
                    .take()
                    .filter(|obs| {
                        obs.responded
                            && obs.model_sequence == model_sequence
                            && obs.metadata == metadata
                    })
                    .context("response lacks its returned call observation")?;
                let native = NativeResponse {
                    call: call.clone(),
                    attempt: self.attempt.attempt_id.clone(),
                    content,
                    tool_calls,
                    provider_extra: provider_extra.into_iter().collect(),
                    metadata,
                };
                let op = staged
                    .append(vec![(
                        Payload::NativeModelResponse { response: native },
                        View::NativeExchange,
                    )])
                    .await?;
                let response = op.fact_id(0);
                stage_observation(
                    &txn,
                    Observation::Outcome {
                        call,
                        attempt: self.attempt.attempt_id.clone(),
                        status: CallStatus::Responded,
                        response: Some(response.clone()),
                        accounting: Some(Box::new(Accounting {
                            agent: self.identity.agent.clone(),
                            timestamp: observed.timestamp,
                            request_budget_tokens: observed.request_budget_tokens,
                            input_tokens: observed.input_tokens,
                            output_tokens: observed
                                .output_tokens
                                .context("missing output accounting")?,
                            prices: observed.prices,
                        })),
                    },
                )
                .await?;
                #[cfg(test)]
                ensure!(
                    state.fail != Some(FailCommit::Response),
                    "injected refusal after staging response and accounting"
                );
                Box::pin(state.commit(&session, &self.attempt, txn)).await?;
                state.responses.insert(model_sequence, response.clone());
                state
                    .exchanges
                    .insert(model_sequence, vec![op.representation_id(0)]);
                if terminal {
                    state.terminal = Some(response);
                }
            }
            RuntimeRecord::ToolResult {
                model_sequence,
                call_index,
                call_id,
                name,
                output,
                outcome,
            } => {
                let response = state
                    .responses
                    .get(&model_sequence)
                    .context("tool result without native response")?;
                let Payload::NativeModelResponse { response: native } = staged.payload(response)?
                else {
                    anyhow::bail!("tool result without native call");
                };
                let call = native
                    .tool_calls
                    .get(call_index)
                    .filter(|call| call.id == call_id && call.name == name)
                    .context("tool result has wrong occurrence")?;
                let execution = ToolExecutionId {
                    call: ToolCallId {
                        response: response.clone(),
                        index: call_index.try_into()?,
                    },
                    attempt: self.attempt.attempt_id.clone(),
                    ordinal: 0,
                };
                let op = staged
                    .append(vec![(
                        Payload::ToolOutcome {
                            execution,
                            executed_arguments: call.arguments.clone(),
                            content: output,
                            outcome,
                        },
                        View::ToolResult,
                    )])
                    .await?;
                Box::pin(state.commit(&session, &self.attempt, txn)).await?;
                state
                    .exchanges
                    .get_mut(&model_sequence)
                    .context("missing response representation")?
                    .push(op.representation_id(0));
            }
            _ => anyhow::bail!("unsupported runtime observation on fenced turn"),
        }
        Ok(())
    }

    pub(super) async fn fenced_complete(
        &self,
        fenced: &Mutex<FencedTurn>,
        session: &mut Session,
        final_entry: Option<SessionEntry>,
        outbound: Option<SessionEntry>,
    ) -> anyhow::Result<()> {
        let final_entry = final_entry
            .filter(|entry| {
                entry.entry_type == EntryType::Message
                    && entry.routing.is_none()
                    && entry.sender == self.identity.agent
            })
            .context("fenced turn requires a local final message")?;
        ensure!(outbound.is_none(), "fenced turn cannot settle an outbox");
        session.refresh_writer().await?;
        let db = session.database();
        let mut state = fenced.lock().await;
        let response = state
            .terminal
            .take()
            .context("no unsettled terminal response")?;
        let txn = state.transaction(db).await?;
        let staged = Staging::at(
            db,
            &state.frontier,
            &txn,
            self.identity
                .execution_id
                .as_deref()
                .context("missing fenced branch")?,
        )
        .await?;
        let publication = staged.publish(&response, &final_entry.content).await?;
        stage_observation(
            &txn,
            Observation::TurnCompleted {
                completion: Completion {
                    response,
                    publication,
                    attempt: self.attempt.attempt_id.clone(),
                },
            },
        )
        .await?;
        stage_attempt(
            &txn,
            &TurnAttempt {
                status: TurnAttemptStatus::Completed,
                completed_at: Some(final_entry.timestamp),
                ..self.attempt.clone()
            },
        )
        .await?;
        #[cfg(test)]
        ensure!(
            state.fail != Some(FailCommit::Completion),
            "injected refusal after staging completion"
        );
        Box::pin(state.commit(session, &self.attempt, txn)).await?;
        Ok(())
    }
}

impl FencedTurn {
    async fn transaction(&self, db: &Database) -> anyhow::Result<Transaction> {
        if self.compaction.is_none() {
            Session::require_accepted_history_frontier(db, &self.frontier).await?;
        }
        Ok(db.new_transaction_at(&self.frontier).await?)
    }

    async fn commit(
        &mut self,
        session: &Session,
        attempt: &TurnAttempt,
        txn: Transaction,
    ) -> anyhow::Result<()> {
        if let Some(cancel) = &self.cancel {
            // Eidetica signs against the transaction's historical parents. Check
            // live authority separately; the synthetic writer does not provide a
            // distributed compare-and-swap against revocation or cancellation.
            ensure!(
                session
                    .database()
                    .current_permission()
                    .await
                    .context("fenced writer authority unavailable")?
                    .can_write(),
                "fenced writer has no current write permission"
            );
            ensure!(
                session
                    .read_turn_attempt(&attempt.attempt_id)
                    .await?
                    .as_ref()
                    == Some(attempt),
                "fenced turn no longer owns its attempt"
            );
            ensure!(!cancel.is_cancelled(), "fenced turn cancelled");
        }
        self.frontier = Snapshot::from([txn.commit().await?]);
        Ok(())
    }
}
