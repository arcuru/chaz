//! Native Anthropic Messages API backend (`api.anthropic.com`).
//!
//! Mirrors [`crate::openai`] in shape but speaks Anthropic's own wire format
//! rather than the OpenAI chat-completions shape. The key differences handled
//! here:
//!
//! - `system` is a top-level field, not a `role: "system"` message.
//! - Tool parameters live under `input_schema`, not `parameters`.
//! - MCP tool names with `__` separator are sent verbatim (Anthropic accepts them).
//! - The response is a `content` block array (text + tool_use), not flat
//!   `tool_calls`.
//! - Prompt-cache breakpoints are first-class `cache_control` fields on
//!   system/tool/content blocks; the *placement policy* is shared with the
//!   OpenAI-compatible backend via [`crate::cache`].
//!
//! Uses raw `reqwest` (no SDK) — the same transport `openai.rs` uses for its
//! `/models` fetch — since there is no official Anthropic Rust SDK.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::Duration;

use crate::{
    backends::{ChatContext, LLMBackend, MessageRole, ModelInfo},
    cache::{CacheControl, CacheRegion},
    config::Backend,
    error::LlmError,
    runtime::{LLMResponse, ResponseMetadata, RuntimeMessage, TokenUsage, ToolCallRequest},
    security::SecretStore,
    tool::ToolDefinition,
};

/// Anthropic API version header value (the stable Messages API).
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Default API base when the backend config doesn't override it.
const DEFAULT_API_BASE: &str = "https://api.anthropic.com/v1";

/// How many times to double `max_tokens` when a truncated tool-use is
/// detected before giving up and surfacing the truncation.
const MAX_MAX_TOKENS_RAISES: u32 = 3;

/// `retry-after` values above this are not honored as an absolute floor —
/// they fall back to exponential backoff. Matches the official SDKs.
const MAX_RETRY_AFTER_SECS: f64 = 60.0;

/// Handle connections to the native Anthropic Messages API.
pub struct Anthropic {
    /// Backend config (api_key cleared at startup — resolved via secret store).
    backend: Backend,
    /// Secret store for host-boundary key injection.
    secrets: SecretStore,
}

// ================================================================
// Wire types — Anthropic Messages API
// ================================================================

#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    /// Required by Anthropic — unlike OpenAI, there is no server-side default.
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a [TextBlock]>,
    messages: &'a [ReqMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ReqTool]>,
}

/// A `system`-field text block (Anthropic's top-level system prompt shape).
#[derive(Debug, Serialize)]
struct TextBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize)]
struct ReqMessage {
    role: String,
    content: Vec<ReqBlock>,
}

/// A content block on a request message. Serialized with an internal `type`
/// tag matching Anthropic's block grammar.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ReqBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

impl ReqBlock {
    /// Attach a cache breakpoint to a block that supports one (`tool_use`
    /// blocks carry none — they're never the tail of a user turn).
    fn set_cache_control(&mut self, cc: CacheControl) {
        match self {
            ReqBlock::Text { cache_control, .. } | ReqBlock::ToolResult { cache_control, .. } => {
                *cache_control = Some(cc)
            }
            ReqBlock::ToolUse { .. } => {}
        }
    }
}

#[derive(Debug, Serialize)]
struct ReqTool {
    name: String,
    description: String,
    /// Anthropic's key is `input_schema`, not OpenAI's `parameters`.
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Vec<RespBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthUsage>,
}

impl MessagesResponse {
    /// Whether the provider truncated this response. Anthropic reports a
    /// truncated generation as `stop_reason: "max_tokens"` (output budget
    /// exhausted) or `"model_context_window_exceeded"` (context window full).
    fn is_truncated(&self) -> bool {
        matches!(
            self.stop_reason.as_deref(),
            Some("max_tokens") | Some("model_context_window_exceeded")
        )
    }

    /// Whether any content block is a tool-use request.
    fn has_tool_use(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, RespBlock::ToolUse { .. }))
    }
}

/// A content block on a response. `Other` catches blocks chaz doesn't consume
/// (thinking, redacted_thinking, and any future block types) so they don't
/// fail the parse.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RespBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct AnthUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

