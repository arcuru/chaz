/// OpenAI Compatible Backend
///
/// Communicates over the OpenAI API as a backend for chaz.
use openai_api_rs::v1::{
    api::OpenAIClient,
    chat_completion::{
        self, ChatCompletionMessage, ChatCompletionRequest, MessageRole, Tool as OpenAITool,
        ToolType,
    },
    types,
};

use crate::{
    backends::{ChatContext, LLMBackend},
    config::Backend,
    runtime::{LLMResponse, RuntimeMessage, ToolCallRequest},
    tool::ToolDefinition,
};
use std::collections::HashMap;

/// Handle connections to an OpenAI compatible backend
pub struct OpenAI {
    /// Stores the full info given in the config file
    backend: Backend,
}

impl OpenAI {
    pub fn new(backend: &Backend) -> Self {
        OpenAI {
            backend: backend.clone(),
        }
    }

    fn build_client(&self) -> Result<OpenAIClient, String> {
        let api_key = self
            .backend
            .api_key
            .clone()
            .ok_or("API key doesn't exist")?;
        let api_base = self
            .backend
            .api_base
            .clone()
            .ok_or("API base doesn't exist")?;
        OpenAIClient::builder()
            .with_endpoint(api_base)
            .with_api_key(api_key)
            .build()
            .map_err(|e| e.to_string())
    }

    /// Resolve the model name for this backend, stripping the backend prefix
    pub fn resolve_model(&self, context: &ChatContext) -> String {
        let model_prefix = self.backend.name.clone().unwrap_or("openai".to_string());
        let mut model = context.model.clone().unwrap_or_default();
        model = model
            .trim_start_matches(&format!("{}:", model_prefix))
            .to_string();
        if model.is_empty() {
            model = self.default_model().unwrap_or_default();
        }
        model
    }

