//! Per-model-call context projection.
//!
//! Before every logical model call the runtime derives a fresh request
//! baseline — the assembled context plus the completed exchanges of the
//! current attempt — and hands it through the operator-granted chain of
//! [`ContextProjector`] endpoints. The accepted result is what the backend
//! receives for that call (and for every transport retry of it); the
//! authoritative message vector, the session transcript and the recorder
//! stream are never edited.
//!
//! # Authority
//!
//! A projector runs only when the operator lists it in `context_projection`
//! ([`ContextProjectionGrant`]). Installation, activation, or a manifest
//! declaration alone is not a grant. The grant carries one of two
//! authorities:
//!
//! * [`ProjectionAuthority::Conversation`] — may rewrite, drop, or add
//!   conversation messages. The System messages and tool declarations must
//!   come back unchanged.
//! * [`ProjectionAuthority::FullContext`] — may also rewrite the
//!   instructions and the model-facing tool declarations. It may drop or
//!   re-describe declarations, but never declare a tool the call does not
//!   already expose. Actual tool execution is checked against the turn's
//!   scoped tools regardless of what the model was shown.
//!
//! Every authority keeps tool-exchange integrity: a call group is retained
//! whole (assistant call message immediately followed by one result per call,
//! in order) or omitted whole; ids, names, arguments and opaque provider data
//! are byte-identical to the baseline; groups never repeat or reorder; and
//! exchanges completed in the current attempt are never dropped. Result
//! bodies and other message text may change. Retained assistant output without
//! calls has the same immutable provider data, order and current-attempt checks.
//!
//! # Order and failure
//!
//! Conversation grants run first, then full-context grants; within a phase
//! the configured list order holds. Each step sees the previous accepted
//! step's output and is validated before it is accepted. A required step
//! that is missing, inactive, errors, panics, times out, or returns an
//! invalid request stops the call — no request is sent. An optional step
//! that fails is skipped and its input (including every earlier accepted
//! edit) carries on. A configured optional projector that is absent or
//! inactive is never invoked. The final request must still fit the budget;
//! there is no fallback past that gate.
//!
//! This is an API guardrail for operator-trusted in-process extensions, not
//! a sandbox: an instance can still use handles it captured at construction.

use crate::extension::caps::CapFuture;
use crate::runtime::{RuntimeMessage, ToolCallRequest};
use crate::tool::ToolDefinition;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// Upper bound on one projector invocation. A projector is on the critical
/// path of every model call; a stalled one counts as failed.
pub(crate) const PROJECTOR_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of the model-facing request a granted projector may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionAuthority {
    /// Conversation messages only; instructions and tools stay fixed.
    #[default]
    Conversation,
    /// The whole model-facing request, including instructions and the
    /// model-facing tool declarations.
    FullContext,
}

impl ProjectionAuthority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::FullContext => "full_context",
        }
    }

    fn phase(self) -> u8 {
        match self {
            Self::Conversation => 0,
            Self::FullContext => 1,
        }
    }
}

/// One operator grant from the `context_projection` config list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextProjectionGrant {
    /// Extension name (its manifest name).
    pub extension: String,
    #[serde(default)]
    pub authority: ProjectionAuthority,
    /// A required projector that cannot run stops the model call.
    #[serde(default)]
    pub required: bool,
}

/// Reject grant lists the runtime could not apply unambiguously.
pub fn validate_grants(grants: &[ContextProjectionGrant]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for grant in grants {
        if grant.extension.trim().is_empty() {
            return Err("context_projection entry has an empty extension name".into());
        }
        if !seen.insert(grant.extension.as_str()) {
            return Err(format!(
                "context_projection lists extension '{}' more than once",
                grant.extension
            ));
        }
    }
    Ok(())
}

/// The deterministic invocation order: conversation phase, then
/// full-context phase, configured list order within each phase.
pub(crate) fn invocation_order(grants: &[ContextProjectionGrant]) -> Vec<ContextProjectionGrant> {
    let mut ordered = grants.to_vec();
    ordered.sort_by_key(|grant| grant.authority.phase());
    ordered
}