impl AnthUsage {
    /// Project Anthropic usage onto chaz's normalized [`TokenUsage`].
    ///
    /// Anthropic's `input_tokens` counts only the *uncached* prompt; cache
    /// reads/writes are reported separately. chaz's `TokenUsage` treats
    /// `prompt_tokens` as the whole prompt with `cached_tokens` a subset of it,
    /// so fold the cache counts back in. Anthropic reports no cost.
    fn into_token_usage(self) -> TokenUsage {
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_creation = self.cache_creation_input_tokens.unwrap_or(0);
        let prompt_tokens = self
            .input_tokens
            .saturating_add(cache_read)
            .saturating_add(cache_creation);
        TokenUsage {
            prompt_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: prompt_tokens.saturating_add(self.output_tokens),
            cached_tokens: self.cache_read_input_tokens,
            cache_creation_tokens: self.cache_creation_input_tokens,
            reasoning_tokens: None,
            cost_usd: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    /// Context window, when the catalog reports it (newer `/v1/models`).
    #[serde(default)]
    max_input_tokens: Option<u32>,
    /// Maximum output tokens — the ceiling for the Messages `max_tokens`
    /// parameter. Distinct from `max_input_tokens`, which is the context
    /// window (1M for current models) and must never cap the output budget.
    #[serde(default)]
    max_tokens: Option<u32>,
}

impl Anthropic {
    pub fn new(backend: &Backend, secrets: &SecretStore) -> Self {
        Anthropic {
            backend: backend.clone(),
            secrets: secrets.clone(),
        }
    }

    /// Resolve the API key from the secret store (by reference), falling back
    /// to the raw `api_key` field for backward compatibility.
    fn api_key(&self) -> Result<String, LlmError> {
        self.backend
            .api_key_ref
            .as_ref()
            .and_then(|r| self.secrets.get(r))
            .or_else(|| self.backend.api_key.clone())
            .ok_or_else(|| LlmError::Configuration {
                message: "API key not configured".to_string(),
            })
    }

    fn api_base(&self) -> String {
        self.backend
            .api_base
            .clone()
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
    }

    fn http_client(&self) -> Result<reqwest::Client, LlmError> {
        reqwest::Client::builder()
            .timeout(self.backend.request_timeout())
            .build()
            .map_err(|e| LlmError::NetworkError {
                message: format!("client build failed: {e}"),
            })
    }

    /// Execute a single LLM call with tool definitions, returning a structured
    /// response. Called by the runtime's ReAct loop.
    async fn chat_with_tools_impl(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<LLMResponse, LlmError> {
        let api_key = self.api_key()?;
        let client = self.http_client()?;

        let mut req_tools = convert_tool_definitions(tools);
        let (mut system, mut req_messages) = convert_runtime_messages(messages);
        apply_cache_control(&mut system, &mut req_messages, &mut req_tools);

        let url = format!("{}/messages", self.api_base().trim_end_matches('/'));
        let mut max_tokens = self.backend.max_output_tokens();
        let initial_max_tokens = max_tokens;
        let mut raise_attempts: u32 = 0;
        // Lazily-discovered model output ceiling. Fetched once, on the first
        // truncated tool-use, so a normal completed response costs no extra
        // API call. `None` before that lookup and `None` after it when the
        // endpoint is missing or omits the field (custom/proxy endpoints) —
        // the doubling bound and the 400 reclassification below are then the
        // only guards.
        let mut output_ceiling: Option<u32> = None;
        let mut ceiling_resolved = false;

        loop {
            let request = MessagesRequest {
                model,
                max_tokens,
                system: system.as_deref(),
                messages: &req_messages,
                tools: if req_tools.is_empty() {
                    None
                } else {
                    Some(&req_tools)
                },
            };

            let http_resp = client
                .post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&request)
                .send()
                .await
                .map_err(map_reqwest_err)?;

            let status = http_resp.status();
            if !status.is_success() {
                let retry_after = parse_retry_after(http_resp.headers());
                let body = http_resp.text().await.unwrap_or_default();
                // A 400 on a retry whose only change from the preceding 200 is
                // a raised `max_tokens` is the provider rejecting that budget
                // (the model's real output limit sits below it). Surface the
                // truncation rather than an opaque invalid-request error.
                if status.as_u16() == 400 && max_tokens > initial_max_tokens {
                    tracing::warn!(
                        model,
                        max_tokens,
                        body = %body,
                        "Anthropic rejected the raised max_tokens; surfacing truncation"
                    );
                    return Err(LlmError::TruncatedOutput {
                        reason: "max_tokens".to_string(),
                    });
                }
                return Err(LlmError::from_http_status_with_retry_after(
                    status.as_u16(),
                    body,
                    retry_after,
                ));
            }

            let response: MessagesResponse =
                http_resp
                    .json()
                    .await
                    .map_err(|e| LlmError::InvalidRequest {
                        message: format!("decode messages response: {e}"),
                    })?;

            // A truncated tool-use is not runnable — its `input` is incomplete
            // and would otherwise be coerced to `{}`. Re-request with a higher
            // max_tokens (the documented recovery), then surface truncation if
            // the raise budget is exhausted.
            if response.is_truncated() {
                let can_raise = response.stop_reason.as_deref() == Some("max_tokens")
                    && response.has_tool_use()
                    && raise_attempts < MAX_MAX_TOKENS_RAISES;
                if can_raise {
                    if !ceiling_resolved {
                        ceiling_resolved = true;
                        output_ceiling = self
                            .fetch_model_max_output_tokens(&client, &api_key, model)
                            .await;
                    }
                    let doubled = max_tokens.saturating_mul(2);
                    let raised = match output_ceiling {
                        Some(cap) => doubled.min(cap),
                        None => doubled,
                    };
                    if raised > max_tokens {
                        raise_attempts += 1;
                        max_tokens = raised;
                        tracing::warn!(
                            model,
                            max_tokens,
                            raise_attempts,
                            "Anthropic response truncated mid-tool-use; retrying with higher max_tokens"
                        );
                        continue;
                    }
                }
                return Err(LlmError::TruncatedOutput {
                    reason: response.stop_reason.unwrap_or_default(),
                });
            }

            let MessagesResponse {
                id,
                model: response_model,
                content: blocks,
                usage,
                stop_reason: _,
            } = response;

            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();
            for block in blocks {
                match block {
                    RespBlock::Text { text } => text_parts.push(text),
                    RespBlock::ToolUse { id, name, input } => {
                        tool_calls.push(ToolCallRequest {
                            id,
                            name,
                            arguments: input.to_string(),
                        });
                    }
                    RespBlock::Other => {}
                }
            }
            let content = if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join(""))
            };

            let metadata = build_metadata(id, response_model, usage, model);

            tracing::debug!(
                "Anthropic response: content={:?} tool_calls={} usage={:?}",
                content.as_deref().map(|c| &c[..c.len().min(100)]),
                tool_calls.len(),
                metadata.as_ref().map(|m| &m.usage),
            );

            if !tool_calls.is_empty() {
                // Anthropic thinking blocks aren't requested, so there is nothing
                // provider-specific to echo back on the follow-up request.
                return Ok(LLMResponse::ToolCalls {
                    content,
                    tool_calls,
                    provider_extra: Map::new(),
                    metadata,
                });
            }

            return Ok(LLMResponse::Text {
                content: content.unwrap_or_default(),
                metadata,
            });
        }
    }

