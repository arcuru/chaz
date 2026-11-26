//! Staging for the synthetic writer. The session host supplies and commits
//! each transaction; the Store never executes a turn or an effect.
#[cfg(test)]
use super::contract;
use super::{
    ConversationError, ConversationRead, ConversationStore, ReadAtoms, Result, STORE, atom, open,
    provenance, view,
};
use crate::{
    runtime::RuntimeMessage,
    session::{ContextReplacement, SessionContextView, history::HistoryBody},
};
use eidetica::{Database, Snapshot, Transaction};
use serde::Serialize;

use super::record::{
    Fact, FactId, ObservedBinding, Operation, OperationKind, Representation, RepresentationId,
};
#[cfg(test)]
pub(in crate::session) use super::turn::AcceptedInput;
pub(in crate::session) use super::{
    compaction::{Intent as CompactionIntent, Settlement as CompactionSettlement},
    provenance::{Accounting, CallStatus, ModelExecution, Observation},
    record::{MessageRole, NativeResponse, Payload, ToolCallId, ToolExecutionId, View},
    turn::{AcceptedValue, Completion},
};

pub(super) async fn atom_in(store: &impl ReadAtoms, value: &impl Serialize) -> Result<()> {
    let text = atom::encode(value)?;
    store.doc().set(atom::key(&text), text).await?;
    Ok(())
}

#[cfg(test)]
pub(in crate::session) async fn stage_descriptor(
    txn: &Transaction,
    db: &Database,
    at: &Snapshot,
    compaction: bool,
) -> Result<()> {
    stage_contract(
        txn,
        db,
        at,
        if compaction {
            contract::FENCED_COMPACTION
        } else {
            contract::FENCED_INPUT_APPENDS
        },
    )
    .await
}

#[cfg(test)]
pub(in crate::session) async fn stage_pinned_descriptor(
    txn: &Transaction,
    db: &Database,
    at: &Snapshot,
) -> Result<()> {
    stage_contract(txn, db, at, contract::PINNED_EXECUTIONS).await
}

#[cfg(test)]
async fn stage_contract(
    txn: &Transaction,
    db: &Database,
    at: &Snapshot,
    capability: &str,
) -> Result<()> {
    let mut required_operations = std::collections::BTreeSet::from([
        "append-v1".into(),
        "replace-v1".into(),
        contract::FENCED_TURN.into(),
    ]);
    required_operations.insert(capability.into());
    atom_in(
        &txn.get_store::<contract::ContractStore>(contract::STORE)
            .await?,
        &contract::Descriptor {
            version: 1,
            encoding: 1,
            context_semantics: 1,
            renderer: 1,
            required_operations,
            legacy: contract::LegacyBoundary {
                database: db.root_id().to_string(),
                frontier: at.clone(),
                adapter_version: 1,
            },
        },
    )
    .await
}

pub(in crate::session) async fn stage_observation(
    txn: &Transaction,
    observation: Observation,
) -> Result<()> {
    atom_in(
        &txn.get_store::<provenance::ProvenanceStore>(provenance::STORE)
            .await?,
        &provenance::Record {
            version: 1,
            observation,
        },
    )
    .await
}

pub(in crate::session) struct Staging<'a> {
    read: ConversationRead,
    txn: &'a Transaction,
    pub observed: ObservedBinding,
    branch: &'a str,
}

impl<'a> Staging<'a> {
    pub async fn at(
        db: &Database,
        at: &Snapshot,
        txn: &'a Transaction,
        branch: &'a str,
    ) -> Result<Self> {
        // The write side can already contain staged atoms from this model
        // call. Validate the original accepted cut, not those pending writes.
        let input = db.new_transaction_at(at).await?;
        let read = Box::pin(open(db, at, &input)).await?;
        read.effective()?;
        Ok(Self {
            read,
            txn,
            observed: ObservedBinding {
                database: db.root_id().to_string(),
                frontier: at.clone(),
            },
            branch,
        })
    }

    fn operation(&self, kind: OperationKind, targets: Vec<RepresentationId>) -> Result<Operation> {
        Ok(Operation {
            version: 1,
            op_id: uuid::Uuid::new_v4().to_string(),
            branch_id: self.branch.into(),
            observed: self.observed.clone(),
            causal_seq: view::next_sequence(&self.read.operations)?,
            kind,
            targets,
            facts: Vec::new(),
            outputs: Vec::new(),
        })
    }

    pub async fn append(&self, facts: Vec<(Payload, View)>) -> Result<Operation> {
        let op = self.new_append(facts)?;
        self.stage(&op).await?;
        Ok(op)
    }

    fn new_append(&self, facts: Vec<(Payload, View)>) -> Result<Operation> {
        let mut op = self.operation(OperationKind::Append, Vec::new())?;
        for (index, (payload, view)) in facts.into_iter().enumerate() {
            let index = u32::try_from(index)
                .map_err(|_| ConversationError::Malformed("too many facts".into()))?;
            op.facts.push(Fact {
                payload_version: 1,
                payload,
            });
            op.outputs.push(Representation {
                fact: op.fact_id(index),
                view,
                decoder_version: 1,
            });
        }
        Ok(op)
    }

