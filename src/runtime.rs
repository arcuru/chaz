//! Agent runtime — executes the ReAct loop.
//!
//! The runtime takes a ChatContext, a backend, and a set of tools.
//! If tools are available and the backend supports them, it runs a
//! ReAct loop (Reason → Act → Observe → repeat). Otherwise it falls
//! back to a single-shot LLM call.
//!
//! Security controls (Phase 3.8):
//! - Tool calls are checked against approval requirements before execution
//! - Tool outputs are scanned for secret leaks before entering the conversation
//! - Tool execution is wrapped in a timeout
//! - Content from tool outputs is scanned for injection patterns (warning-only)

use crate::backends::{BackendManager, ChatContext};
use crate::gateway::ApprovalDecision;
use crate::security::{Sanitizer, SecurityContext};
use crate::tool::{ToolApprovalInfo, ToolContext, ToolPolicyRegistry};
use openai_api_rs::v1::chat_completion::MessageRole;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Events emitted during the ReAct loop for audit trail / observability.
#[allow(dead_code)]
pub enum RuntimeEvent {
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
}

const MAX_TOOL_ITERATIONS: usize = 10;

// === Message types for the ReAct loop ===

/// A message in the runtime conversation, richer than ChatContext::Message
/// to support tool call/result exchanges.
#[derive(Clone, Debug)]
pub enum RuntimeMessage {
    System(String),
    User(String),
    Assistant(String),
    AssistantToolCalls {
        content: Option<String>,
        tool_calls: Vec<ToolCallRequest>,
    },
    ToolResult {
        call_id: String,
        content: String,
    },
}

/// A tool call requested by the LLM
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Response from a single LLM call — either final text or tool calls
pub enum LLMResponse {
    Text(String),
    ToolCalls {
        content: Option<String>,
        tool_calls: Vec<ToolCallRequest>,
    },
}

