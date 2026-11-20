//! Call preparation evidence, pinned to signed ancestry. No replay authority.
use super::{Ancestry, ConversationError as Error, Result, atom, record::*, view};
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
    /// Ordered model-facing slots. Older synthetic preparations had only the
    /// selection lists; absence remains explicitly approximate, not guessed.
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
pub(super) enum Observation {
    #[serde(rename = "CallPrepared")]
    Prepared { input: Box<CallInput> },
    #[serde(rename = "CallDispatchIntent")]
    DispatchIntent { call: CallId, attempt: String },
    #[serde(rename = "CallOutcome")]
    Outcome {
        call: CallId,
        attempt: String,
        status: CallStatus,
        response: Option<FactId>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub version: u32,
    pub observation: Observation,
}

impl Observation {
    fn identity(&self) -> (&CallId, &str, u8) {
        match self {
            Self::Prepared { input } => (&input.call, &input.attempt, 0),
            Self::DispatchIntent { call, attempt } => (call, attempt, 1),
            Self::Outcome { call, attempt, .. } => (call, attempt, 2),
        }
    }
}

pub(super) async fn read(
    db: &Database,
    frontier: &Snapshot,
    txn: &Transaction,
    conversation: &super::ConversationRead,
    ancestry: &Ancestry,
) -> Result<(super::Originals, BTreeMap<(CallId, String), CallInput>)> {
    let entries = super::entries(db, STORE, frontier).await?;
    let mut originals = super::Originals::default();
    let mut records = BTreeMap::new();
    for entry in &entries {
        for (hash, text) in originals.delta(entry, STORE)? {
            let record = match atom::decode_v1::<Record>(&text) {
                Ok(record) => record,
                Err(error) => {
                    originals.issues.push(error);
                    continue;
                }
            };
            let (call, attempt, kind) = record.observation.identity();
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
    let mut prepared = BTreeMap::new();
    if !originals.issues.is_empty() || !conversation.inspection.issues.is_empty() {
        return Ok((originals, prepared));
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
                    validate_input(db, &parents, input, conversation, ancestry)
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
                    status, response, ..
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
                        validate_outcome(db, entry, call, attempt, *status, response.as_ref())
                    }
                }
            };
            if let Err(error) = check {
                originals.issues.push(error);
            }
        }
        if let Observation::Prepared { input } = &record.observation {
            prepared.insert((call.clone(), attempt.clone()), input.as_ref().clone());
        }
    }
    Ok((originals, prepared))
}

fn validate_input(
    db: &Database,
    parents: &BTreeSet<eidetica::entry::ID>,
    input: &CallInput,
    conversation: &super::ConversationRead,
    ancestry: &Ancestry,
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
    selected.validate_selection(&input.representations)?;
    // This reader can inspect original accepted facts. Projection/step outputs
    // staged with preparation belong to the later fenced writer integration.
    if input
        .accepted_additions
        .iter()
        .any(|id| view::fact(id, &ops).is_err())
        || input
            .tools
            .iter()
            .any(|id| !input.accepted_additions.contains(id))
        || input.messages.iter().any(|item| match item {
            CallMessageBinding::Representation(id) => !input.representations.contains(id),
            CallMessageBinding::AcceptedAddition(id) => !input.accepted_additions.contains(id),
        })
    {
        return Err(Error::InvalidScope(
            "call slot outside original selected input".into(),
        ));
    }
    Ok(())
}

fn validate_outcome(
    db: &Database,
    entry: &eidetica::entry::Entry,
    call: &CallId,
    attempt: &str,
    status: CallStatus,
    response: Option<&FactId>,
) -> Result<()> {
    let Some(id) = response else {
        return if status == CallStatus::Responded {
            Err(Error::Missing("call response fact".into()))
        } else {
            Ok(())
        };
    };
    if id.database != db.root_id().to_string() || !entry.in_subtree(super::STORE) {
        return Err(Error::Integrity("response is not in outcome Entry".into()));
    }
    let atoms = atom::atoms(&atom::decode_doc(entry.data(super::STORE)?)?)?;
    for text in atoms.values() {
        let op: Operation = atom::decode_v1(text)?;
        if op.op_id != id.operation {
            continue;
        }
        let expected_view = if status == CallStatus::Responded {
            View::NativeExchange
        } else {
            View::AuditOnly
        };
        if op.branch_id == call.branch
            && op.kind == OperationKind::Append
            && id.index == 0
            && op.outputs.len() == 1
            && op.outputs[0].fact == *id
            && op.outputs[0].view == expected_view
            && matches!(&op.facts[..], [Fact { payload: Payload::NativeModelResponse { response }, .. }] if &response.call == call && response.attempt == attempt)
            && !matches!(status, CallStatus::Failed | CallStatus::Uncertain)
        {
            return Ok(());
        }
    }
    Err(Error::Integrity(
        "call outcome differs from its atomic response".into(),
    ))
}

pub(super) struct ProvenanceStore(pub(super) eidetica::store::DocStore);
super::doc_store!(ProvenanceStore, "chaz/conversation-provenance:v1");
