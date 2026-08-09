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
    system: Option<Vec<TextBlock>>,
    messages: Vec<ReqMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ReqTool>>,
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
    usage: Option<AnthUsage>,
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

        let request = MessagesRequest {
            model,
            max_tokens: self.backend.max_output_tokens(),
            system,
            messages: req_messages,
            tools: if req_tools.is_empty() {
                None
            } else {
                Some(req_tools)
            },
        };

        let url = format!("{}/messages", self.api_base().trim_end_matches('/'));
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
            let body = http_resp.text().await.unwrap_or_default();
            return Err(LlmError::from_http_status(status.as_u16(), body));
        }

        let response: MessagesResponse =
            http_resp
                .json()
                .await
                .map_err(|e| LlmError::InvalidRequest {
                    message: format!("decode messages response: {e}"),
                })?;

        let MessagesResponse {
            id,
            model: response_model,
            content: blocks,
            usage,
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

        Ok(LLMResponse::Text {
            content: content.unwrap_or_default(),
            metadata,
        })
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
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::from_http_status(
                status.as_u16(),
                format!("GET {url} returned {status}: {body}"),
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
}