/// Run the agent runtime for a single turn.
///
/// If tools are registered and the backend supports tool calling,
/// runs a ReAct loop. Otherwise falls back to a single-shot execute.
///
/// Accepts pre-built `RuntimeMessage`s from the `ContextBuilder`.
/// The `context` is still needed for backend selection (model routing)
/// and the simple-execution fallback path.
pub async fn execute(
    context: &ChatContext,
    initial_messages: Vec<RuntimeMessage>,
    backend: &BackendManager,
    security: &SecurityContext,
    tool_ctx: &ToolContext,
    policies: &ToolPolicyRegistry,
    event_sink: Option<mpsc::Sender<RuntimeEvent>>,
) -> Result<String, String> {
    let tools = &tool_ctx.tools;
    let model = backend.resolve_model(context);

    // Fast path: no tools or backend doesn't support them → single-shot
    // Use chat_with_tools with empty tools — it sends RuntimeMessages directly.
    if tools.is_empty() || !backend.supports_tools(context) {
        return match backend
            .chat_with_tools(context, &initial_messages, &[], &model)
            .await
        {
            Ok(LLMResponse::Text(text)) => Ok(text),
            Ok(LLMResponse::ToolCalls { .. }) => {
                Err("Unexpected tool calls in no-tools fallback".to_string())
            }
            Err(e) => Err(e),
        };
    }

    let tool_defs = tools.definitions(&tool_ctx.profile);
    let mut messages = initial_messages;
    let mut approve_all = false; // tracks if user chose "approve all" this turn

    for iteration in 0..MAX_TOOL_ITERATIONS {
        let response = match backend
            .chat_with_tools(context, &messages, &tool_defs, &model)
            .await
        {
            Ok(resp) => resp,
            Err(e) if iteration == 0 => {
                // First call failed with tools — retry without tools as fallback.
                // Some models/providers don't support function calling.
                info!("Tool-aware call failed ({e}), falling back to simple execution");
                return backend.execute(context).await;
            }
            Err(e) => return Err(e),
        };

        match response {
            LLMResponse::Text(text) if !text.is_empty() => {
                if iteration > 0 {
                    info!("ReAct loop completed after {} tool iterations", iteration);
                }
                return Ok(text);
            }
            LLMResponse::Text(_) if iteration > 0 => {
                // Model returned empty response after tool calls — some models do this.
                // Return the last tool result as the response.
                info!("Empty response after tool calls, using last tool result");
                if let Some(RuntimeMessage::ToolResult { content, .. }) = messages.last() {
                    return Ok(content.clone());
                }
                return Err("Model returned empty response after tool execution".to_string());
            }
            LLMResponse::Text(text) => return Ok(text),
            LLMResponse::ToolCalls {
                content,
                tool_calls,
            } => {
                info!(
                    "Tool calls requested: {:?}",
                    tool_calls.iter().map(|tc| &tc.name).collect::<Vec<_>>()
                );

                // Record the assistant's tool call request
                messages.push(RuntimeMessage::AssistantToolCalls {
                    content: content.clone(),
                    tool_calls: tool_calls.clone(),
                });

                // Execute each tool with security checks
                for call in &tool_calls {
                    // Emit tool call event
                    if let Some(ref sink) = event_sink {
                        let _ = sink
                            .send(RuntimeEvent::ToolCall {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            })
                            .await;
                    }

                    let result = match tools.get(&call.name) {
                        Some(tool) => {
                            let policy = policies.resolve(tool);
                            let args: serde_json::Value =
                                serde_json::from_str(&call.arguments).unwrap_or_default();

                            // --- Security: approval gate ---
                            if !approve_all && security.needs_approval(&call.name, &policy.approval)
                            {
                                let sensitive_refs: Vec<&str> =
                                    policy.sensitive_params.iter().map(|s| s.as_str()).collect();
                                let info = ToolApprovalInfo {
                                    name: call.name.clone(),
                                    arguments_display: redact_sensitive_params(
                                        &call.arguments,
                                        &sensitive_refs,
                                    ),
                                    risk_level: policy.risk.clone(),
                                };

                                let decision = security.request_approval(info).await;
                                match decision {
                                    ApprovalDecision::Approve => {} // proceed
                                    ApprovalDecision::ApproveAll => {
                                        approve_all = true; // skip approval for rest of turn
                                    }
                                    ApprovalDecision::Deny => {
                                        messages.push(RuntimeMessage::ToolResult {
                                            call_id: call.id.clone(),
                                            content: "Tool execution denied by user".to_string(),
                                        });
                                        continue;
                                    }
                                }
                            }

                            // --- Security: execute with timeout ---
                            let timeout = policy.timeout_duration();
                            let exec_result =
                                tokio::time::timeout(timeout, tool.execute(args, tool_ctx)).await;

                            match exec_result {
                                Ok(Ok(output)) => {
                                    info!(
                                        "Tool {} returned: {}",
                                        call.name,
                                        &output[..output.len().min(100)]
                                    );

                                    // --- Security: scan for injection patterns (warning-only) ---
                                    let warnings = Sanitizer::scan(&output);
                                    if !warnings.is_empty() {
                                        warn!(
                                            tool = %call.name,
                                            count = warnings.len(),
                                            "Prompt injection patterns detected in tool output"
                                        );
                                    }

                                    // --- Security: leak detection ---
                                    match security.leak_detector.scan(&output) {
                                        Ok(scanned) => scanned,
                                        Err(e) => {
                                            warn!(tool = %call.name, "Tool output blocked by leak detector");
                                            format!("Tool output blocked: {e}")
                                        }
                                    }
                                }
                                Ok(Err(e)) => format!("Tool error: {e}"),
                                Err(_) => {
                                    warn!(
                                        tool = %call.name,
                                        timeout_secs = timeout.as_secs(),
                                        "Tool execution timed out"
                                    );
                                    format!("Tool timed out after {} seconds", timeout.as_secs())
                                }
                            }
                        }
                        None => format!("Unknown tool: {}", call.name),
                    };

                    // Emit tool result event
                    if let Some(ref sink) = event_sink {
                        let is_error = result.starts_with("Tool error:")
                            || result.starts_with("Tool timed out");
                        let _ = sink
                            .send(RuntimeEvent::ToolResult {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                output: result.clone(),
                                is_error,
                            })
                            .await;
                    }

                    info!(
                        "Tool result for {}: {}",
                        call.id,
                        &result[..result.len().min(200)]
                    );
                    messages.push(RuntimeMessage::ToolResult {
                        call_id: call.id.clone(),
                        content: result,
                    });
                }
            }
        }
    }

    // Hit the cap — make one final call without tools to force a text summary
    info!("Max tool iterations reached, forcing final response");
    messages.push(RuntimeMessage::User(
        "Please summarize what you found so far and respond to the user.".to_string(),
    ));
    match backend
        .chat_with_tools(context, &messages, &[], &model)
        .await
    {
        Ok(LLMResponse::Text(text)) if !text.is_empty() => Ok(text),
        _ => {
            // Last resort: return the last tool result
            for msg in messages.iter().rev() {
                if let RuntimeMessage::ToolResult { content, .. } = msg {
                    return Ok(content.clone());
                }
            }
            Err("Agent reached maximum tool iterations without a final response".to_string())
        }
    }
}

/// Redact sensitive parameter values from a JSON arguments string for display.
fn redact_sensitive_params(arguments_json: &str, sensitive: &[&str]) -> String {
    if sensitive.is_empty() {
        return arguments_json.to_string();
    }

    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(arguments_json) {
        if let Some(obj) = value.as_object_mut() {
            for key in sensitive {
                if obj.contains_key(*key) {
                    obj.insert(
                        key.to_string(),
                        serde_json::Value::String("[REDACTED]".to_string()),
                    );
                }
            }
        }
        serde_json::to_string(&value).unwrap_or_else(|_| arguments_json.to_string())
    } else {
        arguments_json.to_string()
    }
}

/// Convert a ChatContext into RuntimeMessages for the ReAct loop.
/// Used by the simple-execution fallback and /compact summarization.
#[allow(dead_code)]
pub fn context_to_messages(context: &ChatContext) -> Vec<RuntimeMessage> {
    let mut messages = Vec::new();

    // System prompt from role
    if let Some(role) = &context.role {
        let prompt = role.get_prompt();
        if !prompt.is_empty() {
            messages.push(RuntimeMessage::System(prompt));
        }
    }

    // Conversation history
    for msg in &context.messages {
        let rm = match msg.role {
            MessageRole::system => RuntimeMessage::System(msg.content.clone()),
            MessageRole::user => RuntimeMessage::User(msg.content.clone()),
            MessageRole::assistant => RuntimeMessage::Assistant(msg.content.clone()),
            _ => continue,
        };
        messages.push(rm);
    }

    messages
}
