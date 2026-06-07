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

/// Anthropic prompt-cache breakpoint marker. On the OpenRouter
/// OpenAI-compatible endpoint this rides inside a content part (or on a tool
/// object) and OpenRouter forwards it to Anthropic. `ttl` is omitted for the
/// default 5-minute cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
}

impl CacheControl {
    fn ephemeral() -> Self {
        CacheControl {
            kind: "ephemeral".to_string(),
            ttl: None,
        }
    }
}

/// One content part in the structured (array) message form. We only ever
/// emit `text` parts; the optional `cache_control` is what carries a cache
/// breakpoint on the OpenRouter/Anthropic path.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: String,
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// A message's content. Serializes as a bare JSON string by default; only the
/// specific message that carries a cache breakpoint is promoted to the parts
/// array form. Keeping everything else a bare string preserves prefix
/// stability (the cached bytes must be byte-identical request to request) and
/// avoids providers mirroring a content-block structure back at us.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    /// Flatten back to plain text. Responses always come back as `Text`;
    /// `Parts` is only ever something we constructed, so concatenating its
    /// text parts is a lossless inverse there.
    fn into_text(self) -> String {
        match self {
            MessageContent::Text(s) => s,
            MessageContent::Parts(parts) => parts
                .into_iter()
                .map(|p| p.text)
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    /// Attach a cache breakpoint to this content's last text part,
    /// promoting a bare string to the single-part array form as needed.
    fn set_cache_control(&mut self, cc: CacheControl) {
        match self {
            MessageContent::Text(s) => {
                *self = MessageContent::Parts(vec![ContentPart {
                    kind: "text".to_string(),
                    text: std::mem::take(s),
                    cache_control: Some(cc),
                }]);
            }
            MessageContent::Parts(parts) => {
                if let Some(last_text) = parts.iter_mut().rev().find(|p| p.kind == "text") {
                    last_text.cache_control = Some(cc);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<MessageContent>,
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
    /// Cache breakpoint, set only on the last tool when caching applies.
    /// Sits at the tool-object top level (sibling of `function`) — the
    /// OpenRouter/Anthropic convention, NOT inside `function`.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct ChatToolFunction {
    name: String,
    description: String,
    parameters: Value,
    /// OpenAI strict-mode flag. When `Some(true)`, the model is constrained
    /// to emit arguments matching the schema exactly: every property in
    /// `required`, every nested object closed with `additionalProperties:
    /// false`. Only set on tools that have audited their schema; left
    /// `None` (and thus omitted from the wire) by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
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
    /// OpenRouter-only: tokens written into the cache this turn. Its presence
    /// also signals OpenRouter's combined-report quirk where `cached_tokens`
    /// is `(previous_reads + current_writes)` rather than reads alone.
    #[serde(default)]
    cache_write_tokens: Option<u32>,
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
        let reported_cached = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .or(self.cache_read_input_tokens);
        let cache_write = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cache_write_tokens);
        // OpenRouter quirk: when it reports a nested `cache_write_tokens`,
        // `cached_tokens` is (previous_reads + current_writes), so back the
        // writes out to recover the true cache-read count. Native Anthropic
        // reports reads/creation as already-separate flat fields, which this
        // leaves untouched (no nested cache_write_tokens present there).
        let cached_tokens = match (reported_cached, cache_write) {
            (Some(c), Some(w)) => Some(c.saturating_sub(w)),
            _ => reported_cached,
        };
        let cache_creation_tokens = self.cache_creation_input_tokens.or(cache_write);
        let reasoning_tokens = self
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens);
        TokenUsage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cached_tokens,
            cache_creation_tokens,
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
        // Per-call metadata; the turn's context high-water mark is set when
        // the ReAct accumulator finalizes (see `MetadataAccumulator`).
        context_tokens: None,
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

        let mut openai_messages = convert_runtime_messages(messages);
        let mut openai_tools = convert_tool_definitions(tools);
        apply_anthropic_cache_control(
            &mut openai_messages,
            &mut openai_tools,
            model,
            &self.backend,
        );

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
        // Responses come back as a bare string; flatten to Option<String>
        // so the rest of the pipeline is unchanged.
        let content = content.map(MessageContent::into_text);

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

impl OpenAI {
    /// Models for this backend with the optional pricing carried over from
    /// the YAML config. Separate from the `LLMBackend::list_models` trait
    /// method because the trait keeps a string-only surface that the rest
    /// of the system uses.
    pub fn list_models_with_info(&self) -> Vec<crate::backends::ModelInfo> {
        self.backend
            .models
            .as_ref()
            .map(|models| {
                models
                    .iter()
                    .map(|m| crate::backends::ModelInfo {
                        id: m.name.clone(),
                        price_input: m.price_input,
                        price_output: m.price_output,
                        price_cache_read: m.price_cache_read,
                        input_modalities: Vec::new(),
                        output_modalities: Vec::new(),
                        context_window: m.context_window,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Live-fetch the backend's model catalog via `GET {api_base}/models`.
    /// OpenAI-compatible providers (OpenRouter, vanilla OpenAI, DeepSeek)
    /// expose this endpoint with the shape `{ data: [{ id, pricing? }] }`.
    /// Pricing is optional and read in OpenRouter's $/token format, converted
    /// to $/Mtok to match the YAML-config convention on `ModelInfo`.
    pub async fn fetch_models_from_api(&self) -> Result<Vec<crate::backends::ModelInfo>, LlmError> {
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

        let url = format!("{}/models", api_base.trim_end_matches('/'));
        let timeout = self.backend.request_timeout();

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| LlmError::NetworkError {
                message: format!("client build failed: {e}"),
            })?;

        let resp = client
            .get(&url)
            .bearer_auth(&api_key)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    LlmError::Timeout
                } else {
                    LlmError::NetworkError {
                        message: e.to_string(),
                    }
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::ServerError {
                status: status.as_u16(),
                message: format!("GET {url} returned {status}: {body}"),
            });
        }

        let payload: ModelsResponse = resp.json().await.map_err(|e| LlmError::InvalidRequest {
            message: format!("decode /models response: {e}"),
        })?;

        // OpenRouter quotes prices as $/token (decimal string). ModelInfo
        // carries $/Mtok so the picker can display "$2.50".
        fn parse_per_mtok(s: Option<String>) -> Option<f64> {
            s.and_then(|s| s.parse::<f64>().ok())
                .map(|per_token| per_token * 1_000_000.0)
        }

        Ok(payload
            .data
            .into_iter()
            .map(|m| {
                let pricing = m.pricing.unwrap_or_default();
                let arch = m.architecture.unwrap_or_default();
                // The routed provider's cap is the operative limit; fall back
                // to the model's headline window when it isn't reported.
                let context_window = m
                    .top_provider
                    .and_then(|tp| tp.context_length)
                    .or(m.context_length);
                crate::backends::ModelInfo {
                    id: m.id,
                    price_input: parse_per_mtok(pricing.prompt),
                    price_output: parse_per_mtok(pricing.completion),
                    price_cache_read: parse_per_mtok(pricing.input_cache_read),
                    input_modalities: arch.input_modalities,
                    output_modalities: arch.output_modalities,
                    context_window,
                }
            })
            .collect())
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    pricing: Option<ModelPricing>,
    #[serde(default)]
    architecture: Option<ModelArchitecture>,
    /// OpenRouter's published context window for the model.
    #[serde(default)]
    context_length: Option<u32>,
    /// The provider OpenRouter actually routes to may cap context lower than
    /// the headline `context_length`; prefer this when present.
    #[serde(default)]
    top_provider: Option<TopProvider>,
}

#[derive(Debug, Default, Deserialize)]
struct TopProvider {
    #[serde(default)]
    context_length: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelPricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
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
        let (model, mut messages) =
            convert_chat_context(context, &model_prefix, &self.default_model());
        apply_anthropic_cache_control(&mut messages, &mut [], &model, &self.backend);

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
            .map(MessageContent::into_text)
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
                content: Some(MessageContent::Text(content.clone())),
                tool_calls: None,
                tool_call_id: None,
                extra: Map::new(),
            },
            RuntimeMessage::User(content) => ChatMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text(content.clone())),
                tool_calls: None,
                tool_call_id: None,
                extra: Map::new(),
            },
            RuntimeMessage::Assistant(content) => ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text(content.clone())),
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
                content: content.clone().map(MessageContent::Text),
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
                content: Some(MessageContent::Text(content.clone())),
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
                strict: td.strict.then_some(true),
            },
            cache_control: None,
        })
        .collect()
}

