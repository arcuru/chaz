//! Input and completion bindings reuse the host's attempt row. Neither a
//! receipt nor a readable branch grants an executor another attempt.
use super::{ConversationError as Error, ConversationRead, Result, record::*};
use crate::session::{TURN_ATTEMPTS_STORE, TurnAttempt, TurnAttemptStatus};
use eidetica::{Database, Snapshot, entry::Entry, store::Table};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::session) struct AcceptedInput {
    pub fact: FactId,
    pub branch: String,
    pub attempt: String,
}

impl AcceptedInput {
    pub(in crate::session) fn request(&self) -> String {
        format!("conversation:{}", self.fact.operation)
    }

    pub(in crate::session) fn representation(&self) -> RepresentationId {
        RepresentationId {
            database: self.fact.database.clone(),
            operation: self.fact.operation.clone(),
            index: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::session) struct Completion {
    pub response: FactId,
    pub publication: String,
    pub attempt: String,
}

/// Irreducible inputs of this narrow writer; no raw request or authority.
#[derive(Serialize, Deserialize)]
#[serde(tag = "source", content = "value", deny_unknown_fields)]
pub(in crate::session) enum AcceptedValue {
    Instruction(String),
    BeforeAgent(String),
    Tool(crate::tool::ToolDefinition),
}

pub(super) fn operation_in<'a>(
    read: &'a ConversationRead,
    entry: &Entry,
    id: &str,
) -> Result<&'a Operation> {
    if read
        .locations
        .get(entry.id_ref())
        .is_some_and(|ids| ids.contains(id))
    {
        Ok(&read.operations[id])
    } else {
        Err(Error::Missing("operation not in the receipt Entry".into()))
    }
}

/// Require the other half of the atomic binding too: inspecting a signed
/// receipt is not enough if a writer can omit it and leave a visible fact.
pub(super) fn validate_operations<'a>(
    read: &ConversationRead,
    inputs: impl Iterator<Item = &'a AcceptedInput>,
    responses: &BTreeSet<FactId>,
    publications: &BTreeSet<&str>,
    accepted_values: &BTreeSet<FactId>,
) -> Result<()> {
    let mut branches = BTreeMap::new();
    for input in inputs {
        if branches.insert(&input.branch, input).is_some() {
            return Err(Error::IdentityConflict("fenced execution branch".into()));
        }
    }
    for op in read.operations.values() {
        if read.fenced_turn {
            let input = branches.get(&op.branch_id).ok_or_else(|| {
                Error::Integrity("operation outside a fenced execution branch".into())
            })?;
            if publications.contains(op.op_id.as_str()) {
                continue;
            }
            if op.kind != OperationKind::Append
                || op.facts.is_empty()
                || op.facts.len() != op.outputs.len()
                || op
                    .outputs
                    .iter()
                    .enumerate()
                    .any(|(i, out)| out.fact != op.fact_id(i as u32))
            {
                return Err(Error::Integrity(
                    "fenced operation lacks atomic publication or append binding".into(),
                ));
            }
            for (i, (fact, output)) in op.facts.iter().zip(&op.outputs).enumerate() {
                let valid = match &fact.payload {
                    Payload::Input { .. } => output.fact == input.fact,
                    Payload::NativeModelResponse { .. } => responses.contains(&output.fact),
                    Payload::ToolOutcome {
                        execution,
                        executed_arguments,
                        ..
                    } => {
                        let owner = super::view::fact(&execution.call.response, &read.operations)?;
                        output.view == View::ToolResult
                            && execution.ordinal == 0
                            && responses.contains(&execution.call.response)
                            && matches!(&owner.payload, Payload::NativeModelResponse { response }
                                if response.tool_calls.get(execution.call.index as usize)
                                    .is_some_and(|call| call.arguments == *executed_arguments))
                    }
                    Payload::ControlObservation { .. } => {
                        output.view == View::AuditOnly && accepted_values.contains(&output.fact)
                    }
                    _ => false,
                };
                if !valid {
                    return Err(Error::Integrity(format!(
                        "unbound fenced fact {}:{i}",
                        op.op_id
                    )));
                }
            }
        } else if op.outputs.iter().any(|out| {
            out.view == View::Message(MessageRole::Assistant)
                && matches!(
                    super::view::fact(&out.fact, &read.operations),
                    Ok(Fact {
                        payload: Payload::NativeModelResponse { .. },
                        ..
                    })
                )
        }) {
            return Err(Error::Unsupported(
                "native publication without fenced turn capability".into(),
            ));
        }
    }
    Ok(())
}

