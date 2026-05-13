//! OpenAI-compatible backend for chaz.
//!
//! Uses `async-openai`'s **bring-your-own-type** (byot) API: we pass our
//! own request/response structs to `client.chat().create_byot()` so provider
//! extensions like DeepSeek's `reasoning_content` round-trip without the
//! crate's strict types dropping unknown fields.

use async_openai::{Client, config::OpenAIConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    backends::{ChatContext, LLMBackend},
    config::Backend,
    error::LlmError,
    runtime::{LLMResponse, ResponseMetadata, RuntimeMessage, TokenUsage, ToolCallRequest},
    security::SecretStore,
    tool::ToolDefinition,
};

/// Handle connections to an OpenAI compatible backend
pub struct OpenAI {
    /// Stores the backend config (api_key cleared — use secret store)
    backend: Backend,
    /// Secret store for host-boundary key injection
    secrets: SecretStore,
}

// ================================================================
// BYOT wire types
// ================================================================
//
// The openai chat completions shape, written directly so we control every
// field on both the request and response side. `#[serde(flatten)] extra`
// on messages catches unknown provider-specific fields and preserves them
// across round-trips — critical for providers like DeepSeek where the
// `reasoning_content` field must be echoed back verbatim on subsequent
// requests or the API 400s.

#[derive(Debug, Clone, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ChatTool>>,
    /// Opt into OpenRouter's extended usage accounting (`cost`, cache details,
    /// reasoning tokens). Unknown to vanilla OpenAI/DeepSeek but ignored
    /// silently per the spec, so we always set it.
    usage: UsageOpts,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct UsageOpts {
    include: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// Catch-all for provider-specific fields on an assistant message:
    /// DeepSeek's `reasoning_content`, Anthropic's `reasoning_details`,
    /// OpenRouter's `reasoning`, and whatever else providers add. Preserving
    /// this across round-trips is essential — DeepSeek thinking mode rejects
    /// the follow-up with 400 if the reasoning field isn't echoed back.
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: ChatToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatToolFunction,
}

#[derive(Debug, Clone, Serialize)]
struct ChatToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    /// Response id (e.g. OR's `gen-…`). Useful for correlating with the
    /// backend's request logs. Absent on some compatible providers.
    #[serde(default)]
    id: Option<String>,
    /// The model that actually answered. May differ from the requested model
    /// when the backend (OR) falls back or routes elsewhere.
    #[serde(default)]
    model: Option<String>,
    /// Upstream inference provider when reported (OpenRouter-specific).
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
    /// Catch-all for provider extensions we don't normalize but want to
    /// preserve (e.g. OR's `cost_details`, `is_byok`).
    #[serde(flatten, default)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
    /// Backend-reported cost in USD. Populated when the request opts into
    /// extended usage accounting (OR with `usage.include = true`).
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    /// Anthropic-style prompt cache breakdown when the backend surfaces it.
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

impl Usage {
    /// Project the wire-format Usage onto the normalized `TokenUsage` shape.
    /// Picks the first populated source for each cache/reasoning field so we
    /// transparently handle both OpenAI-style nested details and
    /// Anthropic-style flat fields.
    fn into_token_usage(self) -> TokenUsage {
        let cached_tokens = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .or(self.cache_read_input_tokens);
        let reasoning_tokens = self
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens);
        TokenUsage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cached_tokens,
            cache_creation_tokens: self.cache_creation_input_tokens,
            reasoning_tokens,
            cost_usd: self.cost,
        }
    }
}

/// Build `ResponseMetadata` from the parsed response. Falls back to the
/// requested model name if the backend didn't echo it.
fn build_metadata(
    response_id: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    usage: Option<Usage>,
    extra: Map<String, Value>,
    requested_model: &str,
) -> Option<ResponseMetadata> {
    // If absolutely nothing useful came back, surface None so callers know
    // this call wasn't accounted for.
    if response_id.is_none() && model.is_none() && provider.is_none() && usage.is_none() {
        return None;
    }
    Some(ResponseMetadata {
        model: model.unwrap_or_else(|| requested_model.to_string()),
        provider,
        response_id,
        usage: usage.map(Usage::into_token_usage).unwrap_or_default(),
        extra,
    })
}

impl OpenAI {
    pub fn new(backend: &Backend, secrets: &SecretStore) -> Self {
        OpenAI {
            backend: backend.clone(),
            secrets: secrets.clone(),
        }
    }

    fn build_client(&self) -> Result<Client<OpenAIConfig>, LlmError> {
        // Host-boundary injection: resolve API key from SecretStore by reference,
        // falling back to the raw api_key field for backward compatibility.
        let api_key = self
            .backend
            .api_key_ref
            .as_ref()
            .and_then(|r| self.secrets.get(r))
            .or_else(|| self.backend.api_key.clone())
            .ok_or_else(|| LlmError::Configuration {
                message: "API key not configured".to_string(),
            })?;
        let api_base = self
            .backend
            .api_base
            .clone()
            .ok_or_else(|| LlmError::Configuration {
                message: "API base URL not configured".to_string(),
            })?;
        let config = OpenAIConfig::new()
            .with_api_base(api_base)
            .with_api_key(api_key);
        Ok(Client::with_config(config))
    }

