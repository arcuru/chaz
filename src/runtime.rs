//! Agent runtime — executes the ReAct loop.
//!
//! The runtime takes a ChatContext, a backend, and a set of tools.
//! If tools are available and the backend supports them, it runs a
//! ReAct loop (Reason → Act → Observe → repeat). Otherwise it falls
//! back to a single-shot LLM call.

use crate::backends::{BackendManager, ChatContext};
use crate::tool::ToolRegistry;
use openai_api_rs::v1::chat_completion::MessageRole;
use serde::{Deserialize, Serialize};
use tracing::info;

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
pub async fn execute(
    context: &ChatContext,
    backend: &BackendManager,
    tools: &ToolRegistry,
) -> Result<String, String> {
    // Fast path: no tools or backend doesn't support them → single-shot
    if tools.is_empty() || !backend.supports_tools(context) {
        return backend.execute(context).await;
    }

    let tool_defs = tools.definitions();
    let model = backend.resolve_model(context);
    let mut messages = context_to_messages(context);

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

                // Execute each tool and record results
                for call in &tool_calls {
                    let result = match tools.get(&call.name) {
                        Some(tool) => {
                            let args: serde_json::Value =
                                serde_json::from_str(&call.arguments).unwrap_or_default();
                            match tool.execute(args).await {
                                Ok(output) => {
                                    info!(
                                        "Tool {} returned: {}",
                                        call.name,
                                        &output[..output.len().min(100)]
                                    );
                                    output
                                }
                                Err(e) => format!("Tool error: {e}"),
                            }
                        }
                        None => format!("Unknown tool: {}", call.name),
                    };

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

/// Convert a ChatContext into RuntimeMessages for the ReAct loop
fn context_to_messages(context: &ChatContext) -> Vec<RuntimeMessage> {
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
