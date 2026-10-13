//! Stateless OpenAI Responses transport. The complete ordered output is kept
//! in the durable turn transcript, including opaque encrypted reasoning items.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    backends::{ChatContext, LLMBackend, MessageRole, ModelInfo},
    config::{Backend, ModelApi, ReasoningConfig, ReasoningEffort},
    error::LlmError,
    openai::OpenAI,
    runtime::{LLMResponse, ResponseMetadata, RuntimeMessage, TokenUsage, ToolCallRequest},
    security::SecretStore,
    tool::ToolDefinition,
};

pub(crate) const OUTPUT_ITEMS: &str = "openai_responses_output";
pub(crate) const REPLAY_ORIGIN: &str = "chaz_responses_origin";

/// Host-owned replay boundary, not the provider's returned alias or a session cache key.
/// The connection digest contains no reusable credential or private host identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplayOrigin {
    version: u8,
    api: String,
    requested_model: String,
    connection: String,
}

impl ReplayOrigin {
    pub(crate) fn new(
        backend: &Backend,
        secrets: &SecretStore,
        model: &str,
        api: ModelApi,
    ) -> Self {
        let credential = backend
            .api_key_ref
            .as_ref()
            .and_then(|r| secrets.get(r))
            .or_else(|| backend.api_key.clone());
        Self::with_credential(backend, secrets, model, api, credential.as_deref())
    }

    fn with_credential(
        backend: &Backend,
        secrets: &SecretStore,
        model: &str,
        api: ModelApi,
        credential: Option<&str>,
    ) -> Self {
        Self {
            version: 1,
            api: api.as_str().into(),
            requested_model: model.into(),
            connection: secrets.replay_connection_scope(backend, credential),
        }
    }

    pub(crate) fn matches(&self, extra: &Map<String, Value>) -> bool {
        extra
            .get(REPLAY_ORIGIN)
            .and_then(|v| serde_json::from_value::<Self>(v.clone()).ok())
            .is_some_and(|origin| origin == *self)
    }
}
/// Host-rendered routing context, kept separate from immutable native output.
pub(crate) const ENTRY_PROVENANCE: &str = "chaz_entry_provenance";

pub struct Responses {
    backend: Backend,
    secrets: SecretStore,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    instructions: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    store: bool,
    stream: bool,
    include: [&'static str; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
}

#[derive(Deserialize)]
struct Response {
    status: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    incomplete_details: Option<Value>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl Responses {
    pub fn new(backend: &Backend, secrets: &SecretStore) -> Self {
        Self {
            backend: backend.clone(),
            secrets: secrets.clone(),
        }
    }

    fn catalog(&self) -> OpenAI {
        OpenAI::new(&self.backend, &self.secrets)
    }
    pub(crate) fn list_models(&self) -> Vec<String> {
        self.catalog().list_models()
    }
    pub(crate) fn list_models_with_info(&self) -> Vec<ModelInfo> {
        self.catalog().list_models_with_info()
    }
    pub(crate) async fn fetch_models_from_api(&self) -> Result<Vec<ModelInfo>, LlmError> {
        self.catalog().fetch_models_from_api().await
    }
    pub(crate) fn default_model(&self) -> Option<String> {
        self.catalog().default_model()
    }
    pub(crate) fn supports_tools(&self) -> bool {
        true
    }

    #[cfg(test)]
    fn request<'a>(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &'a str,
        prompt_cache_key: Option<&'a str>,
    ) -> Result<Request<'a>, LlmError> {
        self.request_with_origin(
            messages,
            tools,
            model,
            prompt_cache_key,
            &ReplayOrigin::new(&self.backend, &self.secrets, model, ModelApi::Responses),
        )
    }