async fn attempt_at(db: &Database, at: &Snapshot, id: &str) -> Result<TurnAttempt> {
    Ok(db
        .new_transaction_at(at)
        .await?
        .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
        .await?
        .get(id)
        .await?)
}

pub(super) async fn validate_input(
    db: &Database,
    read: &ConversationRead,
    entry: &Entry,
    input: &AcceptedInput,
) -> Result<()> {
    let op = operation_in(read, entry, &input.fact.operation)?;
    let valid_op = op.kind == OperationKind::Append
        && op.branch_id == input.branch
        && input.fact == op.fact_id(0)
        && matches!(&op.facts[..], [Fact { payload: Payload::Input { sender, text }, .. }] if !sender.is_empty() && !text.is_empty())
        && matches!(&op.outputs[..], [output] if output.fact == input.fact && output.view == View::Message(MessageRole::User));
    if !valid_op || !entry.in_subtree(TURN_ATTEMPTS_STORE) {
        return Err(Error::Integrity(
            "input lacks its atomic fact or attempt".into(),
        ));
    }
    let attempt = attempt_at(db, &Snapshot::from([entry.id()]), &input.attempt).await?;
    if attempt.attempt_id != input.attempt
        || attempt.request_id.as_str() != input.request()
        || attempt.status != TurnAttemptStatus::Started
        || attempt.completed_at.is_some()
        || attempt.generation != 1
    {
        return Err(Error::Integrity(
            "input attempt differs from its receipt".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_publication(
    op: &Operation,
    response: &FactId,
    native: &NativeResponse,
) -> Result<()> {
    let view = View::Message(MessageRole::Assistant);
    let unchanged = op.facts.is_empty()
        && matches!(&op.outputs[..], [out] if out.fact == *response && out.view == view);
    let edited = matches!(&op.facts[..], [Fact { payload: Payload::PublishedBody { text }, .. }]
        if !text.is_empty() && Some(text) != native.content.as_ref())
        && matches!(&op.outputs[..], [out] if out.fact == op.fact_id(0) && out.view == view);
    let source = RepresentationId {
        database: response.database.clone(),
        operation: response.operation.clone(),
        index: 0,
    };
    if !native.tool_calls.is_empty()
        || native.content.as_ref().is_none_or(String::is_empty)
        || op.branch_id != native.call.branch
        || op.kind != OperationKind::Replace
        || op.targets != [source]
        || !(unchanged || edited)
    {
        return Err(Error::Integrity(
            "publication differs from its final response".into(),
        ));
    }
    Ok(())
}

pub(super) async fn validate_completion(
    db: &Database,
    read: &ConversationRead,
    entry: &Entry,
    completion: &Completion,
) -> Result<()> {
    let op = operation_in(read, entry, &completion.publication)?;
    let Payload::NativeModelResponse { response } =
        &super::view::fact(&completion.response, &read.operations)?.payload
    else {
        return Err(Error::Integrity(
            "completion lacks its native response".into(),
        ));
    };
    validate_publication(op, &completion.response, response)?;
    if response.attempt != completion.attempt || !entry.in_subtree(TURN_ATTEMPTS_STORE) {
        return Err(Error::Integrity(
            "publication lacks its atomic completion".into(),
        ));
    }
    let parents = Snapshot::from(entry.parents()?);
    let before = attempt_at(db, &parents, &completion.attempt).await?;
    let after = attempt_at(db, &Snapshot::from([entry.id()]), &completion.attempt).await?;
    if before.attempt_id != completion.attempt
        || before.request_id.as_str() != response.call.request
        || before.status != TurnAttemptStatus::Started
        || before.completed_at.is_some()
        || after.completed_at.is_none()
        || after
            != (TurnAttempt {
                status: TurnAttemptStatus::Completed,
                completed_at: after.completed_at,
                ..before
            })
    {
        return Err(Error::Integrity("invalid atomic attempt transition".into()));
    }
    Ok(())
}
