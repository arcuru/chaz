//! Staging for the synthetic writer. The session host supplies and commits
//! each transaction; the Store never executes a turn or an effect.
#[cfg(test)]
use super::contract;
use super::{
    ConversationError, ConversationRead, ConversationStore, ReadAtoms, Result, STORE, atom, open,
    provenance, view,
};
use crate::session::SessionContextView;
use eidetica::{Database, Snapshot, Transaction};
use serde::Serialize;

use super::record::{
    Fact, FactId, ObservedBinding, Operation, OperationKind, Representation, RepresentationId,
};
#[cfg(test)]
pub(in crate::session) use super::turn::AcceptedInput;
pub(in crate::session) use super::{
    provenance::{Accounting, CallStatus, Observation},
    record::{MessageRole, NativeResponse, Payload, ToolCallId, ToolExecutionId, View},
    turn::{AcceptedValue, Completion},
};

async fn atom_in(store: &impl ReadAtoms, value: &impl Serialize) -> Result<()> {
    let text = atom::encode(value)?;
    store.doc().set(atom::key(&text), text).await?;
    Ok(())
}

#[cfg(test)]
pub(in crate::session) async fn stage_descriptor(
    txn: &Transaction,
    db: &Database,
    at: &Snapshot,
) -> Result<()> {
    atom_in(
        &txn.get_store::<contract::ContractStore>(contract::STORE)
            .await?,
        &contract::Descriptor {
            version: 1,
            encoding: 1,
            context_semantics: 1,
            renderer: 1,
            required_operations: [
                "append-v1".into(),
                "replace-v1".into(),
                contract::FENCED_TURN.into(),
            ]
            .into(),
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
        let read = Box::pin(open(db, at, txn)).await?;
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
        self.stage(&op).await?;
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

    pub fn context(&self) -> Result<SessionContextView> {
        self.read.context()
    }

    async fn stage(&self, op: &Operation) -> Result<()> {
        view::validate_at_observed(op, &self.read.operations, &self.read.legacy)?;
        atom_in(&self.txn.get_store::<ConversationStore>(STORE).await?, op).await
    }
}