/// Read-only identity of a baseline message: where the host derived it from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextSource {
    /// Host instructions: system prompt, augmentation, invocation prompt.
    Instructions,
    /// Host-derived timeline metadata, not access to the entry's body.
    DateBoundary {
        entry_index: usize,
        date: chrono::NaiveDate,
    },
    /// A session entry selected into context, by its position in the
    /// session's entry list at assembly time.
    SessionEntry {
        index: usize,
        sender: String,
        timestamp: DateTime<Utc>,
    },
    /// Stable Eidetica row identity when supplied by coherent reconstruction.
    PersistedSessionEntry { id: String },
    /// Intentional committed custom context, identified by the host.
    DurableContribution {
        source_id: String,
        extension: String,
    },
    /// A completed native tool exchange replayed from an earlier turn's
    /// transcript.
    ReplayedExchange {
        request_id: String,
        attempt_id: String,
        model_sequence: u64,
    },
    /// Extension context-tail text appended after the conversation.
    ContextTail,
    /// A `before_agent_start` injection for this turn.
    TurnInjection,
    /// Messages completed or accepted at a boundary of the current attempt.
    CurrentAttempt { model_sequence: u64 },
    /// A caller supplied no provenance for this message.
    Unattributed,
}

/// The model-facing request for one logical model call.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedRequest {
    pub cache: crate::cache::CacheOptions,
    pub messages: Vec<RuntimeMessage>,
    pub tools: Vec<ToolDefinition>,
}

impl ProjectedRequest {
    /// Refresh occurrence identities on a host-created baseline, never on a
    /// projector's output. Identities stay with their groups through Vec edits.
    pub(crate) fn identify_exchanges(&mut self) {
        for (index, message) in self.messages.iter_mut().enumerate() {
            if let RuntimeMessage::AssistantToolCalls { exchange_id, .. }
            | RuntimeMessage::AssistantWithExtra { exchange_id, .. } = message
            {
                *exchange_id = Some(index);
            }
        }
    }
}

/// What a projector sees about the call it is shaping. Everything here is
/// read-only; the projector's only output is the returned request.
pub struct ProjectionCall<'a> {
    pub agent_name: &'a str,
    pub session_db_id: Option<&'a str>,
    pub request_id: Option<&'a str>,
    pub attempt_id: Option<&'a str>,
    /// The resolved model name the request is for.
    pub model: &'a str,
    /// Zero-based logical model call within this attempt.
    pub model_round: u64,
    pub authority: ProjectionAuthority,
    pub required: bool,
    /// The unprojected baseline for this round.
    pub baseline: &'a ProjectedRequest,
    /// Provenance of `baseline.messages`, index-aligned.
    pub sources: &'a [ContextSource],
    /// This extension's current persisted settings; read errors fail this step.
    pub settings: &'a serde_json::Value,
    /// Host-scoped retained source reads. No ambient database/execute handle.
    pub history: Option<&'a super::history::ScopedHistory>,
    /// Estimated-token ceiling the final request must fit, when known.
    pub budget_tokens: Option<usize>,
}

/// Request-local context projection endpoint, published from
/// [`crate::extension::ExtensionInstance::context_projector`].
pub trait ContextProjector: Send + Sync {
    /// Return the request to send in place of `request` (the previous
    /// accepted step's output). Returning `request` unchanged is a no-op.
    fn project<'a>(
        &'a self,
        call: &'a ProjectionCall<'a>,
        request: ProjectedRequest,
    ) -> CapFuture<'a, ProjectedRequest>;
}

/// A granted, active projector resolved for one model call.
#[derive(Clone)]
pub(crate) struct ResolvedProjector {
    pub extension: String,
    pub authority: ProjectionAuthority,
    pub required: bool,
    pub projector: Arc<dyn ContextProjector>,
}