    pub async fn publish(&self, response: &FactId, text: &str) -> Result<String> {
        let Payload::NativeModelResponse { response: native } = self.payload(response)? else {
            return Err(ConversationError::Integrity(
                "terminal fact is not a response".into(),
            ));
        };
        let target = RepresentationId {
            database: response.database.clone(),
            operation: response.operation.clone(),
            index: 0,
        };
        let mut op = self.operation(OperationKind::Replace, vec![target])?;
        let fact = if native.content.as_deref() == Some(text) {
            response.clone()
        } else {
            op.facts.push(Fact {
                payload_version: 1,
                payload: Payload::PublishedBody { text: text.into() },
            });
            op.fact_id(0)
        };
        op.outputs.push(Representation {
            fact,
            view: View::Message(MessageRole::Assistant),
            decoder_version: 1,
        });
        super::turn::validate_publication(&op, response, native)?;
        self.stage(&op).await?;
        Ok(op.op_id)
    }

    pub fn payload(&self, id: &FactId) -> Result<&Payload> {
        Ok(&view::fact(id, &self.read.operations)?.payload)
    }

    pub fn validate_preparation(&self, input: &super::CallInput) -> Result<()> {
        provenance::validate_selection(
            &self.read.effective()?,
            &input.representations,
            &input.messages,
        )
    }

    pub fn context(&self) -> Result<SessionContextView> {
        self.read.context()
    }

    pub fn context_for(&self, agent: &str) -> Result<SessionContextView> {
        self.read.context_for(agent)
    }

    async fn stage(&self, op: &Operation) -> Result<()> {
        view::validate_at_observed(op, &self.read.operations, &self.read.legacy)?;
        atom_in(&self.txn.get_store::<ConversationStore>(STORE).await?, op).await
    }
}

impl Staging<'_> {
    #[cfg(test)]
    pub async fn stage_fixture_op(&self, op: &Operation) -> Result<()> {
        self.stage(op).await
    }

    #[cfg(test)]
    pub fn prepare_pinned_input(
        &self,
        entry: crate::session::SessionEntry,
        agent: &eidetica::entry::ID,
    ) -> Result<PreparedPinnedInput> {
        let pin = self
            .read
            .agent_pin(&agent.to_string())?
            .ok_or_else(|| {
                ConversationError::Missing("Agent is not pinned in this Session".into())
            })?
            .0;
        super::turn::require_local_input(&entry)?;
        let mut op = self.new_append(vec![(
            Payload::Input {
                sender: entry.sender,
                text: entry.content,
            },
            View::Message(MessageRole::User),
        )])?;
        // A v1 grouping key is still required by the legacy view machinery;
        // it is derived, never a selectable execution lineage.
        op.branch_id = format!("conversation:{}", op.op_id);
        let accepted = AcceptedInput {
            fact: op.fact_id(0),
            branch: op.branch_id.clone(),
            attempt: op.op_id.clone(),
            pin: Some(pin.clone()),
        };
        Ok(PreparedPinnedInput {
            attempted: Default::default(),
            attempt: crate::session::TurnAttempt {
                attempt_id: accepted.attempt.clone(),
                request_id: crate::session::TurnRequestId::parse(accepted.request()),
                started_at: entry.timestamp,
                generation: 1,
                status: crate::session::TurnAttemptStatus::Started,
                completed_at: None,
            },
            op,
            accepted,
        })
    }

    #[cfg(test)]
    pub fn has_branch(&self, branch: &str) -> bool {
        self.read
            .operations
            .values()
            .any(|op| op.branch_id == branch)
    }

    #[cfg(test)]
    pub fn supports_compaction(&self) -> bool {
        self.read.fenced_compaction
    }

    pub fn compactions(&self) -> &[CompactionIntent] {
        &self.read.provenance.compactions
    }

    pub fn completed_turn_activity(
        &self,
        agent: &str,
        model: &str,
    ) -> crate::session::CompletedTurnActivity {
        use std::collections::BTreeMap;
        let evidence = &self.read.provenance;
        let mut latest_completion = None;
        let mut counts = BTreeMap::new();
        for call in &evidence.accounting {
            if call.agent == agent
                && let Some(at) = evidence.completed_at.get(&call.invocation)
            {
                // A newer return with unknown pricing cannot prove inactivity.
                latest_completion = latest_completion.max(Some(*at));
                if call
                    .call
                    .prices
                    .as_ref()
                    .is_some_and(|prices| prices.id == model)
                {
                    *counts.entry(&call.invocation).or_insert(0) += 1;
                }
            }
        }
        crate::session::CompletedTurnActivity {
            latest_completion,
            call_counts: counts.into_values().collect(),
        }
    }

    pub fn historical_members(
        &self,
        input: &RepresentationId,
    ) -> Result<Vec<(RuntimeMessage, RepresentationId)>> {
        let mut members = Vec::new();
        for group in self.context()?.groups {
            if group.representation.as_ref() == Some(input) {
                return Ok(members);
            }
            if let HistoryBody::Recorded { messages, .. } = group.body {
                members.extend(messages);
            }
        }
        Err(ConversationError::Missing(
            "accepted input in context selection".into(),
        ))
    }

    pub fn validate_compaction(&self, intent: &CompactionIntent) -> Result<()> {
        if !self.read.fenced_compaction || intent.observed != self.observed {
            return Err(ConversationError::InvalidScope(
                "compaction source binding".into(),
            ));
        }
        super::compaction::validate_selection(
            &self.read.effective()?,
            &intent.input,
            &intent.targets,
        )
    }

    pub fn compact_tool_summary(&self, tool: &ToolCallId) -> Option<String> {
        super::compaction::tool_summary(&self.read.operations, tool)
    }

    pub async fn replace_context(
        &self,
        write: &Transaction,
        intent: &CompactionIntent,
        summary: String,
    ) -> Result<Option<ContextReplacement>> {
        self.validate_compaction(intent)?;
        if summary.trim().is_empty() {
            return Err(ConversationError::Malformed(
                "empty compaction summary".into(),
            ));
        }
        let summary = if intent.tool.is_none() {
            let cited = self.read.cite_automatic_summary(intent, summary)?;
            let members: Vec<_> = self
                .historical_members(&intent.input)?
                .into_iter()
                .filter(|(_, id)| intent.targets.contains(id))
                .map(|(message, _)| message)
                .collect();
            let rendered = RuntimeMessage::User(view::historical_transformation(&cited));
            use crate::context::estimate_request_tokens;
            if estimate_request_tokens(&[rendered], &[]) >= estimate_request_tokens(&members, &[]) {
                return Ok(None);
            }
            cited
        } else {
            summary
        };
        let mut op = self.operation(OperationKind::Replace, intent.targets.clone())?;
        op.op_id.clone_from(&intent.id);
        op.facts.push(Fact {
            payload_version: 1,
            payload: Payload::TransformationOutput {
                text: summary.clone(),
            },
        });
        op.outputs.push(Representation {
            fact: op.fact_id(0),
            view: View::Summary,
            decoder_version: 1,
        });
        view::validate_at_observed(&op, &self.read.operations, &self.read.legacy)?;
        atom_in(&write.get_store::<ConversationStore>(STORE).await?, &op).await?;
        Ok(Some(ContextReplacement {
            targets: intent.targets.clone(),
            summary: op.representation_id(0),
            text: summary,
        }))
    }
}