    /// Execute a single LLM call with tool definitions, returning a structured response.
    ///
    /// This is called by the runtime's ReAct loop. It converts RuntimeMessages
    /// to OpenAI format, includes tool definitions, and parses the response.
    async fn chat_with_tools_impl(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<LLMResponse, LlmError> {
        let client = self.build_client()?;

        let openai_messages = convert_runtime_messages(messages);
        let openai_tools = convert_tool_definitions(tools);

        let request = ChatRequest {
            model,
            messages: openai_messages,
            tools: if openai_tools.is_empty() {
                None
            } else {
                Some(openai_tools)
            },
            usage: UsageOpts { include: true },
        };

        let timeout = self.backend.request_timeout();
        let response: ChatResponse = tokio::time::timeout(
            timeout,
            client
                .chat()
                .create_byot::<ChatRequest, ChatResponse>(request),
        )
        .await
        .map_err(|_| {
            tracing::warn!(timeout_secs = timeout.as_secs(), "LLM request timed out");
            LlmError::Timeout
        })?
        .map_err(LlmError::from_openai_error)?;

        let ChatResponse {
            choices,
            id,
            model: response_model,
            provider,
            usage,
            extra: response_extra,
        } = response;

        let metadata = build_metadata(id, response_model, provider, usage, response_extra, model);

        let choice = choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::EmptyResponse {
                message: "No choices in response".to_string(),
            })?;

        let ChatMessage {
            content,
            tool_calls,
            extra,
            ..
        } = choice.message;

        tracing::debug!(
            "LLM response: content={:?} tool_calls={:?} extra_fields={:?} finish_reason={:?} usage={:?}",
            content.as_deref().map(|c| &c[..c.len().min(100)]),
            tool_calls.as_ref().map(|tc| tc.len()),
            extra.keys().collect::<Vec<_>>(),
            choice.finish_reason,
            metadata.as_ref().map(|m| &m.usage),
        );

        // Check if the LLM wants to call tools
        if let Some(calls) = tool_calls
            && !calls.is_empty()
        {
            let requests = calls
                .into_iter()
                .map(|tc| ToolCallRequest {
                    id: tc.id,
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                })
                .collect();

            return Ok(LLMResponse::ToolCalls {
                content,
                tool_calls: requests,
                provider_extra: extra,
                metadata,
            });
        }

        // Final text response
        Ok(LLMResponse::Text {
            content: content.unwrap_or_default(),
            metadata,
        })
    }
}

impl LLMBackend for OpenAI {
    /// List the models available to this backend
    fn list_models(&self) -> Vec<String> {
        let mut models = Vec::new();
        for model in &self.backend.models.clone().unwrap_or_default() {
            models.push(model.name.clone());
        }
        models
    }

    /// Get the default model for this backend
    fn default_model(&self) -> Option<String> {
        if let Some(models) = &self.backend.models
            && !models.is_empty()
        {
            return Some(models[0].name.clone());
        }
        None
    }