/// Why a projected request cannot be dispatched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionViolation {
    #[error("the request has no messages")]
    Empty,
    #[error("invalid cache options: {0}")]
    Cache(String),
    #[error("an assistant tool-call message has no calls")]
    EmptyCallGroup,
    #[error("conversation authority cannot change the instructions")]
    InstructionsChanged,
    #[error("conversation authority cannot change the tool declarations")]
    ToolsChanged,
    #[error("tool '{0}' is not exposed to this call")]
    UngrantedTool(String),
    #[error("tool '{0}' is declared more than once")]
    DuplicateTool(String),
    #[error("tool call '{0}' was never made by the model")]
    ForgedCall(String),
    #[error("tool call '{0}' was altered")]
    AlteredCall(String),
    #[error("retained provider data for tool call '{0}' was altered")]
    AlteredProviderEcho(String),
    #[error("tool exchange '{0}' was repeated or reordered")]
    ReorderedExchange(String),
    #[error("tool exchange '{0}' is incomplete")]
    HalfExchange(String),
    #[error("current-attempt tool exchange '{0}' was dropped")]
    DroppedCurrentExchange(String),
    #[error("retained assistant output {0}")]
    InvalidAssistantOutput(&'static str),
    #[error("the request is ~{estimated} tokens, over the {budget}-token budget")]
    OverBudget { estimated: usize, budget: usize },
}

/// A model call that projection refused to dispatch.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionError {
    #[error("required context projector '{extension}' is unavailable: {reason}")]
    RequiredUnavailable { extension: String, reason: String },
    #[error("required context projector '{extension}' failed: {reason}")]
    RequiredFailed { extension: String, reason: String },
    #[error("context projection cannot dispatch the request: {0}")]
    Undispatchable(ProjectionViolation),
}

/// Read-only inputs for one round of the chain.
pub(crate) struct RoundInputs<'a> {
    pub agent_name: &'a str,
    pub session_db_id: Option<&'a str>,
    pub request_id: Option<&'a str>,
    pub attempt_id: Option<&'a str>,
    pub model: &'a str,
    pub model_round: u64,
    pub sources: &'a [ContextSource],
    /// Baseline message indices belonging to the current attempt. Provider
    /// call IDs are not occurrence identities and may legally repeat.
    pub current_exchanges: &'a HashSet<usize>,
    pub budget_tokens: Option<usize>,
    pub session_db: Option<&'a eidetica::Database>,
    pub history: Option<&'a super::history::ScopedHistory>,
}

/// Run the resolved chain over `baseline` and return the accepted request.
pub(crate) async fn run_chain(
    chain: &[ResolvedProjector],
    baseline: &ProjectedRequest,
    inputs: &RoundInputs<'_>,
) -> Result<ProjectedRequest, ProjectionError> {
    let mut current = baseline.clone();
    for step in chain {
        let input = current.clone();
        let invoked = tokio::time::timeout(
            PROJECTOR_TIMEOUT,
            AssertUnwindSafe(async {
                let settings = match inputs.session_db {
                    Some(db) => super::read_settings_checked(db, &step.extension).await?,
                    None => serde_json::json!({}),
                };
                let call = ProjectionCall {
                    agent_name: inputs.agent_name,
                    session_db_id: inputs.session_db_id,
                    request_id: inputs.request_id,
                    attempt_id: inputs.attempt_id,
                    model: inputs.model,
                    model_round: inputs.model_round,
                    authority: step.authority,
                    required: step.required,
                    baseline,
                    sources: inputs.sources,
                    budget_tokens: inputs.budget_tokens,
                    settings: &settings,
                    history: inputs.history,
                };
                step.projector.project(&call, input).await
            })
            .catch_unwind(),
        )
        .await;
        let outcome = match invoked {
            Err(_) => Err(format!(
                "timed out after {} seconds",
                PROJECTOR_TIMEOUT.as_secs()
            )),
            Ok(Err(_)) => Err("panicked".to_string()),
            Ok(Ok(Err(error))) => Err(error.to_string()),
            Ok(Ok(Ok(candidate))) => validate_step(
                baseline,
                &current,
                &candidate,
                step.authority,
                inputs.current_exchanges,
                inputs.budget_tokens,
            )
            .map(|()| candidate)
            .map_err(|violation| format!("invalid output: {violation}")),
        };
        match outcome {
            Ok(candidate) => current = candidate,
            Err(reason) if step.required => {
                return Err(ProjectionError::RequiredFailed {
                    extension: step.extension.clone(),
                    reason,
                });
            }
            Err(reason) => warn!(
                extension = %step.extension,
                model_round = inputs.model_round,
                %reason,
                "Optional context projector failed; keeping its validated input"
            ),
        }
    }
    // The fallback is not exempt: whatever survives must fit the budget.
    check_budget(&current, inputs.budget_tokens).map_err(ProjectionError::Undispatchable)?;
    Ok(current)
}