#[cfg(test)]
pub(in crate::session) struct PreparedPinnedInput {
    attempted: std::sync::atomic::AtomicBool,
    pub op: Operation,
    pub accepted: AcceptedInput,
    pub attempt: crate::session::TurnAttempt,
}

#[cfg(test)]
impl PreparedPinnedInput {
    pub fn reserve_once(&self) -> bool {
        !self
            .attempted
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    pub async fn stage(&self, staged: &Staging<'_>) -> Result<()> {
        staged.stage(&self.op).await?;
        stage_observation(
            staged.txn,
            Observation::InputAccepted {
                input: self.accepted.clone(),
            },
        )
        .await?;
        staged
            .txn
            .get_store::<eidetica::store::Table<crate::session::TurnAttempt>>(
                crate::session::TURN_ATTEMPTS_STORE,
            )
            .await?
            .set(&self.attempt.attempt_id, self.attempt.clone())
            .await?;
        Ok(())
    }

    /// Query accepted history without executing or granting another attempt.
    pub async fn readback(
        &self,
        db: &Database,
        at: &Snapshot,
    ) -> Result<Option<eidetica::entry::ID>> {
        let txn = db.new_transaction_at(at).await?;
        let read = Box::pin(open(db, at, &txn)).await?;
        if let Some(error) = read.inspection.issues.first() {
            return Err(error.clone());
        }
        let conflict = || ConversationError::IdentityConflict("prepared input".into());
        let Some((entry, receipt)) = read.provenance.inputs.get(&self.accepted.request()) else {
            if read.operations.contains_key(&self.op.op_id) {
                return Err(conflict());
            }
            return Ok(None);
        };
        if receipt != &self.accepted
            || read.op_atoms.get(&self.op.op_id) != Some(&atom::key(&atom::encode(&self.op)?))
            || super::turn::attempt_at(
                db,
                &Snapshot::from([entry.clone()]),
                &self.attempt.attempt_id,
            )
            .await?
                != self.attempt
        {
            return Err(conflict());
        }
        Ok(Some(entry.clone()))
    }
}

/// Conflict preflight, not a CAS. A replacement racing this read can still
/// land on a sibling; signed joined readers refuse ambiguous effective context.
pub(in crate::session) async fn current_targets(
    db: &Database,
    targets: &[RepresentationId],
) -> Result<bool> {
    let at = db.snapshot().await?;
    let txn = db.new_transaction_at(&at).await?;
    let read = Box::pin(open(db, &at, &txn)).await?;
    if let Some(issue) = read.inspection.issues.first() {
        return Err(issue.clone());
    }
    Ok(!read
        .operations
        .values()
        .any(|op| op.targets.iter().any(|id| targets.contains(id))))
}