    fn supports_tools(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<LLMResponse, LlmError> {
        self.chat_with_tools_impl(messages, tools, model).await
    }

    /// Execute a simple chat request (no tools)
    async fn execute(&self, context: &ChatContext) -> Result<String, LlmError> {
        let client = self.build_client()?;
        let model_prefix = self.backend.name.clone().unwrap_or("openai".to_string());
        let (model, messages) = convert_chat_context(context, &model_prefix, &self.default_model());

        tracing::debug!(
            model = %model,
            messages = messages.len(),
            "LLM request"
        );

        let request = ChatRequest {
            model: &model,
            messages,
            tools: None,
            usage: UsageOpts { include: true },
        };

        let timeout = self.backend.request_timeout();
        let response: ChatResponse = tokio::time::timeout(
            timeout,
            client
                .chat()
                .create_byot::<ChatRequest, ChatResponse>(request),
        )
        .await
        .map_err(|_| {
            tracing::warn!(timeout_secs = timeout.as_secs(), "LLM request timed out");
            LlmError::Timeout
        })?
        .map_err(LlmError::from_openai_error)?;

        Ok(response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_else(|| "Error retrieving response".to_string()))
    }
}

/// Convert RuntimeMessages to our BYOT ChatMessages.
fn convert_runtime_messages(messages: &[RuntimeMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|msg| match msg {
            RuntimeMessage::System(content) => ChatMessage {
                role: "system".to_string(),
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: None,
                extra: Map::new(),
            },
            RuntimeMessage::User(content) => ChatMessage {
                role: "user".to_string(),
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: None,
                extra: Map::new(),
            },
            RuntimeMessage::Assistant(content) => ChatMessage {
                role: "assistant".to_string(),
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: None,
                extra: Map::new(),
            },
            RuntimeMessage::AssistantToolCalls {
                content,
                tool_calls,
                provider_extra,
            } => ChatMessage {
                role: "assistant".to_string(),
                content: content.clone(),
                tool_calls: Some(
                    tool_calls
                        .iter()
                        .map(|tc| ChatToolCall {
                            id: tc.id.clone(),
                            kind: "function".to_string(),
                            function: ChatToolCallFunction {
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            },
                        })
                        .collect(),
                ),
                tool_call_id: None,
                extra: provider_extra.clone(),
            },
            RuntimeMessage::ToolResult { call_id, content } => ChatMessage {
                role: "tool".to_string(),
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: Some(call_id.clone()),
                extra: Map::new(),
            },
        })
        .collect()
}

/// Convert ToolDefinitions to our BYOT tool shape.
fn convert_tool_definitions(tools: &[ToolDefinition]) -> Vec<ChatTool> {
    tools
        .iter()
        .map(|td| ChatTool {
            kind: "function",
            function: ChatToolFunction {
                name: td.name.clone(),
                description: td.description.clone(),
                parameters: td.parameters.clone(),
            },
        })
        .collect()
}

/// Convert a ChatContext (legacy, no-tools path) to (model, messages) for a request.
fn convert_chat_context(
    context: &ChatContext,
    model_prefix: &str,
    default_model: &Option<String>,
) -> (String, Vec<ChatMessage>) {
    let mut messages = Vec::new();
    if let Some(role) = &context.role {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(role.get_prompt()),
            tool_calls: None,
            tool_call_id: None,
            extra: Map::new(),
        });
    }
    for message in &context.messages {
        messages.push(ChatMessage {
            role: message.role.as_str().to_string(),
            content: Some(message.content.clone()),
            tool_calls: None,
            tool_call_id: None,
            extra: Map::new(),
        });
    }
    let mut model = context.model.clone().unwrap_or_default();
    model = model
        .trim_start_matches(&format!("{}:", model_prefix))
        .to_string();
    if model.is_empty() {
        model = default_model.clone().unwrap_or_default();
    }
    (model, messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_normalizes_openai_style_cached_tokens() {
        let u = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cost: Some(0.0123),
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(40),
            }),
            completion_tokens_details: Some(CompletionTokensDetails {
                reasoning_tokens: Some(20),
            }),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        let t = u.into_token_usage();
        assert_eq!(t.prompt_tokens, 100);
        assert_eq!(t.cached_tokens, Some(40));
        assert_eq!(t.reasoning_tokens, Some(20));
        assert_eq!(t.cost_usd, Some(0.0123));
        assert_eq!(t.cache_creation_tokens, None);
    }

    #[test]
    fn usage_normalizes_anthropic_style_cache_fields() {
        // Anthropic uses flat cache_read_input_tokens / cache_creation_input_tokens
        // instead of nested prompt_tokens_details.cached_tokens.
        let u = Usage {
            prompt_tokens: 200,
            completion_tokens: 100,
            total_tokens: 300,
            cost: None,
            prompt_tokens_details: None,
            completion_tokens_details: None,
            cache_read_input_tokens: Some(50),
            cache_creation_input_tokens: Some(10),
        };
        let t = u.into_token_usage();
        assert_eq!(t.cached_tokens, Some(50));
        assert_eq!(t.cache_creation_tokens, Some(10));
        assert_eq!(t.cost_usd, None);
    }

    #[test]
    fn usage_prefers_nested_over_flat_when_both_present() {
        // If a backend somehow returns both shapes, the nested OpenAI-style
        // field wins. Arbitrary but consistent.
        let u = Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cost: None,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(7),
            }),
            completion_tokens_details: None,
            cache_read_input_tokens: Some(99),
            cache_creation_input_tokens: None,
        };
        assert_eq!(u.into_token_usage().cached_tokens, Some(7));
    }

    #[test]
    fn build_metadata_returns_none_when_response_has_nothing() {
        let m = build_metadata(
            None,
            None,
            None,
            None,
            Map::new(),
            "anthropic/claude-haiku-4-5",
        );
        assert!(m.is_none());
    }

    #[test]
    fn build_metadata_falls_back_to_requested_model() {
        let m = build_metadata(
            Some("gen-abc".into()),
            None, // backend didn't echo model
            None,
            None,
            Map::new(),
            "openai/gpt-5",
        )
        .expect("response_id alone is enough to surface metadata");
        assert_eq!(m.model, "openai/gpt-5");
        assert_eq!(m.response_id.as_deref(), Some("gen-abc"));
    }
}