fn check_budget(
    request: &ProjectedRequest,
    budget: Option<usize>,
) -> Result<(), ProjectionViolation> {
    if request.messages.is_empty() {
        return Err(ProjectionViolation::Empty);
    }
    request
        .cache
        .validate(&request.messages)
        .map_err(|e| ProjectionViolation::Cache(e.to_string()))?;
    let Some(budget) = budget else {
        return Ok(());
    };
    let estimated = crate::context::estimate_request_tokens(&request.messages, &request.tools);
    if estimated > budget {
        return Err(ProjectionViolation::OverBudget { estimated, budget });
    }
    Ok(())
}

fn system_messages(messages: &[RuntimeMessage]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|message| match message {
            RuntimeMessage::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

struct CallGroup<'a> {
    calls: &'a [ToolCallRequest],
    provider_extra: &'a serde_json::Map<String, serde_json::Value>,
}

/// Validate one step's candidate against its input and the round baseline.
pub(crate) fn validate_step(
    baseline: &ProjectedRequest,
    input: &ProjectedRequest,
    candidate: &ProjectedRequest,
    authority: ProjectionAuthority,
    current_exchanges: &HashSet<usize>,
    budget: Option<usize>,
) -> Result<(), ProjectionViolation> {
    if candidate.messages.is_empty() {
        return Err(ProjectionViolation::Empty);
    }

    match authority {
        ProjectionAuthority::Conversation => {
            if system_messages(&candidate.messages) != system_messages(&input.messages) {
                return Err(ProjectionViolation::InstructionsChanged);
            }
            if candidate.tools != input.tools {
                return Err(ProjectionViolation::ToolsChanged);
            }
        }
        ProjectionAuthority::FullContext => {
            let exposed: HashSet<&str> = baseline.tools.iter().map(|t| t.name.as_str()).collect();
            let mut declared = HashSet::new();
            for tool in &candidate.tools {
                if !exposed.contains(tool.name.as_str()) {
                    return Err(ProjectionViolation::UngrantedTool(tool.name.clone()));
                }
                if !declared.insert(tool.name.as_str()) {
                    return Err(ProjectionViolation::DuplicateTool(tool.name.clone()));
                }
            }
        }
    }

    // Index host occurrences, not provider IDs. Equal calls in separate model
    // responses must remain distinguishable even after their bodies change.
    let mut groups: HashMap<usize, CallGroup<'_>> = HashMap::new();
    let mut result_ids: HashSet<&str> = HashSet::new();
    for (index, message) in baseline.messages.iter().enumerate() {
        match message {
            RuntimeMessage::AssistantToolCalls {
                tool_calls,
                provider_extra,
                ..
            } if !tool_calls.is_empty() => {
                groups.insert(
                    index,
                    CallGroup {
                        calls: tool_calls,
                        provider_extra,
                    },
                );
            }
            RuntimeMessage::ToolResult { call_id, .. } => {
                result_ids.insert(call_id.as_str());
            }
            _ => {}
        }
    }

    let mut seen: HashSet<usize> = HashSet::new();
    let mut last_order: Option<usize> = None;
    let mut i = 0;
    while i < candidate.messages.len() {
        match &candidate.messages[i] {
            RuntimeMessage::AssistantWithExtra {
                exchange_id,
                provider_extra,
                ..
            } => {
                let Some(index) = exchange_id else {
                    return Err(ProjectionViolation::InvalidAssistantOutput(
                        "has no host occurrence",
                    ));
                };
                let Some(RuntimeMessage::AssistantWithExtra {
                    provider_extra: original,
                    ..
                }) = baseline.messages.get(*index)
                else {
                    return Err(ProjectionViolation::InvalidAssistantOutput(
                        "was never made by the model",
                    ));
                };
                if provider_extra != original {
                    return Err(ProjectionViolation::InvalidAssistantOutput("was altered"));
                }
                if !seen.insert(*index) || last_order.is_some_and(|last| *index <= last) {
                    return Err(ProjectionViolation::InvalidAssistantOutput(
                        "was repeated or reordered",
                    ));
                }
                last_order = Some(*index);
            }
            RuntimeMessage::AssistantToolCalls { tool_calls, .. } if tool_calls.is_empty() => {
                return Err(ProjectionViolation::EmptyCallGroup);
            }
            RuntimeMessage::AssistantToolCalls {
                exchange_id,
                tool_calls,
                provider_extra,
                ..
            } if !tool_calls.is_empty() => {
                let first = tool_calls[0].id.as_str();
                let Some(index) = exchange_id else {
                    return Err(ProjectionViolation::ForgedCall(first.to_string()));
                };
                let Some(group) = groups.get(index) else {
                    return Err(ProjectionViolation::ForgedCall(first.to_string()));
                };
                if tool_calls.len() > group.calls.len() {
                    return Err(ProjectionViolation::ForgedCall(
                        tool_calls[group.calls.len()].id.clone(),
                    ));
                }
                if tool_calls.len() < group.calls.len() {
                    return Err(ProjectionViolation::HalfExchange(first.to_string()));
                }
                for (made, sent) in group.calls.iter().zip(tool_calls) {
                    if made.id != sent.id {
                        return Err(ProjectionViolation::ForgedCall(sent.id.clone()));
                    }
                    if made != sent {
                        return Err(ProjectionViolation::AlteredCall(sent.id.clone()));
                    }
                }
                if provider_extra != group.provider_extra {
                    return Err(ProjectionViolation::AlteredProviderEcho(first.to_string()));
                }
                if !seen.insert(*index) || last_order.is_some_and(|last| *index <= last) {
                    return Err(ProjectionViolation::ReorderedExchange(first.to_string()));
                }
                last_order = Some(*index);
                for (offset, call) in group.calls.iter().enumerate() {
                    match candidate.messages.get(i + 1 + offset) {
                        Some(RuntimeMessage::ToolResult { call_id, .. }) if call_id == &call.id => {
                        }
                        _ => return Err(ProjectionViolation::HalfExchange(call.id.clone())),
                    }
                }
                i += 1 + group.calls.len();
                continue;
            }
            RuntimeMessage::ToolResult { call_id, .. } => {
                return Err(if result_ids.contains(call_id.as_str()) {
                    ProjectionViolation::HalfExchange(call_id.clone())
                } else {
                    ProjectionViolation::ForgedCall(call_id.clone())
                });
            }
            _ => {}
        }
        i += 1;
    }

    if current_exchanges.iter().any(|index| {
        matches!(
            baseline.messages.get(*index),
            Some(RuntimeMessage::AssistantWithExtra { .. })
        ) && !seen.contains(index)
    }) {
        return Err(ProjectionViolation::InvalidAssistantOutput(
            "from the current attempt was dropped",
        ));
    }

    if let Some((_, group)) = groups
        .iter()
        .filter(|(index, _)| current_exchanges.contains(index) && !seen.contains(index))
        .min_by_key(|(index, _)| **index)
    {
        return Err(ProjectionViolation::DroppedCurrentExchange(
            group.calls[0].id.clone(),
        ));
    }

    check_budget(candidate, budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> ToolCallRequest {
        ToolCallRequest {
            id: id.into(),
            name: "echo".into(),
            arguments: "{}".into(),
        }
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: json!({"type": "object"}),
            strict: false,
        }
    }

    fn exchange(id: &str, extra: serde_json::Value) -> Vec<RuntimeMessage> {
        vec![
            RuntimeMessage::AssistantToolCalls {
                exchange_id: None,
                content: None,
                tool_calls: vec![call(id)],
                provider_extra: extra.as_object().cloned().unwrap_or_default(),
            },
            RuntimeMessage::ToolResult {
                call_id: id.into(),
                content: format!("result {id}"),
            },
        ]
    }

    fn baseline() -> ProjectedRequest {
        let mut messages = vec![
            RuntimeMessage::System("instructions".into()),
            RuntimeMessage::User("old question".into()),
        ];
        messages.extend(exchange("old", json!({"reasoning": "kept"})));
        messages.push(RuntimeMessage::User("new question".into()));
        messages.extend(exchange("now", json!({})));
        let mut request = ProjectedRequest {
            cache: Default::default(),
            messages,
            tools: vec![tool("echo")],
        };
        request.identify_exchanges();
        request
    }

    fn check(
        candidate: &ProjectedRequest,
        authority: ProjectionAuthority,
    ) -> Result<(), ProjectionViolation> {
        let base = baseline();
        let current: HashSet<usize> = [5].into();
        validate_step(&base, &base, candidate, authority, &current, None)
    }

    #[test]
    fn unchanged_request_is_valid_under_both_authorities() {
        check(&baseline(), ProjectionAuthority::Conversation).unwrap();
        check(&baseline(), ProjectionAuthority::FullContext).unwrap();
    }

    #[test]
    fn whole_historical_exchange_may_be_omitted_and_bodies_rewritten() {
        let mut candidate = baseline();
        candidate.messages.drain(2..4);
        if let Some(RuntimeMessage::ToolResult { content, .. }) = candidate.messages.last_mut() {
            *content = "[pruned]".into();
        }
        check(&candidate, ProjectionAuthority::Conversation).unwrap();
    }

    #[test]
    fn conversation_authority_cannot_touch_instructions_or_tools() {
        let mut edited = baseline();
        edited.messages[0] = RuntimeMessage::System("new rules".into());
        assert_eq!(
            check(&edited, ProjectionAuthority::Conversation),
            Err(ProjectionViolation::InstructionsChanged)
        );
        edited.messages[0] = RuntimeMessage::System("instructions".into());
        edited
            .messages
            .insert(1, RuntimeMessage::System("promoted".into()));
        assert_eq!(
            check(&edited, ProjectionAuthority::Conversation),
            Err(ProjectionViolation::InstructionsChanged)
        );
        let mut tools = baseline();
        tools.tools[0].description = "rewritten".into();
        assert_eq!(
            check(&tools, ProjectionAuthority::Conversation),
            Err(ProjectionViolation::ToolsChanged)
        );
        // The same edits are within full-context authority.
        check(&edited, ProjectionAuthority::FullContext).unwrap();
        check(&tools, ProjectionAuthority::FullContext).unwrap();
    }

    #[test]
    fn full_context_cannot_expose_ungranted_or_duplicate_tools() {
        let mut candidate = baseline();
        candidate.tools.push(tool("shell"));
        assert_eq!(
            check(&candidate, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::UngrantedTool("shell".into()))
        );
        let mut candidate = baseline();
        candidate.tools.push(tool("echo"));
        assert_eq!(
            check(&candidate, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::DuplicateTool("echo".into()))
        );
    }

    #[test]
    fn exchange_integrity_violations_are_rejected() {
        // Half-exchange: the result without its call.
        let mut half = baseline();
        half.messages.remove(2);
        assert_eq!(
            check(&half, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::HalfExchange("old".into()))
        );
        // Half-exchange: the call without its result.
        let mut half = baseline();
        half.messages.remove(3);
        assert_eq!(
            check(&half, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::HalfExchange("old".into()))
        );
        // Forged call.
        let mut forged = baseline();
        forged.messages.extend(exchange("invented", json!({})));
        assert_eq!(
            check(&forged, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::ForgedCall("invented".into()))
        );
        // Altered arguments.
        let mut altered = baseline();
        if let RuntimeMessage::AssistantToolCalls { tool_calls, .. } = &mut altered.messages[2] {
            tool_calls[0].arguments = r#"{"x":1}"#.into();
        }
        assert_eq!(
            check(&altered, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::AlteredCall("old".into()))
        );
        // Altered retained provider echo.
        let mut echo = baseline();
        if let RuntimeMessage::AssistantToolCalls { provider_extra, .. } = &mut echo.messages[2] {
            provider_extra.insert("reasoning".into(), json!("changed"));
        }
        assert_eq!(
            check(&echo, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::AlteredProviderEcho("old".into()))
        );
        // Reordered exchanges.
        let base = baseline();
        let mut reordered = ProjectedRequest {
            cache: Default::default(),
            messages: vec![base.messages[0].clone()],
            tools: base.tools.clone(),
        };
        reordered
            .messages
            .extend(base.messages[5..7].iter().cloned());
        reordered
            .messages
            .extend(base.messages[2..4].iter().cloned());
        assert_eq!(
            check(&reordered, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::ReorderedExchange("old".into()))
        );
        // Dropping the current attempt's exchange.
        let mut dropped = baseline();
        dropped.messages.truncate(5);
        assert_eq!(
            check(&dropped, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::DroppedCurrentExchange("now".into()))
        );
        // Empty request.
        let empty = ProjectedRequest {
            cache: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        assert_eq!(
            check(&empty, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::Empty)
        );
    }

    #[test]
    fn repeated_provider_ids_keep_distinct_occurrences_and_current_protection() {
        let mut base = baseline();
        // Deliberately identical immutable fields AND result bodies; source
        // occurrence, not textual similarity, must distinguish these groups.
        base.messages
            .splice(5..7, exchange("old", json!({"reasoning": "kept"})));
        base.identify_exchanges();
        let current = HashSet::from([5]);
        let validate = |candidate: &ProjectedRequest| {
            validate_step(
                &base,
                &base,
                candidate,
                ProjectionAuthority::FullContext,
                &current,
                None,
            )
        };
        validate(&base).unwrap();
        let mut historical_omitted = base.clone();
        historical_omitted.messages.drain(2..4);
        if let RuntimeMessage::ToolResult { content, .. } =
            historical_omitted.messages.last_mut().unwrap()
        {
            *content = "authorized replacement body".into();
        }
        validate(&historical_omitted).unwrap();
        let mut current_omitted = base.clone();
        current_omitted.messages.truncate(5);
        assert_eq!(
            validate(&current_omitted),
            Err(ProjectionViolation::DroppedCurrentExchange("old".into()))
        );
        let mut duplicate = base.clone();
        duplicate.messages.extend_from_within(5..7);
        assert_eq!(
            validate(&duplicate),
            Err(ProjectionViolation::ReorderedExchange("old".into()))
        );
        let mut swapped = base.clone();
        swapped.messages.swap(2, 5);
        swapped.messages.swap(3, 6);
        assert_eq!(
            validate(&swapped),
            Err(ProjectionViolation::ReorderedExchange("old".into()))
        );
        let mut forged = base.clone();
        if let RuntimeMessage::AssistantToolCalls { exchange_id, .. } = &mut forged.messages[5] {
            *exchange_id = Some(99);
        }
        assert_eq!(
            validate(&forged),
            Err(ProjectionViolation::ForgedCall("old".into()))
        );
    }

    #[test]
    fn retained_assistant_output_obeys_exchange_integrity() {
        let mut base = baseline();
        let native = crate::runtime::RuntimeMessage::assistant(
            "answer".into(),
            serde_json::Map::from_iter([(
                crate::responses::OUTPUT_ITEMS.into(),
                json!([{"type":"reasoning", "encrypted_content":"opaque"}]),
            )]),
        );
        // Identical provider bytes still represent two distinct occurrences.
        base.messages.insert(4, native.clone());
        base.messages.push(native);
        base.identify_exchanges();
        let current = HashSet::from([6, 8]);
        for authority in [
            ProjectionAuthority::Conversation,
            ProjectionAuthority::FullContext,
        ] {
            let validate = |candidate: &ProjectedRequest| {
                validate_step(&base, &base, candidate, authority, &current, None)
            };
            validate(&base).unwrap();
            let mut text_edited = base.clone();
            if let RuntimeMessage::AssistantWithExtra { content, .. } =
                text_edited.messages.last_mut().unwrap()
            {
                *content = Some("display text changed".into());
            }
            validate(&text_edited).unwrap();
            let mut historical_omitted = base.clone();
            historical_omitted.messages.remove(4);
            validate(&historical_omitted).unwrap();
            let mut altered = base.clone();
            if let RuntimeMessage::AssistantWithExtra { provider_extra, .. } =
                altered.messages.last_mut().unwrap()
            {
                provider_extra.clear();
            }
            assert!(
                validate(&altered).is_err(),
                "opaque output cannot be edited"
            );
            let mut dropped = base.clone();
            dropped.messages.pop();
            assert!(
                validate(&dropped).is_err(),
                "current output cannot be dropped"
            );
            let mut repeated = base.clone();
            repeated.messages.push(base.messages[4].clone());
            assert!(validate(&repeated).is_err(), "output cannot repeat");
            let mut swapped = base.clone();
            swapped.messages.swap(4, 8);
            assert!(
                validate(&swapped).is_err(),
                "equal bytes cannot exchange occurrences"
            );
            let mut forged = base.clone();
            forged
                .messages
                .push(crate::runtime::RuntimeMessage::assistant(
                    "invented".into(),
                    serde_json::Map::from_iter([("unknown".into(), json!("forged"))]),
                ));
            assert!(
                validate(&forged).is_err(),
                "provider output needs a host occurrence"
            );
        }
    }

    #[test]
    fn budget_counts_the_whole_request() {
        let base = baseline();
        let estimated = crate::context::estimate_request_tokens(&base.messages, &base.tools);
        let none = HashSet::new();
        validate_step(
            &base,
            &base,
            &base,
            ProjectionAuthority::Conversation,
            &none,
            Some(estimated),
        )
        .unwrap();
        assert_eq!(
            validate_step(
                &base,
                &base,
                &base,
                ProjectionAuthority::Conversation,
                &none,
                Some(estimated - 1)
            ),
            Err(ProjectionViolation::OverBudget {
                estimated,
                budget: estimated - 1
            })
        );
    }

    #[test]
    fn grants_validate_and_order_by_phase_then_list() {
        let grant = |name: &str, authority| ContextProjectionGrant {
            extension: name.into(),
            authority,
            required: false,
        };
        let grants = vec![
            grant("full-a", ProjectionAuthority::FullContext),
            grant("conv-a", ProjectionAuthority::Conversation),
            grant("full-b", ProjectionAuthority::FullContext),
            grant("conv-b", ProjectionAuthority::Conversation),
        ];
        validate_grants(&grants).unwrap();
        let order: Vec<_> = invocation_order(&grants)
            .into_iter()
            .map(|g| g.extension)
            .collect();
        assert_eq!(order, ["conv-a", "conv-b", "full-a", "full-b"]);
        let duplicate = vec![
            grant("x", ProjectionAuthority::Conversation),
            grant("x", ProjectionAuthority::FullContext),
        ];
        assert!(
            validate_grants(&duplicate)
                .unwrap_err()
                .contains("more than once")
        );
        assert!(validate_grants(&[grant(" ", ProjectionAuthority::Conversation)]).is_err());
        let parsed: ContextProjectionGrant =
            serde_yaml::from_str("extension: pruner\nauthority: full_context\nrequired: true\n")
                .unwrap();
        assert_eq!(parsed.authority, ProjectionAuthority::FullContext);
        assert!(parsed.required);
        let defaulted: ContextProjectionGrant =
            serde_yaml::from_str("extension: pruner\n").unwrap();
        assert_eq!(defaulted.authority, ProjectionAuthority::Conversation);
        assert!(!defaulted.required);
    }
}