/// Whether inline Anthropic `cache_control` markers should be emitted: only
/// for an `anthropic/…` model on OpenRouter. Every other provider/model on an
/// OpenAI-compatible endpoint caches server-side automatically (and may 400 on
/// unexpected inline markers), so we send nothing and just read usage back.
fn anthropic_cache_control(backend: &Backend, model: &str) -> Option<CacheControl> {
    let is_openrouter = backend
        .api_base
        .as_deref()
        .is_some_and(|b| b.contains("openrouter.ai"))
        || backend
            .name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case("openrouter"));
    if is_openrouter && model.starts_with("anthropic/") {
        Some(CacheControl::ephemeral())
    } else {
        None
    }
}

/// Stamp Anthropic prompt-cache breakpoints onto the assembled request.
///
/// Three breakpoints — last tool → system → latest user message — allocated
/// in that order (cache-invalidation order: the most stable region is covered
/// first) under a hard cap of 4 (Anthropic rejects requests with more). No-op
/// unless [`anthropic_cache_control`] says caching applies. This is the single
/// place the breakpoint policy lives; the `cache_control` struct fields are
/// inert serialization slots it writes into.
fn apply_anthropic_cache_control(
    messages: &mut [ChatMessage],
    tools: &mut [ChatTool],
    model: &str,
    backend: &Backend,
) {
    let Some(cc) = anthropic_cache_control(backend, model) else {
        return;
    };
    let mut remaining: u8 = 4;
    let next = |remaining: &mut u8| -> Option<CacheControl> {
        if *remaining == 0 {
            return None;
        }
        *remaining -= 1;
        Some(cc.clone())
    };

    // 1. End of the tool-schema block.
    if let Some(last_tool) = tools.last_mut()
        && let Some(c) = next(&mut remaining)
    {
        last_tool.cache_control = Some(c);
    }
    // 2. System prompt — head of the stable prefix.
    if let Some(content) = messages
        .iter_mut()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_mut())
        && let Some(c) = next(&mut remaining)
    {
        content.set_cache_control(c);
    }
    // 3. Latest user message — the conversation boundary that intra-turn
    //    tool-call round-trips all share, so it's the best hit point.
    if let Some(content) = messages
        .iter_mut()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_mut())
        && let Some(c) = next(&mut remaining)
    {
        content.set_cache_control(c);
    }
}