    /// Models for this backend with pricing carried from YAML config.
    pub fn list_models_with_info(&self) -> Vec<ModelInfo> {
        self.backend
            .models
            .as_ref()
            .map(|models| {
                models
                    .iter()
                    .map(|m| ModelInfo {
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

    /// Live-fetch the model catalog via `GET {api_base}/models`. Anthropic's
    /// catalog reports ids and (on newer versions) `max_input_tokens`, but no
    /// pricing, so prices stay `None`.
    pub async fn fetch_models_from_api(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let api_key = self.api_key()?;
        let client = self.http_client()?;
        let url = format!("{}/models", self.api_base().trim_end_matches('/'));

        let resp = client
            .get(&url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .send()
            .await
            .map_err(map_reqwest_err)?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::from_http_status_with_retry_after(
                status.as_u16(),
                format!("GET {url} returned {status}: {body}"),
                retry_after,
            ));
        }

        let payload: ModelsResponse = resp.json().await.map_err(|e| LlmError::InvalidRequest {
            message: format!("decode /models response: {e}"),
        })?;

        Ok(payload
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id,
                price_input: None,
                price_output: None,
                price_cache_read: None,
                input_modalities: Vec::new(),
                output_modalities: Vec::new(),
                context_window: m.max_input_tokens,
            })
            .collect())
    }

    /// Lazily fetch one model's maximum output tokens via
    /// `GET {api_base}/models/{model}` — the endpoint accepts an id or alias,
    /// and `max_tokens` is the ceiling for the Messages `max_tokens` parameter
    /// (not to be confused with `max_input_tokens`, the context window).
    ///
    /// Returns `None` when the fetch fails or the field is absent (custom or
    /// proxy endpoints that don't implement the per-model endpoint, older
    /// catalogs). Callers fall back to bounded doubling + a 400
    /// reclassification in that case.
    async fn fetch_model_max_output_tokens(
        &self,
        client: &reqwest::Client,
        api_key: &str,
        model: &str,
    ) -> Option<u32> {
        let url = model_url(&self.api_base(), model)?;
        let resp = client
            .get(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let entry: ModelEntry = resp.json().await.ok()?;
        entry.max_tokens
    }
}

impl LLMBackend for Anthropic {
    fn list_models(&self) -> Vec<String> {
        self.backend
            .models
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.name)
            .collect()
    }

    fn default_model(&self) -> Option<String> {
        self.backend
            .models
            .as_ref()
            .and_then(|models| models.first())
            .map(|m| m.name.clone())
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

    /// Execute a simple chat request (no tools). Used by Matrix commands and
    /// /compact. Reuses the tools path with an empty tool set.
    async fn execute(&self, context: &ChatContext) -> Result<String, LlmError> {
        let model_prefix = self
            .backend
            .name
            .clone()
            .unwrap_or_else(|| "anthropic".to_string());
        let (model, runtime_messages) =
            convert_chat_context(context, &model_prefix, &self.default_model());
        match self
            .chat_with_tools_impl(&runtime_messages, &[], &model)
            .await?
        {
            LLMResponse::Text { content, .. } => Ok(content),
            LLMResponse::ToolCalls { content, .. } => Ok(content.unwrap_or_default()),
        }
    }
}

/// Build `ResponseMetadata`, falling back to the requested model name if the
/// backend didn't echo one. Returns `None` when nothing useful came back.
fn build_metadata(
    id: Option<String>,
    model: Option<String>,
    usage: Option<AnthUsage>,
    requested_model: &str,
) -> Option<ResponseMetadata> {
    if id.is_none() && model.is_none() && usage.is_none() {
        return None;
    }
    Some(ResponseMetadata {
        model: model.unwrap_or_else(|| requested_model.to_string()),
        provider: None,
        response_id: id,
        usage: usage.map(AnthUsage::into_token_usage).unwrap_or_default(),
        context_tokens: None,
        extra: Map::new(),
    })
}

fn map_reqwest_err(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::Timeout
    } else if e.is_connect() {
        LlmError::NetworkError {
            message: e.to_string(),
        }
    } else if let Some(status) = e.status() {
        LlmError::from_http_status(status.as_u16(), e.to_string())
    } else {
        LlmError::NetworkError {
            message: e.to_string(),
        }
    }
}

/// Build `GET {api_base}/models/{model}` with the model as a single, escaped
/// path segment, so an id needing escaping (or a slash-bearing alias from a
/// proxy catalog) can't reshape the URL. The official SDKs percent-encode this
/// path parameter for the same reason. `None` if `api_base` is not a valid
/// base URL.
fn model_url(api_base: &str, model: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(api_base).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .push("models")
        .push(model);
    Some(url)
}

/// Parse the `retry-after` response header into a retry floor.
///
/// Honors the integer-seconds form and the RFC 7231 HTTP-date form, but only
/// when `0 < retry_after <= 60` — the official SDKs treat larger values as
/// "unreasonable" and fall back to exponential backoff, which `backoff_delay`
/// already provides. Returns `None` when the header is absent, malformed, or
/// outside that range.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();

    let secs = if let Ok(n) = raw.parse::<u64>() {
        n as f64
    } else if let Ok(when) = chrono::DateTime::parse_from_rfc2822(raw) {
        when.with_timezone(&chrono::Utc)
            .signed_duration_since(chrono::Utc::now())
            .num_seconds() as f64
    } else {
        return None;
    };

    if secs > 0.0 && secs <= MAX_RETRY_AFTER_SECS {
        Some(Duration::from_secs_f64(secs))
    } else {
        None
    }
}

/// Append `blocks` to `turns`, coalescing into the last turn when it shares
/// `role`. Anthropic wants all consecutive same-role content (e.g. several
/// tool_result blocks after one assistant turn) in a single message.
fn push_turn(turns: &mut Vec<ReqMessage>, role: &str, blocks: Vec<ReqBlock>) {
    if let Some(last) = turns.last_mut()
        && last.role == role
    {
        last.content.extend(blocks);
    } else {
        turns.push(ReqMessage {
            role: role.to_string(),
            content: blocks,
        });
    }
}

fn text_block(text: String) -> ReqBlock {
    ReqBlock::Text {
        text,
        cache_control: None,
    }
}

/// Convert RuntimeMessages into (top-level `system`, coalesced messages).
/// System messages are pulled out of the message stream into Anthropic's
/// top-level `system` field.
fn convert_runtime_messages(
    messages: &[RuntimeMessage],
) -> (Option<Vec<TextBlock>>, Vec<ReqMessage>) {
    let mut system_texts: Vec<String> = Vec::new();
    let mut turns: Vec<ReqMessage> = Vec::new();

    for msg in messages {
        match msg {
            RuntimeMessage::System(content) => system_texts.push(content.clone()),
            RuntimeMessage::User(content) => {
                push_turn(&mut turns, "user", vec![text_block(content.clone())]);
            }
            RuntimeMessage::Assistant(content) => {
                push_turn(&mut turns, "assistant", vec![text_block(content.clone())]);
            }
            RuntimeMessage::AssistantToolCalls {
                content,
                tool_calls,
                ..
            } => {
                let mut blocks = Vec::new();
                if let Some(c) = content
                    && !c.is_empty()
                {
                    blocks.push(text_block(c.clone()));
                }
                for tc in tool_calls {
                    blocks.push(ReqBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        // coding: arguments should be valid JSON from the model;
                        // fall back to an empty object rather than 400 the turn.
                        input: serde_json::from_str(&tc.arguments)
                            .unwrap_or_else(|_| Value::Object(Map::new())),
                    });
                }
                push_turn(&mut turns, "assistant", blocks);
            }
            RuntimeMessage::ToolResult { call_id, content } => {
                push_turn(
                    &mut turns,
                    "user",
                    vec![ReqBlock::ToolResult {
                        tool_use_id: call_id.clone(),
                        content: content.clone(),
                        cache_control: None,
                    }],
                );
            }
        }
    }

