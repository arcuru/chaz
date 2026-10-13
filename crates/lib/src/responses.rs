//! Stateless OpenAI Responses transport. The complete ordered output is kept
//! in the durable turn transcript, including opaque encrypted reasoning items.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    backends::{ChatContext, LLMBackend, MessageRole, ModelInfo},
    config::{Backend, ReasoningConfig, ReasoningEffort},
    error::LlmError,
    openai::OpenAI,
    runtime::{LLMResponse, ResponseMetadata, RuntimeMessage, TokenUsage, ToolCallRequest},
    security::SecretStore,
    tool::ToolDefinition,
};

pub(crate) const OUTPUT_ITEMS: &str = "openai_responses_output";

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

    fn request<'a>(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &'a str,
        prompt_cache_key: Option<&'a str>,
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
        for message in messages {
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
                RuntimeMessage::AssistantWithExtra {
                    content,
                    provider_extra,
                }
                | RuntimeMessage::AssistantToolCalls {
                    content,
                    provider_extra,
                    ..
                } => {
                    if let Some(items) = provider_extra.get(OUTPUT_ITEMS) {
                        let items = items.as_array().ok_or_else(|| LlmError::InvalidRequest {
                            message: "Invalid persisted Responses output items".into(),
                        })?;
                        input.extend(items.iter().cloned());
                    } else {
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
        let request = self.request(messages, tools, model, prompt_cache_key)?;
        let api_key = self
            .backend
            .api_key_ref
            .as_ref()
            .and_then(|r| self.secrets.get(r))
            .or_else(|| self.backend.api_key.clone())
            .ok_or_else(|| LlmError::Configuration {
                message: "API key not configured".into(),
            })?;
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
        parse_response(response, model)
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

fn parse_usage(usage: Option<&Value>) -> TokenUsage {
    let count = |field: &str| {
        usage
            .and_then(|u| u.pointer(field))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
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