    fn request_with_origin<'a>(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &'a str,
        prompt_cache_key: Option<&'a str>,
        origin: &ReplayOrigin,
    ) -> Result<Request<'a>, LlmError> {
        let reasoning = self
            .backend
            .models
            .as_ref()
            .and_then(|models| models.iter().find(|m| m.name == model))
            .and_then(|m| m.reasoning.clone());
        // `max` is an OpenRouter extension, not a Responses effort value. Never
        // silently lower it or send an unsupported control to another API.
        if reasoning
            .as_ref()
            .is_some_and(|r| r.effort == ReasoningEffort::Max)
        {
            return Err(LlmError::Configuration {
                message: "Responses reasoning effort 'max' is unsupported; configure a supported effort such as 'xhigh'".into(),
            });
        }
        let mut instructions = Vec::new();
        let mut input = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            match message {
                RuntimeMessage::System(text) if input.is_empty() => {
                    instructions.push(text.as_str())
                }
                RuntimeMessage::System(text) => input.push(json!({
                    "type": "message", "role": "system",
                    "content": [{"type": "input_text", "text": text}]
                })),
                RuntimeMessage::User(text) => input.push(json!({
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                })),
                RuntimeMessage::Assistant(text) => input.push(assistant_message(text)),
                RuntimeMessage::AssistantWithExtra { provider_extra, .. }
                | RuntimeMessage::AssistantToolCalls { provider_extra, .. } => {
                    if let Some(items) = provider_extra.get(OUTPUT_ITEMS) {
                        let items = items.as_array().ok_or_else(|| LlmError::InvalidRequest {
                            message: "Invalid persisted Responses output items".into(),
                        })?;
                        if origin.matches(provider_extra) {
                            input.extend(items.iter().cloned());
                        } else {
                            validate_portable_output(items, message, messages, index)?;
                            append_visible_message(&mut input, message);
                        }
                        if let Some(envelope) = provider_extra.get(ENTRY_PROVENANCE) {
                            // Native output must stay unchanged, but it cannot
                            // replace the host's local/bridge delivery context.
                            let envelope =
                                envelope.as_str().ok_or_else(|| LlmError::InvalidRequest {
                                    message: "Invalid Responses entry provenance".into(),
                                })?;
                            input.push(json!({"type":"message", "role":"user",
                                "content":[{"type":"input_text", "text":envelope}]}));
                        }
                    } else {
                        append_visible_message(&mut input, message);
                    }
                }
                RuntimeMessage::ToolResult { call_id, content } => input.push(json!({
                    "type": "function_call_output", "call_id": call_id, "output": content
                })),
            }
        }
        Ok(Request {
            model,
            instructions: instructions.join("\n\n"),
            input,
            tools: tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function", "name": tool.name, "description": tool.description,
                        "parameters": tool.parameters, "strict": tool.strict
                    })
                })
                .collect(),
            reasoning,
            max_output_tokens: self.backend.max_tokens,
            store: false,
            stream: false,
            include: ["reasoning.encrypted_content"],
            prompt_cache_key,
        })
    }

    pub async fn chat_with_tools(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &str,
        prompt_cache_key: Option<&str>,
    ) -> Result<LLMResponse, LlmError> {
        let api_key = self
            .backend
            .api_key_ref
            .as_ref()
            .and_then(|r| self.secrets.get(r))
            .or_else(|| self.backend.api_key.clone())
            .ok_or_else(|| LlmError::Configuration {
                message: "API key not configured".into(),
            })?;
        let origin = ReplayOrigin::with_credential(
            &self.backend,
            &self.secrets,
            model,
            ModelApi::Responses,
            Some(&api_key),
        );
        let request =
            self.request_with_origin(messages, tools, model, prompt_cache_key, &origin)?;
        let api_base = self
            .backend
            .api_base
            .as_ref()
            .ok_or_else(|| LlmError::Configuration {
                message: "API base URL not configured".into(),
            })?;
        let client = reqwest::Client::builder()
            .timeout(self.backend.request_timeout())
            .build()
            .map_err(network_error)?;
        let response = client
            .post(format!("{}/responses", api_base.trim_end_matches('/')))
            .bearer_auth(api_key)
            .json(&request)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|s| *s > 0.0 && *s <= 60.0)
            .map(std::time::Duration::from_secs_f64);
        let body = response.bytes().await.map_err(network_error)?;
        if !status.is_success() {
            let message = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error").map(error_message))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
            return Err(LlmError::from_http_status_with_retry_after(
                status.as_u16(),
                message,
                retry_after,
            ));
        }
        let response =
            serde_json::from_slice::<Response>(&body).map_err(|e| LlmError::InvalidRequest {
                message: format!("decode Responses response: {e}"),
            })?;
        let mut result = parse_response(response, model)?;
        let origin = json!(origin);
        let (provider_extra, metadata) = match &mut result {
            LLMResponse::Text {
                provider_extra,
                metadata,
                ..
            }
            | LLMResponse::ToolCalls {
                provider_extra,
                metadata,
                ..
            } => (provider_extra, metadata),
        };
        // Never accept provenance supplied by the remote response's flattened extras.
        provider_extra.insert(REPLAY_ORIGIN.into(), origin.clone());
        if let Some(metadata) = metadata {
            metadata.extra.insert(REPLAY_ORIGIN.into(), origin);
        }
        Ok(result)
    }

    pub async fn execute(&self, context: &ChatContext) -> Result<String, LlmError> {
        let prefix = format!("{}:", self.backend.get_name());
        let model = context
            .model
            .as_deref()
            .map(|m| m.strip_prefix(&prefix).unwrap_or(m))
            .filter(|m| !m.is_empty())
            .or_else(|| {
                self.backend
                    .models
                    .as_ref()?
                    .first()
                    .map(|m| m.name.as_str())
            })
            .unwrap_or_default();
        let mut messages = Vec::new();
        if let Some(system) = &context.system_prompt {
            messages.push(RuntimeMessage::System(system.clone()));
        }
        messages.extend(context.messages.iter().map(|m| match m.role {
            MessageRole::System => RuntimeMessage::System(m.content.clone()),
            MessageRole::User => RuntimeMessage::User(m.content.clone()),
            MessageRole::Assistant => RuntimeMessage::Assistant(m.content.clone()),
        }));
        match self.chat_with_tools(&messages, &[], model, None).await? {
            LLMResponse::Text { content, .. } => Ok(content),
            LLMResponse::ToolCalls { .. } => Err(LlmError::InvalidRequest {
                message: "Responses returned function calls with no tools requested".into(),
            }),
        }
    }
}