    let system = if system_texts.is_empty() {
        None
    } else {
        Some(vec![TextBlock {
            kind: "text",
            text: system_texts.join("\n\n"),
            cache_control: None,
        }])
    };
    (system, turns)
}

/// Convert ToolDefinitions to Anthropic's tool shape.
fn convert_tool_definitions(tools: &[ToolDefinition]) -> Vec<ReqTool> {
    tools
        .iter()
        .map(|td| ReqTool {
            name: td.name.clone(),
            description: td.description.clone(),
            input_schema: td.parameters.clone(),
            cache_control: None,
        })
        .collect()
}

/// Stamp first-class Anthropic prompt-cache breakpoints onto the request.
/// Always applies (native Anthropic caching is unconditional) — the placement
/// policy is shared with the OpenAI-compatible backend via [`crate::cache`].
fn apply_cache_control(
    system: &mut Option<Vec<TextBlock>>,
    messages: &mut [ReqMessage],
    tools: &mut [ReqTool],
) {
    let cc = CacheControl::ephemeral();
    let mut remaining = crate::cache::MAX_BREAKPOINTS;
    for region in crate::cache::CACHE_PLAN {
        if remaining == 0 {
            break;
        }
        let placed = match region {
            CacheRegion::LastTool => {
                if let Some(last_tool) = tools.last_mut() {
                    last_tool.cache_control = Some(cc.clone());
                    true
                } else {
                    false
                }
            }
            CacheRegion::System => {
                if let Some(block) = system.as_mut().and_then(|s| s.last_mut()) {
                    block.cache_control = Some(cc.clone());
                    true
                } else {
                    false
                }
            }
            CacheRegion::LatestUser => {
                if let Some(block) = messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == "user")
                    .and_then(|m| m.content.last_mut())
                {
                    block.set_cache_control(cc.clone());
                    true
                } else {
                    false
                }
            }
        };
        if placed {
            remaining -= 1;
        }
    }
}

