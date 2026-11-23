//! Signed source selection and settlement for a context replacement. No policy,
//! provider invocation or recovery authority lives in this module.
use super::{
    Ancestry, ConversationError as Error, ConversationRead, Result, record::*, turn, view,
};
use crate::session::history::{FoldedMessage, HistoryBody};
use eidetica::{Snapshot, entry::Entry};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const REFERENCE_KIND: &str = "cr1:";

fn fold_key(database: &str, fold: &str) -> String {
    // The fold ID binds the ordered signed targets; the database prevents a
    // same-ID fold in another session from accepting this citation.
    blake3::hash(format!("{database}:{fold}").as_bytes())
        .to_hex()
        .to_string()
}

impl ConversationRead {
    fn covered_messages<'a>(&'a self, targets: &'a [RepresentationId]) -> Result<Vec<&'a FactId>> {
        let mut pending: Vec<_> = targets.iter().rev().collect();
        let mut seen = BTreeSet::new();
        let mut messages = Vec::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(op) = self.operations.get(&id.operation) else {
                continue; // Imported legacy anchors are not native message facts.
            };
            let out = op
                .outputs
                .get(id.index as usize)
                .ok_or_else(|| Error::Missing("covered representation".into()))?;
            match out.view {
                View::Message(_) | View::NativeExchange => messages.push(&out.fact),
                View::Summary if self.provenance.applied_compactions.contains(&op.op_id) => {
                    pending.extend(op.targets.iter().rev());
                }
                _ => {}
            }
        }
        Ok(messages)
    }

    pub(super) fn cite_automatic_summary(
        &self,
        intent: &Intent,
        mut text: String,
    ) -> Result<String> {
        let count = self.covered_messages(&intent.targets)?.len();
        if count > 0 {
            let key = fold_key(&intent.input.database, &intent.id);
            text.push_str(&crate::session::fold_citation(&key, count - 1));
        }
        Ok(text)
    }

    pub(in crate::session) fn folded_message(
        &self,
        database: &str,
        reference: &str,
    ) -> Result<Option<FoldedMessage>> {
        if let Some(error) = self.inspection.issues.first() {
            return Err(error.clone());
        }
        let Some((key, slot)) = reference
            .strip_prefix(REFERENCE_KIND)
            .and_then(|r| r.split_once(':'))
        else {
            return Ok(None);
        };
        if key.len() != 64
            || !key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Ok(None);
        }
        let Some(index) = slot.parse::<usize>().ok().filter(|n| n.to_string() == slot) else {
            return Ok(None);
        };
        // Only an applied and validated fold in this verified frontier can
        // name its targets. An arbitrary operation ID is not a history grant.
        let Some(fold) = self
            .provenance
            .applied_compactions
            .iter()
            .find(|id| fold_key(database, id) == key)
        else {
            return Ok(None);
        };
        let Some(id) = self
            .covered_messages(&self.operations[fold].targets)?
            .get(index)
            .copied()
        else {
            return Ok(None);
        };
        let tools = match &view::fact(id, &self.operations)?.payload {
            Payload::NativeModelResponse { response } => response
                .tool_calls
                .iter()
                .map(|call| call.name.clone())
                .collect(),
            _ => Vec::new(),
        };
        // Select the original JSON span, not a re-serialization: whitespace,
        // escaping, native tool arguments and opaque fields stay byte exact.
        #[derive(Deserialize)]
        struct RawFacts<'a> {
            #[serde(borrow)]
            facts: Vec<&'a serde_json::value::RawValue>,
        }
        let atom = &self.inspection.operations.atoms[&self.op_atoms[&id.operation]];
        let raw: RawFacts<'_> = serde_json::from_str(atom)?;
        Ok(Some(FoldedMessage {
            bytes: raw
                .facts
                .get(id.index as usize)
                .ok_or_else(|| Error::Integrity("original fact span missing".into()))?
                .get()
                .to_owned(),
            tools,
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::session) struct Intent {
    /// Also the replacement operation id, if applied.
    pub id: String,
    pub input: RepresentationId,
    pub observed: ObservedBinding,
    pub targets: Vec<RepresentationId>,
    pub tool: Option<ToolCallId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::session) enum Settlement {
    Applied,
    Unapplied,
    Stale,
}

pub(super) fn validate_selection(
    selected: &view::ConversationView,
    input: &RepresentationId,
    targets: &[RepresentationId],
) -> Result<()> {
    let mut rest = targets;
    for group in selected.context_groups()? {
        if group.representation.as_ref() == Some(input) {
            if !targets.is_empty() && rest.is_empty() {
                return Ok(());
            }
            break;
        }
        if group.representation.as_ref() != rest.first() || rest.is_empty() {
            continue;
        }
        if let HistoryBody::Recorded { messages, .. } = &group.body
            && messages
                .iter()
                .map(|(_, id)| id)
                .eq(rest.iter().take(messages.len()))
        {
            rest = &rest[messages.len()..];
        } else {
            break;
        }
    }
    Err(Error::InvalidScope(
        "compaction must select ordered whole historical units before accepted input".into(),
    ))
}

impl Intent {
    pub(super) fn validate(
        &self,
        read: &ConversationRead,
        entry: &Entry,
        ancestry: &Ancestry,
        input_entry: &Entry,
        accepted: &turn::AcceptedInput,
        calls: &BTreeMap<(CallId, String), super::CallInput>,
    ) -> Result<()> {
        let parents = ancestry.prefix(&Snapshot::from(entry.parents()?))?;
        let id_ok = uuid::Uuid::parse_str(&self.id)
            .is_ok_and(|id| id.get_version_num() == 4 && id.to_string() == self.id);
        if !read.fenced_compaction
            || !id_ok
            || self.input != accepted.representation()
            || self.observed.database != self.input.database
            || self.observed.frontier.is_empty()
            || !self
                .observed
                .frontier
                .tips()
                .iter()
                .all(|id| parents.contains(id))
        {
            return Err(Error::InvalidScope("compaction intent binding".into()));
        }
        let observed = ancestry.prefix(&self.observed.frontier)?;
        if !observed.contains(input_entry.id_ref()) {
            return Err(Error::InvalidScope(
                "compaction source does not observe input".into(),
            ));
        }
        let ops = read.operations_in(&observed);
        validate_selection(
            &view::effective(&ops, &read.legacy)?,
            &self.input,
            &self.targets,
        )?;
        if let Some(tool) = &self.tool {
            let response = native_tool(&read.operations, tool)?;
            if response.call.request != accepted.request()
                || response.call.branch != accepted.branch
                || response.attempt != accepted.attempt
                || !read
                    .locations
                    .iter()
                    .any(|(id, ops)| parents.contains(id) && ops.contains(&tool.response.operation))
                || calls
                    .get(&(response.call.clone(), response.attempt.clone()))
                    .is_none_or(|call| {
                        self.observed != call.observed
                            || self
                                .targets
                                .iter()
                                .any(|id| !call.representations.contains(id))
                    })
            {
                return Err(Error::InvalidScope(
                    "compaction not selected for this tool call".into(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_applied(
        &self,
        read: &ConversationRead,
        entry: &Entry,
        branch: &str,
    ) -> Result<()> {
        let op = turn::operation_in(read, entry, &self.id)?;
        if op.kind != OperationKind::Replace
            || op.branch_id != branch
            || op.observed != self.observed
            || op.targets != self.targets
            || !matches!(&op.outputs[..], [out] if out.view == View::Summary && out.fact == op.fact_id(0))
        {
            return Err(Error::Integrity(
                "replacement differs from compaction intent".into(),
            ));
        }
        let [
            Fact {
                payload: Payload::TransformationOutput { text },
                ..
            },
        ] = &op.facts[..]
        else {
            return Err(Error::Integrity(
                "compaction requires one summary fact".into(),
            ));
        };
        if text.trim().is_empty()
            || self.tool.as_ref().is_some_and(|tool| {
                tool_summary(&read.operations, tool).as_deref() != Some(text.as_str())
            })
        {
            return Err(Error::Integrity("invalid compaction summary".into()));
        }
        Ok(())
    }
}

fn native_tool<'a>(
    ops: &'a BTreeMap<String, Operation>,
    tool: &ToolCallId,
) -> Result<&'a NativeResponse> {
    match &view::fact(&tool.response, ops)?.payload {
        Payload::NativeModelResponse { response }
            if response
                .tool_calls
                .get(tool.index as usize)
                .is_some_and(|call| call.name == "compact") =>
        {
            Ok(response)
        }
        _ => Err(Error::InvalidScope(
            "compaction requires its native compact call".into(),
        )),
    }
}

pub(super) fn tool_summary(ops: &BTreeMap<String, Operation>, tool: &ToolCallId) -> Option<String> {
    let response = native_tool(ops, tool).ok()?;
    let args: serde_json::Value =
        serde_json::from_str(&response.tool_calls[tool.index as usize].arguments).ok()?;
    args.get("summary")?.as_str().map(str::to_owned)
}

pub(super) type Intents<'a> = BTreeMap<String, (&'a Entry, Intent)>;
pub(super) type Settlements<'a> = BTreeMap<String, (&'a Entry, Settlement)>;

pub(super) fn validate<'a>(
    read: &ConversationRead,
    ancestry: &Ancestry,
    inputs: &BTreeMap<String, (&Entry, turn::AcceptedInput)>,
    completed: &BTreeMap<&str, &Entry>,
    calls: &BTreeMap<(CallId, String), super::CallInput>,
    intents: &'a Intents<'_>,
    settlements: &'a Settlements<'_>,
) -> Result<BTreeSet<&'a str>> {
    let branches: BTreeMap<_, _> = inputs
        .values()
        .map(|(_, input)| (input.branch.as_str(), input.attempt.as_str()))
        .collect();
    let mut applied = BTreeSet::new();
    let mut tools = BTreeSet::new();
    for (id, (entry, intent)) in intents {
        let request = format!("conversation:{}", intent.input.operation);
        let (input_entry, input) = inputs
            .get(&request)
            .ok_or_else(|| Error::Missing("compaction input".into()))?;
        intent.validate(read, entry, ancestry, input_entry, input, calls)?;
        let observed = ancestry.prefix(&intent.observed.frontier)?;
        if intent.targets.iter().any(|target| {
            let branch = read
                .operations
                .get(&target.operation)
                .map(|op| op.branch_id.as_str());
            branch
                .and_then(|branch| branches.get(branch))
                .and_then(|attempt| completed.get(attempt))
                .is_none_or(|completion| !observed.contains(completion.id_ref()))
        }) {
            return Err(Error::InvalidScope(
                "compaction selected unfinished history".into(),
            ));
        }
        if let Some(done) = completed.get(input.attempt.as_str()) {
            let parents = ancestry.prefix(&Snapshot::from(entry.parents()?))?;
            if parents.contains(done.id_ref()) {
                return Err(Error::InvalidScope(
                    "compaction intent after turn completion".into(),
                ));
            }
        }

        if let Some(tool) = &intent.tool
            && !tools.insert(tool)
        {
            return Err(Error::IdentityConflict("compaction tool invocation".into()));
        }
        if intent.tool.is_some()
            && settlements
                .get(id)
                .is_none_or(|(outcome, _)| outcome.id_ref() != entry.id_ref())
        {
            return Err(Error::Integrity(
                "compact tool intent and outcome must commit together".into(),
            ));
        }
    }
    for (id, (entry, status)) in settlements {
        let (source, intent) = intents
            .get(id)
            .ok_or_else(|| Error::Missing("compaction intent".into()))?;
        if intent.tool.is_none()
            && !ancestry
                .prefix(&Snapshot::from(entry.parents()?))?
                .contains(source.id_ref())
        {
            return Err(Error::InvalidScope(
                "compaction outcome outside intent ancestry".into(),
            ));
        }
        if *status == Settlement::Applied {
            let branch = &read.operations[&intent.input.operation].branch_id;
            intent.validate_applied(read, entry, branch)?;
            applied.insert(id.as_str());
        }
    }
    Ok(applied)
}