fn append_visible_message(input: &mut Vec<Value>, message: &RuntimeMessage) {
    let (RuntimeMessage::AssistantWithExtra { content, .. }
    | RuntimeMessage::AssistantToolCalls { content, .. }) = message
    else {
        return;
    };
    if let Some(text) = content.as_ref().filter(|s| !s.is_empty()) {
        input.push(assistant_message(text));
    }
    if let RuntimeMessage::AssistantToolCalls { tool_calls, .. } = message {
        for call in tool_calls {
            input.push(json!({"type": "function_call", "call_id": call.id,
                "name": call.name, "arguments": call.arguments}));
        }
    }
}

fn conversion_error(reason: &str) -> LlmError {
    LlmError::InvalidRequest {
        message: format!(
            "Cannot convert Responses history: {reason}. Restore the original model/connection or start a new session; the transcript is unchanged"
        ),
    }
}

pub(crate) fn validate_foreign_history(messages: &[RuntimeMessage]) -> Result<(), LlmError> {
    for (index, message) in messages.iter().enumerate() {
        let (RuntimeMessage::AssistantWithExtra { provider_extra, .. }
        | RuntimeMessage::AssistantToolCalls { provider_extra, .. }) = message
        else {
            continue;
        };
        if let Some(items) = provider_extra.get(OUTPUT_ITEMS) {
            let items = items
                .as_array()
                .ok_or_else(|| conversion_error("invalid persisted output items"))?;
            validate_portable_output(items, message, messages, index)?;
        }
    }
    Ok(())
}

