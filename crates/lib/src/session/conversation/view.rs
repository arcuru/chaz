//! Pure interpretation. Nothing in this module can dispatch an effect.
use std::collections::{BTreeMap, BTreeSet};

use crate::runtime::RuntimeMessage;

use super::{ConversationError, Result, record::*};

type OrderKey = (u8, u64, String, u32, Vec<(u32, String)>);

pub(super) fn historical_transformation(text: &str) -> String {
    use crate::runtime::autocompact::{SUMMARY_PREFIX, SUMMARY_SUFFIX};
    format!("{SUMMARY_PREFIX}{text}{SUMMARY_SUFFIX}")
}

#[derive(Clone, Debug)]
pub(super) enum RepresentationSource {
    Conversation {
        representation: Representation,
        fact: Box<Fact>,
    },
    /// The payload still belongs to the pinned legacy row/native records.
    Legacy(Box<super::legacy::LegacyEntry>),
}

#[derive(Clone, Debug)]
pub(super) struct VisibleRepresentation {
    pub id: RepresentationId,
    pub source: RepresentationSource,
}

impl VisibleRepresentation {
    pub fn is_audit_only(&self) -> bool {
        matches!(&self.source, RepresentationSource::Conversation { representation, .. } if representation.view == View::AuditOnly)
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct ConversationView {
    pub representations: Vec<VisibleRepresentation>,
}

pub(super) fn validate_shape(op: &Operation) -> Result<()> {
    if op.version != 1 {
        return Err(ConversationError::Unsupported(format!(
            "operation v{}",
            op.version
        )));
    }
    let id = uuid::Uuid::parse_str(&op.op_id)
        .map_err(|_| ConversationError::Malformed("operation UUID".into()))?;
    if id.get_version_num() != 4 || id.to_string() != op.op_id || op.branch_id.is_empty() {
        return Err(ConversationError::Malformed("operation identity".into()));
    }
    if op.causal_seq == 0 || op.observed.frontier.is_empty() {
        return Err(ConversationError::InvalidScope(
            "empty causal binding".into(),
        ));
    }
    if (op.kind == OperationKind::Append && (op.outputs.is_empty() || !op.targets.is_empty()))
        || (op.kind == OperationKind::Replace && op.targets.is_empty())
        || op.targets.iter().collect::<BTreeSet<_>>().len() != op.targets.len()
        || u32::try_from(op.facts.len()).is_err()
        || u32::try_from(op.outputs.len()).is_err()
    {
        return Err(ConversationError::Malformed(
            "operation batch/targets".into(),
        ));
    }
    for fact in &op.facts {
        if fact.payload_version != 1 {
            return Err(ConversationError::Unsupported(
                "fact payload version".into(),
            ));
        }
        if let Payload::NativeModelResponse { response } = &fact.payload {
            if response.call.branch != op.branch_id
                || response.attempt.is_empty()
                || response.call.request.is_empty()
                || u32::try_from(response.tool_calls.len()).is_err()
            {
                return Err(ConversationError::Malformed("native call identity".into()));
            }
            if response
                .metadata
                .as_ref()
                .and_then(|m| m.usage.cost_usd)
                .is_some_and(|cost| !cost.is_finite())
            {
                return Err(ConversationError::Malformed(
                    "nonfinite reported cost".into(),
                ));
            }
        }
        if let Payload::ToolOutcome { execution, .. } = &fact.payload
            && execution.attempt.is_empty()
        {
            return Err(ConversationError::Malformed(
                "tool execution identity".into(),
            ));
        }
        if let Payload::ToolOutcome { outcome, .. } = &fact.payload
            && matches!(outcome, crate::runtime::ToolResultOutcome::Unknown { .. })
        {
            return Err(ConversationError::Unsupported(
                "tool outcome semantics".into(),
            ));
        }
    }
    for output in &op.outputs {
        if output.decoder_version != 1 {
            return Err(ConversationError::Unsupported(
                "representation decoder".into(),
            ));
        }
        if output.fact.database != op.observed.database {
            return Err(ConversationError::InvalidScope(
                "cross-database fact".into(),
            ));
        }
    }
    if op
        .targets
        .iter()
        .any(|id| id.database != op.observed.database)
    {
        return Err(ConversationError::InvalidScope(
            "cross-database target".into(),
        ));
    }
    Ok(())
}

pub(super) fn next_sequence(ops: &BTreeMap<String, Operation>) -> Result<u64> {
    ops.values()
        .map(|op| op.causal_seq)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| ConversationError::Malformed("causal sequence overflow".into()))
}

pub(super) fn validate_at_observed(
    op: &Operation,
    observed: &BTreeMap<String, Operation>,
    legacy: &super::legacy::LegacyView,
) -> Result<()> {
    validate_shape(op)?;
    if op.causal_seq != next_sequence(observed)? {
        return Err(ConversationError::InvalidScope(
            "causal sequence differs from selected frontier".into(),
        ));
    }
    let view = effective(observed, legacy)?;
    let active: BTreeSet<_> = view.representations.iter().map(|r| &r.id).collect();
    if op.targets.iter().any(|id| !active.contains(id)) {
        return Err(ConversationError::InvalidScope(
            "replacement target not active at observed frontier".into(),
        ));
    }
    for output in &op.outputs {
        if output.fact.operation != op.op_id && !observed.contains_key(&output.fact.operation) {
            return Err(ConversationError::Missing(
                "fact outside observed scope".into(),
            ));
        }
    }
    let mut candidate = observed.clone();
    if candidate.insert(op.op_id.clone(), op.clone()).is_some() {
        return Err(ConversationError::IdentityConflict(op.op_id.clone()));
    }
    // Check the native owner even for an audit-only result. A later
    // replacement or a different view cannot lend a result its call.
    for event in &op.facts {
        if let Payload::ToolOutcome { execution, .. } = &event.payload {
            let owner = fact(&execution.call.response, &candidate)?;
            if !matches!(&owner.payload, Payload::NativeModelResponse { response }
                if response.call.branch == op.branch_id
                    && response.attempt == execution.attempt
                    && (execution.call.index as usize) < response.tool_calls.len())
            {
                return Err(ConversationError::ExchangeConflict(
                    "orphan or mismatched native tool result".into(),
                ));
            }
        }
    }
    effective(&candidate, legacy)?;
    Ok(())
}

pub(super) fn fact<'a>(id: &FactId, ops: &'a BTreeMap<String, Operation>) -> Result<&'a Fact> {
    let op = ops
        .get(&id.operation)
        .ok_or_else(|| ConversationError::Missing("fact operation".into()))?;
    if id.database != op.observed.database {
        return Err(ConversationError::InvalidScope("fact database".into()));
    }
    op.facts
        .get(id.index as usize)
        .ok_or_else(|| ConversationError::Missing("fact index".into()))
}