/// Convert a ChatContext (legacy, no-tools path) to (model, messages) for a request.
fn convert_chat_context(
    context: &ChatContext,
    model_prefix: &str,
    default_model: &Option<String>,
) -> (String, Vec<ChatMessage>) {
    let mut messages = Vec::new();
    if let Some(prompt) = &context.system_prompt {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(MessageContent::Text(prompt.clone())),
            tool_calls: None,
            tool_call_id: None,
            extra: Map::new(),
        });
    }
    for message in &context.messages {
        messages.push(ChatMessage {
            role: message.role.as_str().to_string(),
            content: Some(MessageContent::Text(message.content.clone())),
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
                ..Default::default()
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
                ..Default::default()
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

    // --- prompt caching ---

    fn openrouter_backend() -> Backend {
        let mut b = Backend::new(crate::config::BackendType::OpenAICompatible);
        b.name = Some("openrouter".to_string());
        b.api_base = Some("https://openrouter.ai/api/v1".to_string());
        b
    }

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(MessageContent::Text(text.to_string())),
            tool_calls: None,
            tool_call_id: None,
            extra: Map::new(),
        }
    }

    fn tool(name: &str) -> ChatTool {
        ChatTool {
            kind: "function",
            function: ChatToolFunction {
                name: name.to_string(),
                description: String::new(),
                parameters: Value::Null,
                strict: None,
            },
            cache_control: None,
        }
    }

    #[test]
    fn cache_control_gating() {
        let or = openrouter_backend();
        assert!(anthropic_cache_control(&or, "anthropic/claude-sonnet-4.6").is_some());
        // Non-Anthropic model on OpenRouter → no inline markers.
        assert!(anthropic_cache_control(&or, "deepseek/deepseek-v4-flash").is_none());
        assert!(anthropic_cache_control(&or, "inclusionai/ring-2.6-1t:free").is_none());
        // Anthropic-named model on a non-OpenRouter backend → no markers.
        let mut other = Backend::new(crate::config::BackendType::OpenAICompatible);
        other.name = Some("openai".to_string());
        other.api_base = Some("https://api.openai.com/v1".to_string());
        assert!(anthropic_cache_control(&other, "anthropic/claude-sonnet-4.6").is_none());
    }

    #[test]
    fn apply_places_three_breakpoints_for_anthropic_on_openrouter() {
        let mut messages = vec![
            msg("system", "you are helpful"),
            msg("user", "first"),
            msg("assistant", "reply"),
            msg("user", "latest question"),
        ];
        let mut tools = vec![tool("a"), tool("b")];
        apply_anthropic_cache_control(
            &mut messages,
            &mut tools,
            "anthropic/claude-sonnet-4.6",
            &openrouter_backend(),
        );

        // Last tool only, at the tool-object top level (sibling of function).
        assert!(tools[0].cache_control.is_none());
        let t = serde_json::to_value(&tools[1]).unwrap();
        assert_eq!(t["cache_control"]["type"], "ephemeral");
        assert!(t["function"].get("cache_control").is_none());

        // System promoted to a parts array with the marker.
        let sys = serde_json::to_value(&messages[0]).unwrap();
        assert_eq!(sys["content"][0]["type"], "text");
        assert_eq!(sys["content"][0]["cache_control"]["type"], "ephemeral");

        // The *latest* user message is marked; the earlier one stays a
        // bare string.
        assert_eq!(
            serde_json::to_value(&messages[1]).unwrap()["content"],
            Value::String("first".into())
        );
        let last_user = serde_json::to_value(&messages[3]).unwrap();
        assert_eq!(
            last_user["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // Assistant left untouched (bare string).
        assert_eq!(
            serde_json::to_value(&messages[2]).unwrap()["content"],
            Value::String("reply".into())
        );
    }

    #[test]
    fn apply_is_noop_when_caching_does_not_apply() {
        let mut messages = vec![msg("system", "s"), msg("user", "u")];
        let mut tools = vec![tool("a")];
        apply_anthropic_cache_control(
            &mut messages,
            &mut tools,
            "deepseek/deepseek-v4-flash",
            &openrouter_backend(),
        );
        assert!(tools[0].cache_control.is_none());
        // Content stays a bare string — no structural churn.
        for m in &messages {
            let v = serde_json::to_value(m).unwrap();
            assert!(v["content"].is_string());
        }
    }

    #[test]
    fn text_content_serializes_as_bare_string() {
        let v = serde_json::to_value(msg("user", "hello")).unwrap();
        assert_eq!(v["content"], Value::String("hello".into()));
    }

    #[test]
    fn cached_tokens_back_out_openrouter_cache_write_double_count() {
        // OpenRouter quirk: cached_tokens = previous_reads + current_writes.
        let u = Usage {
            prompt_tokens: 1000,
            completion_tokens: 10,
            total_tokens: 1010,
            cost: None,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(90),
                cache_write_tokens: Some(30),
            }),
            completion_tokens_details: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        let t = u.into_token_usage();
        assert_eq!(t.cached_tokens, Some(60), "writes backed out of reads");
        assert_eq!(
            t.cache_creation_tokens,
            Some(30),
            "nested cache_write surfaced as creation"
        );
    }

    #[test]
    fn anthropic_flat_cache_fields_unaffected_by_double_count_fix() {
        // Native Anthropic reports reads/creation already separate and has no
        // nested cache_write_tokens, so the subtraction must not fire.
        let u = Usage {
            prompt_tokens: 300,
            completion_tokens: 100,
            total_tokens: 400,
            cost: None,
            prompt_tokens_details: None,
            completion_tokens_details: None,
            cache_read_input_tokens: Some(50),
            cache_creation_input_tokens: Some(10),
        };
        let t = u.into_token_usage();
        assert_eq!(t.cached_tokens, Some(50));
        assert_eq!(t.cache_creation_tokens, Some(10));
    }

    // --- tool definitions: strict-mode opt-in ---

    #[test]
    fn convert_tool_definitions_omits_strict_when_false() {
        let defs = vec![ToolDefinition {
            name: "loose".into(),
            description: "loose tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            strict: false,
        }];
        let wire = convert_tool_definitions(&defs);
        let json = serde_json::to_value(&wire[0]).unwrap();
        assert!(
            json["function"].get("strict").is_none(),
            "expected `strict` to be omitted when ToolDefinition.strict=false, got: {json}"
        );
    }

    #[test]
    fn convert_tool_definitions_sets_strict_when_opted_in() {
        let defs = vec![ToolDefinition {
            name: "tight".into(),
            description: "tight tool".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"x": {"type": "string"}},
                "required": ["x"],
                "additionalProperties": false
            }),
            strict: true,
        }];
        let wire = convert_tool_definitions(&defs);
        let json = serde_json::to_value(&wire[0]).unwrap();
        assert_eq!(json["function"]["strict"], serde_json::Value::Bool(true));
    }
}