/// Conversion is a request view only. Unknown provider actions, non-text output,
/// incomplete or ambiguous exchanges have no demonstrated portable equivalent.
fn validate_portable_output(
    items: &[Value],
    message: &RuntimeMessage,
    messages: &[RuntimeMessage],
    index: usize,
) -> Result<(), LlmError> {
    let mut native_calls = Vec::new();
    for item in items {
        if item
            .get("status")
            .is_some_and(|s| s.as_str() != Some("completed"))
        {
            return Err(conversion_error("output is not completed"));
        }
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {} // Opaque reasoning has no portable meaning.
            Some("message") => {
                if item["role"] != "assistant" || !native_calls.is_empty() {
                    return Err(conversion_error(
                        "unsupported message role or interleaved output",
                    ));
                }
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| conversion_error("invalid visible output"))?;
                for part in content {
                    let field = match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => "text",
                        Some("refusal") => "refusal",
                        _ => return Err(conversion_error("non-text output")),
                    };
                    if part.get(field).and_then(Value::as_str).is_none() {
                        return Err(conversion_error("invalid visible text"));
                    }
                }
            }
            Some("function_call") => {
                let string = |field| {
                    item.get(field)
                        .and_then(Value::as_str)
                        .ok_or_else(|| conversion_error("invalid function call"))
                };
                let arguments = string("arguments")?;
                if !serde_json::from_str::<Value>(arguments).is_ok_and(|v| v.is_object()) {
                    return Err(conversion_error("incomplete function arguments"));
                }
                native_calls.push(ToolCallRequest {
                    id: string("call_id")?.into(),
                    name: string("name")?.into(),
                    arguments: arguments.into(),
                });
            }
            _ => return Err(conversion_error("unknown provider output item")),
        }
    }
    let (content, calls) = match message {
        RuntimeMessage::AssistantToolCalls {
            content,
            tool_calls,
            ..
        } => (content, tool_calls.as_slice()),
        RuntimeMessage::AssistantWithExtra { content, .. } => (content, &[][..]),
        _ => unreachable!("native output is assistant data"),
    };
    if native_calls != calls {
        return Err(conversion_error(
            "retained calls do not match native output",
        ));
    }
    if calls.is_empty() && content.as_ref().is_none_or(|s| s.is_empty()) {
        return Err(conversion_error("no retained visible text"));
    }
    for (offset, call) in calls.iter().enumerate() {
        if call.id.is_empty()
            || call.name.is_empty()
            || !matches!(messages.get(index + 1 + offset),
            Some(RuntimeMessage::ToolResult { call_id, .. }) if call_id == &call.id)
        {
            return Err(conversion_error(
                "function call lacks its complete recorded result",
            ));
        }
        let made = messages
            .iter()
            .filter_map(|m| match m {
                RuntimeMessage::AssistantToolCalls { tool_calls, .. } => Some(tool_calls),
                _ => None,
            })
            .flatten()
            .filter(|c| c.id == call.id)
            .count();
        let results = messages
            .iter()
            .filter(|m| {
                matches!(m,
            RuntimeMessage::ToolResult { call_id, .. } if call_id == &call.id)
            })
            .count();
        if made != 1 || results != 1 {
            return Err(conversion_error("ambiguous function call/result identity"));
        }
    }
    Ok(())
}

fn assistant_message(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "content": text})
}

fn network_error(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::NetworkError {
            message: error.to_string(),
        }
    }
}

fn error_message(error: &Value) -> String {
    match error.get("message").and_then(Value::as_str) {
        Some(message) => match error.get("code").and_then(Value::as_str) {
            Some(code) => format!("{code}: {message}"),
            None => message.to_string(),
        },
        None => error
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string()),
    }
}