/// Convert a ChatContext (legacy, no-tools path) to (model, runtime messages).
fn convert_chat_context(
    context: &ChatContext,
    model_prefix: &str,
    default_model: &Option<String>,
) -> (String, Vec<RuntimeMessage>) {
    let mut msgs = Vec::new();
    if let Some(prompt) = &context.system_prompt {
        msgs.push(RuntimeMessage::System(prompt.clone()));
    }
    for message in &context.messages {
        msgs.push(match message.role {
            MessageRole::System => RuntimeMessage::System(message.content.clone()),
            MessageRole::User => RuntimeMessage::User(message.content.clone()),
            MessageRole::Assistant => RuntimeMessage::Assistant(message.content.clone()),
        });
    }
    let mut model = context.model.clone().unwrap_or_default();
    model = model
        .trim_start_matches(&format!("{model_prefix}:"))
        .to_string();
    if model.is_empty() {
        model = default_model.clone().unwrap_or_default();
    }
    (model, msgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_tool_definitions_passes_mcp_names_verbatim() {
        let defs = vec![ToolDefinition {
            name: "mcp__server__tool".into(),
            description: "test tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            strict: false,
        }];
        let wire = convert_tool_definitions(&defs);
        assert_eq!(wire[0].name, "mcp__server__tool");
    }

    #[test]
    fn convert_tool_definitions_preserves_input_schema() {
        let defs = vec![ToolDefinition {
            name: "mcp__fs__read".into(),
            description: "read".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            strict: false,
        }];
        let wire = convert_tool_definitions(&defs);
        assert_eq!(wire[0].name, "mcp__fs__read");
        assert_eq!(wire[0].input_schema, defs[0].parameters);
    }

    #[test]
    fn system_pulled_out_and_tool_results_coalesced() {
        let messages = vec![
            RuntimeMessage::System("be helpful".into()),
            RuntimeMessage::User("hi".into()),
            RuntimeMessage::AssistantToolCalls {
                content: None,
                tool_calls: vec![
                    ToolCallRequest {
                        id: "a".into(),
                        name: "mcp__fs__read".into(),
                        arguments: "{\"path\":\"x\"}".into(),
                    },
                    ToolCallRequest {
                        id: "b".into(),
                        name: "mcp__fs__stat".into(),
                        arguments: "{}".into(),
                    },
                ],
                provider_extra: Map::new(),
            },
            RuntimeMessage::ToolResult {
                call_id: "a".into(),
                content: "r1".into(),
            },
            RuntimeMessage::ToolResult {
                call_id: "b".into(),
                content: "r2".into(),
            },
        ];
        let (system, turns) = convert_runtime_messages(&messages);

        // System is top-level, not a message.
        let sys = system.expect("system present");
        assert_eq!(sys[0].text, "be helpful");

        // user, assistant(2 tool_use), user(2 tool_result coalesced into one).
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].content.len(), 2);
        assert_eq!(turns[2].role, "user");
        assert_eq!(turns[2].content.len(), 2, "tool results coalesced");

        // tool_use names are passed through verbatim.
        let v = serde_json::to_value(&turns[1]).unwrap();
        assert_eq!(v["content"][0]["name"], "mcp__fs__read");
        assert_eq!(v["content"][0]["input"]["path"], "x");
    }

    #[test]
    fn usage_folds_cache_tokens_into_prompt() {
        let u = AnthUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: Some(40),
            cache_creation_input_tokens: Some(10),
        };
        let t = u.into_token_usage();
        // prompt = uncached input + reads + writes.
        assert_eq!(t.prompt_tokens, 150);
        assert_eq!(t.completion_tokens, 50);
        assert_eq!(t.total_tokens, 200);
        assert_eq!(t.cached_tokens, Some(40));
        assert_eq!(t.cache_creation_tokens, Some(10));
        assert_eq!(t.cost_usd, None);
    }

    #[test]
    fn cache_control_marks_system_last_tool_and_latest_user() {
        let (mut system, mut messages) = convert_runtime_messages(&[
            RuntimeMessage::System("s".into()),
            RuntimeMessage::User("first".into()),
            RuntimeMessage::Assistant("reply".into()),
            RuntimeMessage::User("latest".into()),
        ]);
        let mut tools = convert_tool_definitions(&[
            ToolDefinition {
                name: "a".into(),
                description: String::new(),
                parameters: Value::Null,
                strict: false,
            },
            ToolDefinition {
                name: "b".into(),
                description: String::new(),
                parameters: Value::Null,
                strict: false,
            },
        ]);
        apply_cache_control(&mut system, &mut messages, &mut tools);

        // Only the last tool is marked.
        assert!(tools[0].cache_control.is_none());
        assert!(tools[1].cache_control.is_some());
        // System block marked.
        assert!(system.unwrap()[0].cache_control.is_some());
        // Latest user marked, earlier user untouched.
        let first = serde_json::to_value(&messages[0]).unwrap();
        assert!(first["content"][0].get("cache_control").is_none());
        let latest = serde_json::to_value(messages.last().unwrap()).unwrap();
        assert_eq!(latest["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn response_block_ignores_unknown_types() {
        // Thinking / redacted blocks must not fail the parse.
        let json = serde_json::json!([
            {"type": "thinking", "thinking": "hmm"},
            {"type": "text", "text": "answer"},
            {"type": "tool_use", "id": "t1", "name": "mcp-fs-read", "input": {"path": "x"}}
        ]);
        let blocks: Vec<RespBlock> = serde_json::from_value(json).unwrap();
        assert!(matches!(blocks[0], RespBlock::Other));
        assert!(matches!(blocks[1], RespBlock::Text { .. }));
        assert!(matches!(blocks[2], RespBlock::ToolUse { .. }));
    }

    #[test]
    fn messages_response_parses_stop_reason_and_detects_truncation() {
        let json = serde_json::json!({
            "id": "msg_1",
            "model": "claude-5",
            "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "partial"}],
            "usage": {"input_tokens": 10, "output_tokens": 8192}
        });
        let resp: MessagesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.stop_reason.as_deref(), Some("max_tokens"));
        assert!(resp.is_truncated());
        assert!(!resp.has_tool_use());
    }

    #[test]
    fn messages_response_context_window_exceeded_is_truncated() {
        let json = serde_json::json!({
            "stop_reason": "model_context_window_exceeded",
            "content": []
        });
        let resp: MessagesResponse = serde_json::from_value(json).unwrap();
        assert!(resp.is_truncated());
        assert!(!resp.has_tool_use());
    }

    #[test]
    fn messages_response_tool_use_stop_reason_is_not_truncated() {
        let json = serde_json::json!({
            "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "t1", "name": "mcp__fs__read", "input": {"path": "x"}}]
        });
        let resp: MessagesResponse = serde_json::from_value(json).unwrap();
        assert!(!resp.is_truncated());
        assert!(resp.has_tool_use());
    }

    #[test]
    fn messages_response_missing_stop_reason_is_not_truncated() {
        let json = serde_json::json!({
            "content": [{"type": "text", "text": "done"}]
        });
        let resp: MessagesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.stop_reason, None);
        assert!(!resp.is_truncated());
    }

    #[test]
    fn model_url_escapes_the_model_segment() {
        // A plain id is a plain path segment.
        assert_eq!(
            model_url("https://api.anthropic.com/v1", "claude-5")
                .unwrap()
                .as_str(),
            "https://api.anthropic.com/v1/models/claude-5"
        );

        // A trailing slash on the base does not produce an empty segment.
        assert_eq!(
            model_url("https://api.anthropic.com/v1/", "claude-5")
                .unwrap()
                .as_str(),
            "https://api.anthropic.com/v1/models/claude-5"
        );

        // A slash-bearing alias stays one segment instead of becoming a
        // deeper path, and a dot-segment cannot climb out of /models/.
        assert_eq!(
            model_url("https://proxy.example/v1", "vendor/claude-5")
                .unwrap()
                .as_str(),
            "https://proxy.example/v1/models/vendor%2Fclaude-5"
        );
        assert_eq!(
            model_url("https://proxy.example/v1", "../messages")
                .unwrap()
                .as_str(),
            "https://proxy.example/v1/models/..%2Fmessages"
        );
    }

    #[test]
    fn parse_retry_after_integer_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));
    }

    #[test]
    fn parse_retry_after_over_sixty_falls_back_to_exponential() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "120".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_http_date() {
        let future = chrono::Utc::now() + chrono::Duration::seconds(45);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            future.to_rfc2822().parse().unwrap(),
        );
        let d = parse_retry_after(&headers).expect("http-date retry-after parses");
        // Allow slack for the parse-to-now comparison.
        assert!(
            d >= Duration::from_secs(40) && d <= Duration::from_secs(60),
            "unexpected delay {d:?}"
        );
    }

    #[test]
    fn parse_retry_after_missing_or_malformed_is_none() {
        assert_eq!(parse_retry_after(&reqwest::header::HeaderMap::new()), None);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);
    }

    async fn test_secrets() -> SecretStore {
        let (_instance, mut user) = eidetica::Instance::create_backend(
            Box::new(eidetica::backend::database::InMemory::new()),
            eidetica::NewUser::passwordless("t"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let mut doc = eidetica::crdt::Doc::new();
        doc.set("name", "test");
        let db = user.create_database(doc, &key).await.unwrap();
        SecretStore::new(db).await
    }

    /// Build an `Anthropic` pointed at `server`, with `max_tokens` as the
    /// configured output budget (the 8192 backend default when `None`).
    async fn anthropic_for(server: &wiremock::MockServer, max_tokens: Option<u32>) -> Anthropic {
        let secrets = test_secrets().await;
        let mut backend = Backend::new(crate::config::BackendType::Anthropic);
        backend.api_base = Some(server.uri());
        backend.api_key = Some("test-key".into());
        backend.max_tokens = max_tokens;
        Anthropic::new(&backend, &secrets)
    }

    fn read_tool() -> ToolDefinition {
        ToolDefinition {
            name: "mcp__fs__read".into(),
            description: "read".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            strict: false,
        }
    }

    /// The `max_tokens` value carried by a `/messages` request body.
    fn max_tokens_of(req: &wiremock::Request) -> u32 {
        serde_json::from_slice::<serde_json::Value>(&req.body).unwrap()["max_tokens"]
            .as_u64()
            .unwrap() as u32
    }

    /// The `/messages` POST requests, excluding the lazy `GET /models/{model}`
    /// lookup the retry path issues on the first truncated tool-use.
    fn post_requests(requests: &[wiremock::Request]) -> Vec<&wiremock::Request> {
        requests
            .iter()
            .filter(|r| r.method == reqwest::Method::POST)
            .collect()
    }

    /// A truncated-mid-tool-use response body, replayed by the retry tests.
    fn truncated_tool_use_body() -> serde_json::Value {
        serde_json::json!({
            "id": "msg-trunc",
            "model": "claude-5",
            "stop_reason": "max_tokens",
            "content": [{"type": "tool_use", "id": "t1", "name": "mcp__fs__read", "input": {}}],
            "usage": {"input_tokens": 10, "output_tokens": 8192}
        })
    }

    #[test]
    fn model_entry_distinguishes_output_cap_from_context_window() {
        // A catalog entry reporting only the context window must yield no
        // output cap: `max_input_tokens` (1M for current models) is input
        // budget, never the Messages `max_tokens` ceiling.
        let entry: ModelEntry = serde_json::from_value(serde_json::json!({
            "id": "claude-5",
            "max_input_tokens": 1_000_000
        }))
        .unwrap();
        assert_eq!(entry.max_input_tokens, Some(1_000_000));
        assert_eq!(entry.max_tokens, None);

        // When both are reported they parse as separate fields.
        let entry: ModelEntry = serde_json::from_value(serde_json::json!({
            "id": "claude-5",
            "max_tokens": 131_072,
            "max_input_tokens": 1_000_000
        }))
        .unwrap();
        assert_eq!(entry.max_tokens, Some(131_072));
        assert_eq!(entry.max_input_tokens, Some(1_000_000));
    }

    /// Configured at the old 64K ceiling while the live model allows 128K, the
    /// retry must discover the real cap and go to it — the old static 64K
    /// ceiling surfaced truncation without retrying.
    #[tokio::test]
    async fn truncated_tool_use_raises_to_live_model_output_cap() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models/claude-5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "claude-5",
                "max_tokens": 128 * 1024,
                "max_input_tokens": 1_000_000
            })))
            .mount(&server)
            .await;

        let calls = Arc::new(AtomicUsize::new(0));
        struct Responder {
            calls: Arc<AtomicUsize>,
        }
        impl Respond for Responder {
            fn respond(&self, _req: &Request) -> ResponseTemplate {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_json(truncated_tool_use_body())
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "id": "msg-full",
                        "model": "claude-5",
                        "stop_reason": "tool_use",
                        "content": [{"type": "tool_use", "id": "t1", "name": "mcp__fs__read", "input": {"path": "x"}}],
                        "usage": {"input_tokens": 10, "output_tokens": 20}
                    }))
                }
            }
        }

        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(Responder {
                calls: calls.clone(),
            })
            .mount(&server)
            .await;

        let anthropic = anthropic_for(&server, Some(64 * 1024)).await;
        let result = anthropic
            .chat_with_tools_impl(
                &[RuntimeMessage::User("read x".into())],
                &[read_tool()],
                "claude-5",
            )
            .await
            .expect("retry at the live 128K cap resolves to a complete tool-use");

        match result {
            LLMResponse::ToolCalls { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].arguments, "{\"path\":\"x\"}");
            }
            _ => panic!("expected ToolCalls"),
        }

        let requests = server.received_requests().await.unwrap();
        let posts = post_requests(&requests);
        assert_eq!(posts.len(), 2, "one raise from 64K to the live 128K cap");
        assert_eq!(max_tokens_of(posts[0]), 64 * 1024);
        assert_eq!(max_tokens_of(posts[1]), 128 * 1024);
    }

    /// A legacy model whose live cap (32K) is below the old static ceiling
    /// must never be asked for more than it supports.
    #[tokio::test]
    async fn truncated_tool_use_never_exceeds_live_output_cap() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models/claude-5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "claude-5",
                "max_tokens": 32 * 1024,
                "max_input_tokens": 200_000
            })))
            .mount(&server)
            .await;

        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(truncated_tool_use_body()))
            .mount(&server)
            .await;

        let anthropic = anthropic_for(&server, None).await;
        let result = anthropic
            .chat_with_tools_impl(
                &[RuntimeMessage::User("read x".into())],
                &[read_tool()],
                "claude-5",
            )
            .await;

        match result {
            Err(LlmError::TruncatedOutput { reason }) => assert_eq!(reason, "max_tokens"),
            Err(other) => panic!("expected TruncatedOutput, got {other:?}"),
            Ok(_) => panic!("expected TruncatedOutput, got an Ok response"),
        }

        let requests = server.received_requests().await.unwrap();
        let posts = post_requests(&requests);
        // 8192 -> 16384 -> 32768, then the next raise (65536) is capped at
        // 32768 and not sent — the live cap is never exceeded.
        assert_eq!(posts.len(), 3);
        assert_eq!(max_tokens_of(posts[0]), 8192);
        assert_eq!(max_tokens_of(posts[1]), 16 * 1024);
        assert_eq!(max_tokens_of(posts[2]), 32 * 1024);
    }

    /// Without model metadata (endpoint missing) the retry still recovers via
    /// bounded doubling and then surfaces truncation explicitly.
    #[tokio::test]
    async fn truncated_tool_use_without_model_metadata_recovers_bounded() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // No per-model endpoint: the lazy lookup 404s.
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(truncated_tool_use_body()))
            .mount(&server)
            .await;

        let anthropic = anthropic_for(&server, None).await;
        let result = anthropic
            .chat_with_tools_impl(
                &[RuntimeMessage::User("read x".into())],
                &[read_tool()],
                "claude-5",
            )
            .await;

        match result {
            Err(LlmError::TruncatedOutput { reason }) => assert_eq!(reason, "max_tokens"),
            Err(other) => panic!("expected TruncatedOutput, got {other:?}"),
            Ok(_) => panic!("expected TruncatedOutput, got an Ok response"),
        }

        let requests = server.received_requests().await.unwrap();
        let posts = post_requests(&requests);
        // Bounded by MAX_MAX_TOKENS_RAISES: 8192 -> 16384 -> 32768 -> 65536,
        // then the budget is exhausted and truncation surfaces.
        assert_eq!(posts.len(), 4);
        assert_eq!(max_tokens_of(posts[0]), 8192);
        assert_eq!(max_tokens_of(posts[1]), 16 * 1024);
        assert_eq!(max_tokens_of(posts[2]), 32 * 1024);
        assert_eq!(max_tokens_of(posts[3]), 64 * 1024);
    }

    /// A per-model endpoint that answers 200 but omits `max_tokens` (a proxy
    /// or an older catalog) must be treated as "no cap known", not as a cap of
    /// zero: the retry falls back to bounded doubling.
    #[tokio::test]
    async fn truncated_tool_use_with_metadata_lacking_output_cap_falls_back() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // 200, but only the context window — no output cap to raise to.
        Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models/claude-5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "claude-5",
                "max_input_tokens": 1_000_000
            })))
            .mount(&server)
            .await;

        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(truncated_tool_use_body()))
            .mount(&server)
            .await;

        let anthropic = anthropic_for(&server, None).await;
        let result = anthropic
            .chat_with_tools_impl(
                &[RuntimeMessage::User("read x".into())],
                &[read_tool()],
                "claude-5",
            )
            .await;

        match result {
            Err(LlmError::TruncatedOutput { reason }) => assert_eq!(reason, "max_tokens"),
            Err(other) => panic!("expected TruncatedOutput, got {other:?}"),
            Ok(_) => panic!("expected TruncatedOutput, got an Ok response"),
        }

        // Identical to the no-endpoint case: 8192 -> 16384 -> 32768 -> 65536.
        let requests = server.received_requests().await.unwrap();
        let posts = post_requests(&requests);
        assert_eq!(posts.len(), 4);
        assert_eq!(max_tokens_of(posts[3]), 64 * 1024);

        // And the lookup was attempted exactly once, not per raise.
        let gets = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::GET)
            .count();
        assert_eq!(gets, 1, "the model lookup is resolved once per call");
    }

    /// A completed response must not trigger the lazy model lookup — the
    /// `/models/{model}` call is reserved for the truncated-tool-use path.
    #[tokio::test]
    async fn completed_response_makes_no_models_request() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg-full",
                "model": "claude-5",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "done"}],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })))
            .mount(&server)
            .await;

        let anthropic = anthropic_for(&server, None).await;
        let result = anthropic
            .chat_with_tools_impl(
                &[RuntimeMessage::User("hi".into())],
                &[read_tool()],
                "claude-5",
            )
            .await
            .expect("complete response");

        match result {
            LLMResponse::Text { content, .. } => assert_eq!(content, "done"),
            _ => panic!("expected Text"),
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "exactly one POST, no GET /models");
        assert_eq!(requests[0].method, reqwest::Method::POST);
    }

    /// When metadata is unavailable and a raised budget exceeds the model's
    /// real output limit, the resulting 400 surfaces as truncation, not an
    /// opaque invalid-request error.
    #[tokio::test]
    async fn output_limit_rejection_after_raise_surfaces_as_truncation() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let calls = Arc::new(AtomicUsize::new(0));
        struct Responder {
            calls: Arc<AtomicUsize>,
        }
        impl Respond for Responder {
            fn respond(&self, _req: &Request) -> ResponseTemplate {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_json(truncated_tool_use_body())
                } else {
                    ResponseTemplate::new(400).set_body_json(serde_json::json!({
                        "type": "error",
                        "error": {"type": "invalid_request_error", "message": "max_tokens: 16384 > 8192"}
                    }))
                }
            }
        }

        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(Responder {
                calls: calls.clone(),
            })
            .mount(&server)
            .await;

        let anthropic = anthropic_for(&server, None).await;
        let result = anthropic
            .chat_with_tools_impl(
                &[RuntimeMessage::User("read x".into())],
                &[read_tool()],
                "claude-5",
            )
            .await;

        match result {
            Err(LlmError::TruncatedOutput { reason }) => assert_eq!(reason, "max_tokens"),
            Err(other) => panic!("expected TruncatedOutput, got {other:?}"),
            Ok(_) => panic!("expected TruncatedOutput, got an Ok response"),
        }

        let requests = server.received_requests().await.unwrap();
        let posts = post_requests(&requests);
        assert_eq!(posts.len(), 2, "one raise, then the rejection");
        assert_eq!(max_tokens_of(posts[0]), 8192);
        assert_eq!(max_tokens_of(posts[1]), 16 * 1024);
    }
}