    /// Execute a single LLM call with tool definitions, returning a structured response.
    ///
    /// This is called by the runtime's ReAct loop. It converts RuntimeMessages
    /// to OpenAI format, includes tool definitions, and parses the response.
    pub async fn chat_with_tools(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<LLMResponse, String> {
        let client = self.build_client()?;

        let openai_messages = convert_runtime_messages(messages);
        let openai_tools = convert_tool_definitions(tools);

        let mut request = ChatCompletionRequest::new(model.to_string(), openai_messages);
        if !openai_tools.is_empty() {
            request.tools = Some(openai_tools);
        }

        let response = client
            .chat_completion(request)
            .await
            .map_err(|e| e.to_string())?;

        let choice = response.choices.first().ok_or("No choices in response")?;

        tracing::info!(
            "chat_with_tools response: content={:?} tool_calls={:?} finish_reason={:?}",
            choice.message.content,
            choice.message.tool_calls,
            choice.finish_reason
        );

        // Check if the LLM wants to call tools
        if let Some(tool_calls) = &choice.message.tool_calls {
            if !tool_calls.is_empty() {
                let calls = tool_calls
                    .iter()
                    .map(|tc| ToolCallRequest {
                        id: tc.id.clone(),
                        name: tc.function.name.clone().unwrap_or_default(),
                        arguments: tc.function.arguments.clone().unwrap_or_default(),
                    })
                    .collect();

                return Ok(LLMResponse::ToolCalls {
                    content: choice.message.content.clone(),
                    tool_calls: calls,
                });
            }
        }

        // Final text response
        Ok(LLMResponse::Text(
            choice.message.content.clone().unwrap_or_default(),
        ))
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
        if let Some(models) = &self.backend.models {
            if !models.is_empty() {
                return Some(models[0].name.clone());
            }
        }
        None
    }

    /// Execute a simple chat request (no tools)
    async fn execute(&self, context: &ChatContext) -> Result<String, String> {
        let client = self.build_client()?;
        let model_prefix = self.backend.name.clone().unwrap_or("openai".to_string());
        let request =
            convert_to_chatcompletionrequest(context, &model_prefix, &self.default_model());

        tracing::info!(
            "OpenAI execute: endpoint={:?} model={} messages={}",
            self.backend.api_base,
            request.model,
            request.messages.len()
        );

        let response = client.chat_completion(request).await;
        let response = response.map_err(|e| e.to_string())?;

        Ok(response.choices[0]
            .message
            .content
            .clone()
            .unwrap_or("Error retrieving response".to_string()))
    }
}

/// Convert RuntimeMessages to OpenAI ChatCompletionMessages
fn convert_runtime_messages(messages: &[RuntimeMessage]) -> Vec<ChatCompletionMessage> {
    messages
        .iter()
        .flat_map(|msg| match msg {
            RuntimeMessage::System(content) => vec![ChatCompletionMessage {
                role: MessageRole::system,
                content: chat_completion::Content::Text(content.clone()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            RuntimeMessage::User(content) => vec![ChatCompletionMessage {
                role: MessageRole::user,
                content: chat_completion::Content::Text(content.clone()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            RuntimeMessage::Assistant(content) => vec![ChatCompletionMessage {
                role: MessageRole::assistant,
                content: chat_completion::Content::Text(content.clone()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            RuntimeMessage::AssistantToolCalls {
                content,
                tool_calls,
            } => vec![ChatCompletionMessage {
                role: MessageRole::assistant,
                content: chat_completion::Content::Text(content.clone().unwrap_or_default()),
                name: None,
                tool_calls: Some(
                    tool_calls
                        .iter()
                        .map(|tc| chat_completion::ToolCall {
                            id: tc.id.clone(),
                            r#type: "function".to_string(),
                            function: chat_completion::ToolCallFunction {
                                name: Some(tc.name.clone()),
                                arguments: Some(tc.arguments.clone()),
                            },
                        })
                        .collect(),
                ),
                tool_call_id: None,
            }],
            RuntimeMessage::ToolResult { call_id, content } => vec![ChatCompletionMessage {
                role: MessageRole::tool,
                content: chat_completion::Content::Text(content.clone()),
                name: None,
                tool_calls: None,
                tool_call_id: Some(call_id.clone()),
            }],
        })
        .collect()
}

/// Convert ToolDefinitions to OpenAI Tool format
fn convert_tool_definitions(tools: &[ToolDefinition]) -> Vec<OpenAITool> {
    tools
        .iter()
        .map(|td| {
            // Convert serde_json::Value parameters to FunctionParameters
            let properties = td
                .parameters
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|props| {
                    props
                        .iter()
                        .map(|(k, v)| (k.clone(), Box::new(json_to_schema_define(v))))
                        .collect::<HashMap<_, _>>()
                });

            let required = td
                .parameters
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });

            OpenAITool {
                r#type: ToolType::Function,
                function: types::Function {
                    name: td.name.clone(),
                    description: Some(td.description.clone()),
                    parameters: types::FunctionParameters {
                        schema_type: types::JSONSchemaType::Object,
                        properties,
                        required,
                    },
                },
            }
        })
        .collect()
}

/// Convert a serde_json::Value to a JSONSchemaDefine
fn json_to_schema_define(value: &serde_json::Value) -> types::JSONSchemaDefine {
    types::JSONSchemaDefine {
        schema_type: value.get("type").and_then(|t| t.as_str()).map(|t| match t {
            "string" => types::JSONSchemaType::String,
            "number" => types::JSONSchemaType::Number,
            "integer" => types::JSONSchemaType::Number,
            "boolean" => types::JSONSchemaType::Boolean,
            "array" => types::JSONSchemaType::Array,
            "object" => types::JSONSchemaType::Object,
            _ => types::JSONSchemaType::String,
        }),
        description: value
            .get("description")
            .and_then(|d| d.as_str())
            .map(String::from),
        enum_values: value.get("enum").and_then(|e| e.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        }),
        properties: None,
        required: None,
        items: None,
    }
}

/// Convert ChatContext to a ChatCompletionRequest (simple, no tools)
fn convert_to_chatcompletionrequest(
    context: &ChatContext,
    model_prefix: &String,
    default_model: &Option<String>,
) -> ChatCompletionRequest {
    let mut messages = Vec::new();
    if let Some(role) = &context.role {
        messages.push(ChatCompletionMessage {
            role: MessageRole::system,
            content: chat_completion::Content::Text(role.get_prompt()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for message in &context.messages {
        messages.push(ChatCompletionMessage {
            role: message.role.clone(),
            content: chat_completion::Content::Text(message.content.clone()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    let mut model = context.model.clone().unwrap_or_default();
    model = model
        .trim_start_matches(&format!("{}:", model_prefix))
        .to_string();
    if model.is_empty() {
        model = default_model.clone().unwrap_or_default();
    }

    ChatCompletionRequest::new(model, messages)
}