fn parse_response(response: Response, requested_model: &str) -> Result<LLMResponse, LlmError> {
    if let Some(error) = response.error {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = match code {
            "rate_limit_exceeded" => 429,
            "server_error" => 500,
            "invalid_api_key" => 401,
            "insufficient_quota" | "insufficient_credits" => 402,
            _ => 400,
        };
        return Err(LlmError::from_http_status(status, error_message(&error)));
    }
    if response.status == "incomplete" {
        return Err(LlmError::TruncatedOutput {
            reason: response
                .incomplete_details
                .as_ref()
                .and_then(|v| v.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("incomplete Responses output")
                .into(),
        });
    }
    if response.status != "completed" {
        return Err(LlmError::InvalidRequest {
            message: format!("Responses did not complete (status: {})", response.status),
        });
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in &response.output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| LlmError::InvalidRequest {
                        message: "Responses message has no content array".into(),
                    })?;
                for part in content {
                    let field = match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => "text",
                        Some("refusal") => "refusal",
                        _ => continue,
                    };
                    text.push_str(required_string(part, field)?);
                }
            }
            Some("function_call") => {
                let arguments = required_string(item, "arguments")?;
                if !serde_json::from_str::<Value>(arguments).is_ok_and(|v| v.is_object()) {
                    return Err(LlmError::InvalidRequest {
                        message: "Responses function arguments are not a complete JSON object"
                            .into(),
                    });
                }
                calls.push(ToolCallRequest {
                    id: required_string(item, "call_id")?.into(),
                    name: required_string(item, "name")?.into(),
                    arguments: arguments.into(),
                });
            }
            _ => {} // Opaque output (including reasoning) must still replay in order.
        }
    }
    if calls.is_empty() && text.is_empty() {
        return Err(LlmError::EmptyResponse {
            message: "Responses returned no text, refusal or function calls".into(),
        });
    }
    let mut provider_extra = Map::new();
    provider_extra.insert(OUTPUT_ITEMS.into(), Value::Array(response.output));
    let mut extra = response.extra;
    crate::session::usage::mark_usage_fields(
        &mut extra,
        usage_count(response.usage.as_ref(), "/input_tokens").is_some(),
        usage_count(response.usage.as_ref(), "/output_tokens").is_some(),
    );
    // Final entries already persist ResponseMetadata. Retain terminal output
    // there as well so context can recover it at the final entry's position.
    if calls.is_empty() {
        extra.insert(OUTPUT_ITEMS.into(), provider_extra[OUTPUT_ITEMS].clone());
    }
    let metadata = Some(ResponseMetadata {
        model: response.model.unwrap_or_else(|| requested_model.into()),
        response_id: response.id,
        usage: parse_usage(response.usage.as_ref()),
        extra,
        ..Default::default()
    });
    if calls.is_empty() {
        Ok(LLMResponse::Text {
            content: text,
            provider_extra,
            metadata,
        })
    } else {
        Ok(LLMResponse::ToolCalls {
            content: (!text.is_empty()).then_some(text),
            tool_calls: calls,
            provider_extra,
            metadata,
        })
    }
}

fn usage_count(usage: Option<&Value>, field: &str) -> Option<u32> {
    usage
        .and_then(|u| u.pointer(field))
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

fn parse_usage(usage: Option<&Value>) -> TokenUsage {
    let count = |field: &str| usage_count(usage, field);
    TokenUsage {
        prompt_tokens: count("/input_tokens").unwrap_or_default(),
        completion_tokens: count("/output_tokens").unwrap_or_default(),
        total_tokens: count("/total_tokens").unwrap_or_default(),
        cached_tokens: count("/input_tokens_details/cached_tokens"),
        reasoning_tokens: count("/output_tokens_details/reasoning_tokens"),
        cost_usd: usage.and_then(|u| u.get("cost")).and_then(Value::as_f64),
        ..Default::default()
    }
}

fn required_string<'a>(item: &'a Value, field: &str) -> Result<&'a str, LlmError> {
    item.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::InvalidRequest {
            message: format!("Responses output missing string '{field}'"),
        })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod runtime_tests;

#[cfg(test)]
mod origin_tests;