fn compatible(view: View, payload: &Payload) -> bool {
    if let (View::Message(MessageRole::Assistant), Payload::NativeModelResponse { response }) =
        (view, payload)
    {
        return response.tool_calls.is_empty()
            && response
                .content
                .as_ref()
                .is_some_and(|text| !text.is_empty());
    }
    matches!(
        (view, payload),
        (View::AuditOnly, _)
            | (
                View::Message(_),
                Payload::Input { .. } | Payload::PublishedBody { .. }
            )
            | (View::Directive, Payload::Input { .. })
            | (
                View::Directive | View::Summary,
                Payload::TransformationOutput { .. }
            )
            | (View::NativeExchange, Payload::NativeModelResponse { .. })
            | (View::ToolResult, Payload::ToolOutcome { .. })
    )
}

pub(super) fn effective(
    ops: &BTreeMap<String, Operation>,
    legacy: &super::legacy::LegacyView,
) -> Result<ConversationView> {
    let mut representations = BTreeMap::new();
    let mut keys: BTreeMap<RepresentationId, OrderKey> = BTreeMap::new();
    for entry in &legacy.entries {
        let id = entry.anchor.reference.representation_id()?;
        let (epoch, ordinal, digest, index) = entry.anchor.base_key()?;
        keys.insert(id.clone(), (epoch, ordinal, digest, index, Vec::new()));
        representations.insert(id, RepresentationSource::Legacy(Box::new(entry.clone())));
    }
    let mut consumed = BTreeMap::new();
    for op in ops.values() {
        validate_shape(op)?;
        for (i, output) in op.outputs.iter().enumerate() {
            let payload = &fact(&output.fact, ops)?.payload;
            if !compatible(output.view, payload) {
                return Err(ConversationError::Malformed(
                    "incompatible fact/view".into(),
                ));
            }
            representations.insert(
                op.representation_id(i as u32),
                RepresentationSource::Conversation {
                    representation: output.clone(),
                    fact: Box::new(fact(&output.fact, ops)?.clone()),
                },
            );
        }
        for target in &op.targets {
            if consumed.insert(target, &op.op_id).is_some() {
                return Err(ConversationError::ReplacementConflict(target.clone()));
            }
        }
    }
    for target in consumed.keys() {
        if !representations.contains_key(*target) {
            return Err(ConversationError::Missing("replacement target".into()));
        }
    }
    // Iterate in causal order, not recursive target depth: a long valid
    // compaction chain cannot overflow the thread stack. Observed validation
    // additionally proves that each target was active in signed ancestry.
    let mut ordered: Vec<_> = ops.values().collect();
    ordered.sort_by(|a, b| (a.causal_seq, &a.op_id).cmp(&(b.causal_seq, &b.op_id)));
    for op in ordered {
        let anchor = if op.kind == OperationKind::Replace {
            Some(
                op.targets
                    .iter()
                    .map(|id| {
                        keys.get(id).cloned().ok_or_else(|| {
                            ConversationError::InvalidScope("noncausal replacement or cycle".into())
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .min()
                    .ok_or_else(|| ConversationError::Malformed("empty replacement".into()))?,
            )
        } else {
            None
        };
        for i in 0..op.outputs.len() {
            let key = match &anchor {
                Some(anchor) => {
                    let mut key = anchor.clone();
                    key.4.push((i as u32, op.op_id.clone()));
                    key
                }
                None => (1, op.causal_seq, op.op_id.clone(), i as u32, Vec::new()),
            };
            keys.insert(op.representation_id(i as u32), key);
        }
    }
    let mut visible: Vec<_> = representations
        .into_iter()
        .filter(|(id, _)| !consumed.contains_key(id))
        .map(|(id, source)| {
            let key = keys
                .get(&id)
                .ok_or_else(|| ConversationError::Missing("representation order".into()))?
                .clone();
            Ok((key, VisibleRepresentation { id, source }))
        })
        .collect::<Result<_>>()?;
    visible.sort_by(|a, b| a.0.cmp(&b.0));
    let view = ConversationView {
        representations: visible.into_iter().map(|(_, r)| r).collect(),
    };
    view.validate_exchanges(false)?;
    Ok(view)
}

impl ConversationView {
    pub(super) fn context_groups(&self) -> Result<Vec<crate::session::HistoryGroup>> {
        use crate::session::{EntryType, HistoryGroup, history::HistoryBody};
        self.validate_exchanges(true)?;
        let mut groups = Vec::new();
        let mut visible = self
            .representations
            .iter()
            .filter(|row| !row.is_audit_only());
        while let Some(first) = visible.next() {
            let mut unit = vec![first];
            let body = match &first.source {
                RepresentationSource::Legacy(row) => {
                    HistoryBody::Entry(Box::new(row.group.clone()))
                }
                RepresentationSource::Conversation {
                    representation,
                    fact,
                } => {
                    if representation.view == View::NativeExchange
                        && let Payload::NativeModelResponse { response } = &fact.payload
                    {
                        unit.extend(visible.by_ref().take(response.tool_calls.len()));
                    }
                    let kind = match representation.view {
                        View::Directive => EntryType::Directive,
                        View::Summary => EntryType::Summary,
                        View::NativeExchange => EntryType::TurnContext,
                        _ => EntryType::Message,
                    };
                    HistoryBody::Recorded {
                        kind,
                        messages: render_unit(&unit)?,
                    }
                }
            };
            groups.push(HistoryGroup {
                body,
                representation: Some(first.id.clone()),
            });
        }
        Ok(groups)
    }

    pub(super) fn display_rows(
        &self,
        reply_targets: &BTreeMap<String, RepresentationId>,
    ) -> Vec<crate::session::history::DisplayRow> {
        use crate::session::{EntryType, history::DisplayRow};
        let mut user_inputs = BTreeMap::new();
        let mut last_user = None;
        self.representations
            .iter()
            .filter_map(|row| {
                let (sender, content, kind) = match &row.source {
                    RepresentationSource::Legacy(row) => {
                        return Some(row.group.entry.clone().into());
                    }
                    RepresentationSource::Conversation {
                        representation,
                        fact,
                    } => match (&representation.view, &fact.payload) {
                        (View::Message(MessageRole::User), Payload::Input { sender, text }) => {
                            (sender.as_str(), text, EntryType::Message)
                        }
                        (View::Directive, Payload::Input { sender, text }) => {
                            (sender.as_str(), text, EntryType::Directive)
                        }
                        (
                            View::Message(role),
                            Payload::PublishedBody { text } | Payload::Input { text, .. },
                        ) => (
                            match role {
                                MessageRole::Assistant => "assistant",
                                MessageRole::User => "user",
                            },
                            text,
                            EntryType::Message,
                        ),
                        (
                            View::Message(MessageRole::Assistant),
                            Payload::NativeModelResponse { response },
                        ) => ("assistant", response.content.as_ref()?, EntryType::Message),
                        (View::Directive, Payload::TransformationOutput { text }) => {
                            ("history", text, EntryType::Directive)
                        }
                        (View::Summary, Payload::TransformationOutput { text }) => {
                            ("history", text, EntryType::Summary)
                        }
                        _ => return None, // Native observations are not publications.
                    },
                };
                let RepresentationSource::Conversation { representation, .. } = &row.source else {
                    unreachable!("legacy rows return above");
                };
                if representation.view == View::Message(MessageRole::User) {
                    user_inputs.insert(&row.id, content.as_str());
                    last_user = Some(&row.id);
                }
                let reply_to = reply_targets
                    .get(&row.id.operation)
                    .filter(|target| Some(*target) != last_user)
                    .and_then(|target| user_inputs.get(target))
                    .and_then(|text| preview(text));
                Some(DisplayRow {
                    sender: sender.into(),
                    content: content.clone(),
                    kind,
                    reply_to,
                })
            })
            .collect()
    }
    /// Inspection can contain a pending call. A model-bound context cannot.
    fn validate_exchanges(&self, completed: bool) -> Result<()> {
        let mut pending = BTreeSet::new();
        let mut calls = BTreeMap::new();
        let mut results = BTreeSet::new();
        let mut logical_calls = BTreeSet::new();
        for visible in &self.representations {
            let RepresentationSource::Conversation {
                representation,
                fact,
            } = &visible.source
            else {
                if !pending.is_empty() {
                    return Err(ConversationError::ExchangeConflict(
                        "legacy row splits tool exchange".into(),
                    ));
                }
                continue;
            };
            match (&representation.view, &fact.payload) {
                (View::NativeExchange, Payload::NativeModelResponse { response }) => {
                    if !pending.is_empty() || !logical_calls.insert(&response.call) {
                        return Err(ConversationError::ExchangeConflict(
                            "unfinished or duplicate model call".into(),
                        ));
                    }
                    let provider_ids: BTreeSet<_> =
                        response.tool_calls.iter().map(|call| &call.id).collect();
                    if provider_ids.len() != response.tool_calls.len() {
                        return Err(ConversationError::ExchangeConflict(
                            "duplicate provider call ID within one response".into(),
                        ));
                    }
                    for i in 0..response.tool_calls.len() {
                        let call = ToolCallId {
                            response: representation.fact.clone(),
                            index: i as u32,
                        };
                        if calls.insert(call.clone(), &response.attempt).is_some() {
                            return Err(ConversationError::ExchangeConflict(
                                "duplicate native exchange".into(),
                            ));
                        }
                        pending.insert(call);
                    }
                }
                (View::ToolResult, Payload::ToolOutcome { execution, .. }) => {
                    if calls.get(&execution.call) != Some(&&execution.attempt)
                        || !results.insert(execution.clone())
                        || pending.first() != Some(&execution.call)
                        || !pending.remove(&execution.call)
                    {
                        return Err(ConversationError::ExchangeConflict(
                            "orphan, duplicate or reordered tool result".into(),
                        ));
                    }
                }
                (View::AuditOnly, _) => {}
                _ if !pending.is_empty() => {
                    return Err(ConversationError::ExchangeConflict(
                        "split tool exchange".into(),
                    ));
                }
                _ => {}
            }
        }
        if completed && !pending.is_empty() {
            return Err(ConversationError::ExchangeConflict(
                "pending tool result".into(),
            ));
        }
        Ok(())
    }
}

// Only called on one unit after exchange validation. Render facts, not effects.
fn render_unit(unit: &[&VisibleRepresentation]) -> Result<Vec<(RuntimeMessage, RepresentationId)>> {
    let mut calls = BTreeMap::new();
    unit.iter()
        .map(|visible| {
            let RepresentationSource::Conversation {
                representation,
                fact,
            } = &visible.source
            else {
                return Err(ConversationError::InvalidScope(
                    "legacy row in native unit".into(),
                ));
            };
            let message =
                match (&representation.view, &fact.payload) {
                    (
                        View::Directive | View::Message(MessageRole::User),
                        Payload::Input { text, .. },
                    )
                    | (View::Message(MessageRole::User), Payload::PublishedBody { text }) => {
                        RuntimeMessage::User(text.clone())
                    }
                    (
                        View::Message(MessageRole::Assistant),
                        Payload::Input { text, .. } | Payload::PublishedBody { text },
                    ) => RuntimeMessage::Assistant(text.clone()),
                    (
                        View::Message(MessageRole::Assistant),
                        Payload::NativeModelResponse { response },
                    ) => RuntimeMessage::Assistant(response.content.clone().ok_or_else(|| {
                        ConversationError::Missing("published response body".into())
                    })?),
                    (View::Directive | View::Summary, Payload::TransformationOutput { text }) => {
                        // Stored transformation output never becomes system instructions.
                        RuntimeMessage::User(historical_transformation(text))
                    }
                    (View::NativeExchange, Payload::NativeModelResponse { response }) => {
                        let provider_extra = response.provider_extra.clone().into_iter().collect();
                        for (index, call) in response.tool_calls.iter().enumerate() {
                            calls.insert(
                                ToolCallId {
                                    response: representation.fact.clone(),
                                    index: index as u32,
                                },
                                call,
                            );
                        }
                        if response.tool_calls.is_empty() {
                            RuntimeMessage::AssistantWithExtra {
                                exchange_id: None,
                                content: response.content.clone(),
                                provider_extra,
                            }
                        } else {
                            RuntimeMessage::AssistantToolCalls {
                                exchange_id: None,
                                content: response.content.clone(),
                                tool_calls: response.tool_calls.clone(),
                                provider_extra,
                            }
                        }
                    }
                    (
                        View::ToolResult,
                        Payload::ToolOutcome {
                            execution, content, ..
                        },
                    ) => {
                        let call = calls
                            .get(&execution.call)
                            .ok_or_else(|| ConversationError::Missing("native tool call".into()))?;
                        RuntimeMessage::ToolResult {
                            call_id: call.id.clone(),
                            content: if call.name == crate::tool::NO_REPLY_TOOL {
                                content.clone()
                            } else {
                                crate::runtime::wrap_tool_output(
                                    &call.name,
                                    &crate::runtime::bound_tool_output(content),
                                )
                            },
                        }
                    }
                    _ => {
                        return Err(ConversationError::Malformed(
                            "unsupported context representation".into(),
                        ));
                    }
                };
            Ok((message, visible.id.clone()))
        })
        .collect()
}

fn preview(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut chars = line.chars().filter(|c| !c.is_control());
    let mut quote: String = chars.by_ref().take(24).collect();
    if chars.next().is_some() || text.contains('\n') {
        quote.push('…');
    }
    (!quote.is_empty()).then_some(quote)
}
