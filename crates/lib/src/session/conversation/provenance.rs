//! Call preparation evidence, pinned to signed ancestry. No replay authority.
use super::{
    Ancestry, ConversationError as Error, Result, atom, compaction, record::*, turn, view,
};
use crate::session::usage::{CallEvidence, CallObservation};
use eidetica::{Database, Snapshot, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const STORE: &str = "conversation_provenance_v1";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRef {
    pub database: String,
    pub frontier: Snapshot,
    pub store: String,
    pub record_digest: String,
    pub logical_id: String,
    pub member: Option<u32>,
    pub format: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fidelity {
    Approximate,
    Unavailable,
}

/// No Exact constructor: this initial capture has not established a per-call
/// backend-byte reconstruction contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallInput {
    pub call: CallId,
    pub attempt: String,
    pub observed: ObservedBinding,
    pub representations: Vec<RepresentationId>,
    pub renderer_version: u32,
    pub context_version: u32,
    pub requested_model: String,
    pub api: String,
    pub effective_nonsecret_settings: BTreeMap<String, serde_json::Value>,
    pub sources: Vec<RecordRef>,
    pub accepted_additions: Vec<FactId>,
    /// Ordered model-facing slots. A nonempty list binds the selected history
    /// in order, with additions allowed only between intact history groups.
    /// Older synthetic preparations omit these slots and remain approximate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<CallMessageBinding>,
    /// Selected declarations are accepted inputs, not permission to execute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<FactId>,
    pub fidelity: Fidelity,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum CallMessageBinding {
    Representation(RepresentationId),
    AcceptedAddition(FactId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallStatus {
    Responded,
    Failed,
    Cancelled,
    Stale,
    ClaimLost,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(in crate::session) enum Observation {
    CompactionIntent {
        intent: compaction::Intent,
    },
    CompactionSettled {
        id: String,
        status: compaction::Settlement,
    },
    InputAccepted {
        input: turn::AcceptedInput,
    },
    TurnCompleted {
        completion: turn::Completion,
    },
    #[serde(rename = "CallPrepared")]
    Prepared {
        input: Box<CallInput>,
    },
    #[serde(rename = "CallDispatchIntent")]
    DispatchIntent {
        call: CallId,
        attempt: String,
    },
    #[serde(rename = "CallOutcome")]
    Outcome {
        call: CallId,
        attempt: String,
        status: CallStatus,
        response: Option<FactId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accounting: Option<Box<Accounting>>,
    },
}

/// Only local estimates/rates live here. Reported usage and opaque provider
/// data stay on the native response fact in the same signed Entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::session) struct Accounting {
    pub agent: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub request_budget_tokens: Option<usize>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub prices: Option<crate::backends::ModelInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub version: u32,
    pub observation: Observation,
}

impl Observation {
    fn call_key(&self) -> Option<(&CallId, &str, u8)> {
        Some(match self {
            Self::InputAccepted { .. }
            | Self::TurnCompleted { .. }
            | Self::CompactionIntent { .. }
            | Self::CompactionSettled { .. } => return None,
            Self::Prepared { input } => (&input.call, &input.attempt, 0),
            Self::DispatchIntent { call, attempt } => (call, attempt, 1),
            Self::Outcome { call, attempt, .. } => (call, attempt, 2),
        })
    }
}

#[derive(Default)]
pub(super) struct Evidence {
    pub calls: BTreeMap<(CallId, String), CallInput>,
    pub accounting: Vec<CallEvidence>,
    pub compactions: Vec<compaction::Intent>,
}

pub(super) async fn read(
    db: &Database,
    frontier: &Snapshot,
    txn: &Transaction,
    conversation: &super::ConversationRead,
    ancestry: &Ancestry,
) -> Result<(super::Originals, Evidence)> {
    let entries = super::entries(db, STORE, frontier).await?;
    let mut originals = super::Originals::default();
    let mut records = BTreeMap::new();
    let mut inputs = BTreeMap::new();
    let mut completions = BTreeMap::new();
    let mut intents = BTreeMap::new();
    let mut settlements = BTreeMap::new();
    for entry in &entries {
        for (hash, text) in originals.delta(entry, STORE)? {
            let record = match atom::decode_v1::<Record>(&text) {
                Ok(record) => record,
                Err(error) => {
                    originals.issues.push(error);
                    continue;
                }
            };
            let Some((call, attempt, kind)) = record.observation.call_key() else {
                let conflict = match record.observation {
                    Observation::CompactionIntent { intent } => {
                        intents.insert(intent.id.clone(), (entry, intent)).is_some()
                    }
                    Observation::CompactionSettled { id, status } => {
                        settlements.insert(id, (entry, status)).is_some()
                    }
                    Observation::InputAccepted { input } => {
                        inputs.insert(input.request(), (entry, input)).is_some()
                    }
                    Observation::TurnCompleted { completion } => completions
                        .insert(completion.attempt.clone(), (entry, completion))
                        .is_some(),
                    _ => unreachable!(),
                };
                if conflict {
                    originals
                        .issues
                        .push(Error::IdentityConflict("turn receipt".into()));
                }
                continue;
            };
            let key = (call.clone(), attempt.to_owned(), kind);
            let (prior_hash, locations, _) = records
                .entry(key)
                .or_insert_with(|| (hash.clone(), Vec::new(), record.clone()));
            if prior_hash != &hash {
                originals
                    .issues
                    .push(Error::IdentityConflict("call provenance".into()));
            } else {
                locations.push(entry);
            }
        }
    }
    if let Err(error) = originals
        .check_materialized::<ProvenanceStore>(txn, STORE)
        .await
    {
        originals.issues.push(error);
    }
    let mut evidence = Evidence::default();
    if !originals.issues.is_empty() || !conversation.inspection.issues.is_empty() {
        return Ok((originals, evidence));
    }
    let fenced = conversation.fenced_turn;
    if (fenced
        && (!conversation.fenced_compaction && inputs.len() > 1
            || inputs.is_empty() != conversation.operations.is_empty()))
        || (!fenced && (!inputs.is_empty() || !completions.is_empty()))
    {
        originals.issues.push(Error::Integrity(
            "fenced turn and accepted input differ from descriptor".into(),
        ));
        return Ok((originals, evidence));
    }
    let mut responses = BTreeSet::new();
    let mut accounted = BTreeSet::new();
    let mut accepted_values = BTreeSet::new();
    for (entry, input) in inputs.values() {
        if let Err(error) = Box::pin(turn::validate_input(db, conversation, entry, input)).await {
            originals.issues.push(error);
        }
    }
    for ((call, attempt, kind), (_, locations, record)) in &records {
        for entry in locations {
            let parents = ancestry.prefix(&Snapshot::from(entry.parents()?))?;
            let earlier = |kind| {
                records
                    .get(&(call.clone(), attempt.clone(), kind))
                    .is_some_and(|(_, locations, _)| {
                        locations.iter().any(|e| parents.contains(e.id_ref()))
                    })
            };
            if earlier(*kind) {
                continue;
            } // byte-identical reimport, original already checked
            let check = match &record.observation {
                Observation::Prepared { input } => {
                    let receipt = inputs.get(&call.request);
                    if fenced
                        && receipt.is_none_or(|(owner, accepted)| {
                            !parents.contains(owner.id_ref())
                                || accepted.attempt != *attempt
                                || accepted.branch != call.branch
                                || !input.representations.contains(&accepted.representation())
                        })
                    {
                        Err(Error::InvalidScope(
                            "call is not bound to the accepted turn".into(),
                        ))
                    } else {
                        let result = validate_input(
                            db,
                            entry,
                            &parents,
                            input,
                            conversation,
                            ancestry,
                            fenced,
                        );
                        if fenced && result.is_ok() {
                            accepted_values.extend(
                                input
                                    .accepted_additions
                                    .iter()
                                    .filter(|id| {
                                        turn::operation_in(conversation, entry, &id.operation)
                                            .is_ok()
                                    })
                                    .cloned(),
                            );
                        }
                        result
                    }
                }
                Observation::DispatchIntent { .. } => {
                    if earlier(0) && !earlier(2) {
                        Ok(())
                    } else {
                        Err(Error::Integrity(
                            "dispatch requires earlier preparation, not an outcome".into(),
                        ))
                    }
                }
                Observation::Outcome {
                    status,
                    response,
                    accounting,
                    ..
                } => {
                    if !earlier(0)
                        || ((response.is_some()
                            || matches!(status, CallStatus::Failed | CallStatus::Uncertain))
                            && !earlier(1))
                    {
                        Err(Error::Integrity(
                            "outcome lacks original call evidence".into(),
                        ))
                    } else {
                        let input =
                            match &records[&(call.clone(), attempt.clone(), 0)].2.observation {
                                Observation::Prepared { input } => input,
                                _ => unreachable!(),
                            };
                        validate_outcome(
                            conversation,
                            entry,
                            call,
                            attempt,
                            *status,
                            response.as_ref(),
                        )
                        .and_then(|native| {
                            if (fenced && *status == CallStatus::Responded && accounting.is_none())
                                || (!fenced && accounting.is_some())
                            {
                                return Err(Error::Missing("atomic call accounting".into()));
                            }
                            if let Some(accounting) = accounting {
                                let Some(native) = native.filter(|_| {
                                    *status == CallStatus::Responded && !accounting.agent.is_empty()
                                }) else {
                                    return Err(Error::Integrity(
                                        "accounting lacks its observed response".into(),
                                    ));
                                };
                                // The same atom can be imported by multiple branches.
                                responses.extend(response.iter().cloned());
                                if accounted.insert((call, attempt)) {
                                    evidence.accounting.push(CallEvidence {
                                        local_estimate: true,
                                        invocation: attempt.clone(),
                                        branch: Some(call.branch.clone()),
                                        agent: accounting.agent.clone(),
                                        call: CallObservation {
                                            model_sequence: call.model_sequence,
                                            timestamp: accounting.timestamp,
                                            requested_model: input.requested_model.clone(),
                                            request_budget_tokens: accounting.request_budget_tokens,
                                            input_tokens: accounting.input_tokens,
                                            output_tokens: Some(accounting.output_tokens),
                                            prices: accounting.prices.clone(),
                                            metadata: native.metadata.clone(),
                                            responded: true,
                                        },
                                    });
                                }
                            }
                            Ok(())
                        })
                    }
                }
                Observation::InputAccepted { .. }
                | Observation::TurnCompleted { .. }
                | Observation::CompactionIntent { .. }
                | Observation::CompactionSettled { .. } => {
                    unreachable!()
                }
            };
            if let Err(error) = check {
                originals.issues.push(error);
            }
        }
        if let Observation::Prepared { input } = &record.observation {
            evidence
                .calls
                .insert((call.clone(), attempt.clone()), input.as_ref().clone());
        }
    }
    let mut publications = BTreeSet::new();
    let mut completed = BTreeMap::new();
    for (entry, completion) in completions.values() {
        let check = async {
            Box::pin(turn::validate_completion(
                db,
                conversation,
                entry,
                completion,
            ))
            .await?;
            let Payload::NativeModelResponse { response } =
                &view::fact(&completion.response, &conversation.operations)?.payload
            else {
                return Err(Error::Missing("completion response".into()));
            };
            if !inputs.contains_key(&response.call.request)
                || !responses.contains(&completion.response)
            {
                return Err(Error::Integrity(
                    "completion lacks its accepted input or response outcome".into(),
                ));
            }
            Ok(())
        }
        .await;
        if let Err(error) = check {
            originals.issues.push(error);
        } else {
            publications.insert(completion.publication.as_str());
            completed.insert(completion.attempt.as_str(), *entry);
        }
    }
    match compaction::validate(
        conversation,
        ancestry,
        &inputs,
        &completed,
        &evidence.calls,
        &intents,
        &settlements,
    ) {
        Ok(applied) => publications.extend(applied),
        Err(error) => originals.issues.push(error),
    }
    evidence.compactions = intents.values().map(|(_, intent)| intent.clone()).collect();
    if let Err(error) = turn::validate_operations(
        conversation,
        inputs.values().map(|(_, input)| input),
        &responses,
        &publications,
        &accepted_values,
    ) {
        originals.issues.push(error);
    }
    Ok((originals, evidence))
}

fn validate_input(
    db: &Database,
    entry: &eidetica::entry::Entry,
    parents: &BTreeSet<eidetica::entry::ID>,
    input: &CallInput,
    conversation: &super::ConversationRead,
    ancestry: &Ancestry,
    fenced: bool,
) -> Result<()> {
    if input.observed.database != db.root_id().to_string()
        || input.observed.frontier.is_empty()
        || !input
            .observed
            .frontier
            .tips()
            .iter()
            .all(|id| parents.contains(id))
    {
        return Err(Error::InvalidScope(
            "call input outside signed ancestry".into(),
        ));
    }
    if input.renderer_version != 1 || input.context_version != 1 {
        return Err(Error::Unsupported("call renderer/context version".into()));
    }
    if input.call.request.is_empty()
        || input.call.branch.is_empty()
        || input.attempt.is_empty()
        || input.requested_model.is_empty()
        || input.api.is_empty()
        || input.reasons.is_empty()
    {
        return Err(Error::Integrity("incomplete call input binding".into()));
    }
    if input.sources.iter().any(|source| {
        source.database.is_empty()
            || source.frontier.is_empty()
            || source.store.is_empty()
            || source.logical_id.is_empty()
            || source.format != 1
            || source.record_digest.len() != 64
            || !source
                .record_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(Error::Integrity("invalid selected source reference".into()));
    }
    let observed = ancestry.prefix(&input.observed.frontier)?;
    conversation.require_descriptor(&observed)?;
    let ops = conversation.operations_in(&observed);
    let selected = view::effective(&ops, &conversation.legacy)?;
    validate_selection(&selected, &input.representations, &input.messages)?;
    // An accepted addition belongs to the observed prefix or (for this
    // fenced attempt) to the exact Entry that first accepts the request.
    let accepted = |id: &FactId| {
        view::fact(id, &ops).or_else(|err| {
            if !fenced {
                return Err(err);
            }
            let op = turn::operation_in(conversation, entry, &id.operation)?;
            if op.observed != input.observed
                || op.branch_id != input.call.branch
                || op.kind != OperationKind::Append
                || !op.outputs.iter().all(|out| out.view == View::AuditOnly)
            {
                return Err(Error::InvalidScope(
                    "call addition lacks its atomic binding".into(),
                ));
            }
            view::fact(id, &conversation.operations)
        })
    };
    if input
        .accepted_additions
        .iter()
        .any(|id| accepted(id).is_err())
        || input
            .tools
            .iter()
            .any(|id| !input.accepted_additions.contains(id))
        || input.messages.iter().any(|slot| {
            matches!(slot, CallMessageBinding::AcceptedAddition(id) if !input.accepted_additions.contains(id))
        })
    {
        return Err(Error::InvalidScope(
            "call slot outside original selected input".into(),
        ));
    }
    if fenced {
        if input.api != "chat-completions" || !input.sources.is_empty() {
            return Err(Error::Unsupported("fenced call input source or API".into()));
        }
        let slots = input
            .messages
            .iter()
            .filter_map(|slot| match slot {
                CallMessageBinding::AcceptedAddition(id) => Some((id, true)),
                _ => None,
            })
            .chain(input.tools.iter().map(|id| (id, false)));
        let mut used = BTreeSet::new();
        for (id, is_message) in slots {
            let Payload::ControlObservation { value } = &accepted(id)?.payload else {
                return Err(Error::Integrity("invalid accepted request value".into()));
            };
            let valid = match serde_json::from_value::<turn::AcceptedValue>(value.clone())? {
                turn::AcceptedValue::Instruction(_) | turn::AcceptedValue::BeforeAgent(_) => {
                    is_message
                }
                turn::AcceptedValue::Tool(tool) => !is_message && !tool.name.is_empty(),
            };
            if !used.insert(id) || !valid {
                return Err(Error::Integrity("invalid accepted request slot".into()));
            }
        }
        if input.messages.is_empty()
            || used.len() != input.accepted_additions.len()
            || input.accepted_additions.iter().collect::<BTreeSet<_>>() != used
        {
            return Err(Error::Integrity("incomplete accepted request slots".into()));
        }
    }
    Ok(())
}

pub(super) fn validate_selection(
    selected: &view::ConversationView,
    mut ids: &[RepresentationId],
    messages: &[CallMessageBinding],
) -> Result<()> {
    use crate::session::history::HistoryBody;
    let mut slots = messages.iter().map(representation_slot);
    for group in selected.context_groups()? {
        let Some(head) = ids.first() else { break };
        if group.representation.as_ref() != Some(head) {
            continue;
        }
        let len = match &group.body {
            HistoryBody::Entry(_) => 1,
            HistoryBody::Recorded { messages, .. } => {
                if !messages
                    .iter()
                    .map(|(_, id)| id)
                    .eq(ids.iter().take(messages.len()))
                {
                    return Err(Error::ExchangeConflict(
                        "call selects part of a native exchange".into(),
                    ));
                }
                messages.len()
            }
        };
        let (unit, rest) = ids.split_at(len);
        ids = rest;
        // Skip additions only before a unit, never inside it.
        if !messages.is_empty()
            && !slots
                .by_ref()
                .skip_while(Option::is_none)
                .take(len)
                .eq(unit.iter().map(Some))
        {
            return Err(Error::InvalidScope(
                "call message slots differ from ordered selection or split a native exchange"
                    .into(),
            ));
        }
    }
    if !ids.is_empty() {
        return Err(Error::InvalidScope(
            "call representations missing, duplicated or reordered at observed frontier".into(),
        ));
    }
    if slots.any(|slot| slot.is_some()) {
        return Err(Error::InvalidScope(
            "call message slots extend beyond selected representations".into(),
        ));
    }
    Ok(())
}

fn representation_slot(slot: &CallMessageBinding) -> Option<&RepresentationId> {
    match slot {
        CallMessageBinding::Representation(id) => Some(id),
        CallMessageBinding::AcceptedAddition(_) => None,
    }
}

fn validate_outcome<'a>(
    read: &'a super::ConversationRead,
    entry: &eidetica::entry::Entry,
    call: &CallId,
    attempt: &str,
    status: CallStatus,
    response: Option<&FactId>,
) -> Result<Option<&'a NativeResponse>> {
    let Some(id) = response else {
        return if status == CallStatus::Responded {
            Err(Error::Missing("call response fact".into()))
        } else {
            Ok(None)
        };
    };
    let op = turn::operation_in(read, entry, &id.operation)?;
    let expected_view = if status == CallStatus::Responded {
        View::NativeExchange
    } else {
        View::AuditOnly
    };
    if op.branch_id == call.branch
        && op.kind == OperationKind::Append
        && *id == op.fact_id(0)
        && matches!(&op.outputs[..], [output] if output.fact == *id && output.view == expected_view)
        && !matches!(status, CallStatus::Failed | CallStatus::Uncertain)
        && let [
            Fact {
                payload: Payload::NativeModelResponse { response },
                ..
            },
        ] = &op.facts[..]
        && &response.call == call
        && response.attempt == attempt
    {
        return Ok(Some(response));
    }
    Err(Error::Integrity(
        "call outcome differs from its atomic response".into(),
    ))
}

pub(super) struct ProvenanceStore(pub(super) eidetica::store::DocStore);
super::doc_store!(ProvenanceStore, "chaz/conversation-provenance:v1");
