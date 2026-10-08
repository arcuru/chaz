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
//! bodies and other message text may change.
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
    /// A session entry selected into context, by its position in the
    /// session's entry list at assembly time.
    SessionEntry {
        index: usize,
        sender: String,
        timestamp: DateTime<Utc>,
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
    /// A tool exchange completed by the current attempt.
    CurrentAttempt { model_sequence: u64 },
    /// A caller supplied no provenance for this message.
    Unattributed,
}

/// The model-facing request for one logical model call.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedRequest {
    pub messages: Vec<RuntimeMessage>,
    pub tools: Vec<ToolDefinition>,
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
    /// Call ids completed by the current attempt; their exchanges must stay.
    pub current_calls: &'a HashSet<String>,
    pub budget_tokens: Option<usize>,
}

/// Run the resolved chain over `baseline` and return the accepted request.
pub(crate) async fn run_chain(
    chain: &[ResolvedProjector],
    baseline: &ProjectedRequest,
    inputs: &RoundInputs<'_>,
) -> Result<ProjectedRequest, ProjectionError> {
    let mut current = baseline.clone();
    for step in chain {
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
        };
        // The endpoint call sits inside the guarded future so a panic while
        // building the future is caught as well as one while polling it.
        let input = current.clone();
        let invoked = tokio::time::timeout(
            PROJECTOR_TIMEOUT,
            AssertUnwindSafe(async { step.projector.project(&call, input).await }).catch_unwind(),
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
                inputs.current_calls,
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
    order: usize,
    calls: &'a [ToolCallRequest],
    provider_extra: &'a serde_json::Map<String, serde_json::Value>,
}

/// Validate one step's candidate against its input and the round baseline.
pub(crate) fn validate_step(
    baseline: &ProjectedRequest,
    input: &ProjectedRequest,
    candidate: &ProjectedRequest,
    authority: ProjectionAuthority,
    current_calls: &HashSet<String>,
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

    // Index the baseline's call groups by their first call id.
    let mut groups: HashMap<&str, CallGroup<'_>> = HashMap::new();
    let mut result_ids: HashSet<&str> = HashSet::new();
    for message in &baseline.messages {
        match message {
            RuntimeMessage::AssistantToolCalls {
                tool_calls,
                provider_extra,
                ..
            } if !tool_calls.is_empty() => {
                let order = groups.len();
                groups.insert(
                    tool_calls[0].id.as_str(),
                    CallGroup {
                        order,
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

    let mut seen: HashSet<&str> = HashSet::new();
    let mut last_order: Option<usize> = None;
    let mut i = 0;
    while i < candidate.messages.len() {
        match &candidate.messages[i] {
            RuntimeMessage::AssistantToolCalls {
                tool_calls,
                provider_extra,
                ..
            } if !tool_calls.is_empty() => {
                let first = tool_calls[0].id.as_str();
                let Some(group) = groups.get(first) else {
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
                if !seen.insert(first) || last_order.is_some_and(|last| group.order <= last) {
                    return Err(ProjectionViolation::ReorderedExchange(first.to_string()));
                }
                last_order = Some(group.order);
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

    let mut dropped: Vec<&str> = groups
        .iter()
        .filter(|(first, group)| {
            !seen.contains(*first)
                && group
                    .calls
                    .iter()
                    .any(|call| current_calls.contains(&call.id))
        })
        .map(|(first, _)| *first)
        .collect();
    dropped.sort_unstable();
    if let Some(first) = dropped.first() {
        return Err(ProjectionViolation::DroppedCurrentExchange(
            first.to_string(),
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
        ProjectedRequest {
            messages,
            tools: vec![tool("echo")],
        }
    }

    fn check(
        candidate: &ProjectedRequest,
        authority: ProjectionAuthority,
    ) -> Result<(), ProjectionViolation> {
        let base = baseline();
        let current: HashSet<String> = ["now".to_string()].into();
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
            messages: Vec::new(),
            tools: Vec::new(),
        };
        assert_eq!(
            check(&empty, ProjectionAuthority::FullContext),
            Err(ProjectionViolation::Empty)
        );
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
