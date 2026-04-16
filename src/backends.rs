/// Manage all the backends for chaz.
///
/// This module is responsible for handling dispatch, validation, and general management for all the different backends
use openai_api_rs::v1::chat_completion::MessageRole;
use tracing::debug;

use crate::{
    config::Backend,
    error::LlmError,
    openai::OpenAI,
    role::RoleDetails,
    runtime::{LLMResponse, RuntimeMessage},
    security::SecretStore,
    tool::ToolDefinition,
};

pub trait LLMBackend {
    fn list_models(&self) -> Vec<String>;
    fn default_model(&self) -> Option<String>;
    /// Execute a simple chat request (no tools). Used by /compact and Matrix commands.
    async fn execute(&self, context: &ChatContext) -> Result<String, LlmError>;

    /// Whether this backend supports tool/function calling
    fn supports_tools(&self) -> bool {
        false
    }

    /// Execute a single LLM call with tool definitions (ReAct loop step).
    /// Returns structured response with text or tool calls.
    async fn chat_with_tools(
        &self,
        _messages: &[RuntimeMessage],
        _tools: &[ToolDefinition],
        _model: &str,
    ) -> Result<LLMResponse, LlmError> {
        Err(LlmError::Configuration {
            message: "Tool calling not supported by this backend".to_string(),
        })
    }
}

#[derive(Clone)]
pub struct BackendManager {
    backends: Vec<Backend>,
    secrets: SecretStore,
}

/// A generic Message
#[derive(Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let role = match self.role {
            MessageRole::user => "USER",
            MessageRole::assistant => "ASSISTANT",
            MessageRole::system => "SYSTEM",
            _ => "UNKNOWN",
        };
        write!(f, "{}: {}", role, self.content)
    }
}

impl Message {
    /// Create a new message
    pub fn new<S: Into<String>>(role: MessageRole, content: S) -> Message {
        Message {
            role,
            content: content.into(),
        }
    }
}

/// The ChatContext is an internal representation of a ChatCompletion request.
///
/// The frontend converts to this format, and the backend converts this to the backend-specific APIs.
pub struct ChatContext {
    pub messages: Vec<Message>,
    pub model: Option<String>,
    pub role: Option<RoleDetails>,
}

impl ChatContext {
    /// Convert messages into a single string.
    pub fn string_prompt(&self) -> String {
        let mut prompt = String::new();
        for message in self.messages.iter() {
            prompt.push_str(&format!("{}\n", message))
        }
        prompt.push_str("ASSISTANT: ");
        prompt
    }
}

impl BackendManager {
    /// Create a new backend manager
    pub fn new(backends: &Option<Vec<Backend>>, secrets: SecretStore) -> Self {
        Self {
            backends: backends.as_ref().cloned().unwrap_or_default(),
            secrets,
        }
    }

    /// Lists all known backends
    pub fn list_known_backends(&self) -> Vec<String> {
        self.backends.iter().map(|b| b.get_name().clone()).collect()
    }

    /// Lists all known models
    ///
    /// Models may be valid even if they aren't listed
    pub fn list_known_models(&self) -> Vec<String> {
        if self.backends.len() == 1 {
            OpenAI::new(&self.backends[0], &self.secrets).list_models()
        } else {
            self.backends
                .iter()
                .flat_map(|backend| {
                    let prefix = backend.get_name();
                    OpenAI::new(backend, &self.secrets)
                        .list_models()
                        .into_iter()
                        .map(move |model| format!("{}:{}", prefix, model))
                })
                .collect()
        }
    }

    /// Returns true if the model is known
    pub fn is_known_model(&self, model: &str) -> bool {
        self.list_known_models().contains(&model.to_string())
    }

    /// Validate that the model name is valid
    pub fn validate_model(&self, model: &str) -> Result<(), String> {
        if self.is_known_model(model) || self.backends.len() <= 1 {
            return Ok(());
        }
        // Multiple backends: name must be prefixed by backend name
        for backend in &self.backends {
            if model.starts_with(&format!("{}:", backend.name.as_deref().unwrap_or(""))) {
                return Ok(());
            }
        }
        Err("Multiple backends exist, please specify the model name with the backend prepended, e.g. openrouter:model-name".to_string())
    }

    /// Get the default model
    pub fn default_model(&self) -> Option<String> {
        let backend = self.backends.first()?;
        let model = OpenAI::new(backend, &self.secrets).default_model()?;
        if self.backends.len() == 1 {
            Some(model)
        } else {
            Some(format!("{}:{}", backend.get_name(), model))
        }
    }

    /// Select the backend based on a model name.
    /// Multi-backend setups use "backend_name:model" prefixed names.
    fn select_backend_for_model(&self, model: Option<&str>) -> &Backend {
        if let Some(model) = model {
            self.backends
                .iter()
                .find(|backend| {
                    backend.name.as_deref() == Some(model.split(":").next().unwrap_or(""))
                })
                .unwrap_or(&self.backends[0])
        } else {
            &self.backends[0]
        }
    }

    /// Select the backend based on the model name in a ChatContext.
    /// Used by legacy code paths (Matrix commands, /compact).
    fn select_backend(&self, context: &ChatContext) -> &Backend {
        self.select_backend_for_model(context.model.as_deref())
    }

    /// Execute a ChatContext (simple, no tools).
    /// Used by Matrix commands and /compact — not by the runtime.
    pub async fn execute(&self, context: &ChatContext) -> Result<String, LlmError> {
        if self.backends.is_empty() {
            return Err(LlmError::Configuration {
                message: "No backends configured".to_string(),
            });
        }
        let backend = self.select_backend(context);
        OpenAI::new(backend, &self.secrets).execute(context).await
    }

    /// Whether the backend for the given model supports tool/function calling.
    pub fn supports_tools_for_model(&self, model: Option<&str>) -> bool {
        if self.backends.is_empty() {
            return false;
        }
        let backend = self.select_backend_for_model(model);
        OpenAI::new(backend, &self.secrets).supports_tools()
    }

    /// Resolve a model name: strip backend prefix, fall back to default.
    pub fn resolve_model_name(&self, model: Option<&str>) -> String {
        if self.backends.is_empty() {
            return String::new();
        }
        let backend = self.select_backend_for_model(model);
        let model_prefix = backend.name.clone().unwrap_or_else(|| "openai".to_string());
        let mut resolved = model.unwrap_or("").to_string();
        resolved = resolved
            .trim_start_matches(&format!("{model_prefix}:"))
            .to_string();
        if resolved.is_empty() {
            resolved = OpenAI::new(backend, &self.secrets)
                .default_model()
                .unwrap_or_default();
        }
        debug!(
            requested = ?model,
            resolved = %resolved,
            backend = %backend.get_name(),
            "Model resolved"
        );
        resolved
    }

    /// Execute a single LLM call with tool definitions (for ReAct loop).
    pub async fn chat_with_tools_for_model(
        &self,
        model: Option<&str>,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        resolved_model: &str,
    ) -> Result<LLMResponse, LlmError> {
        if self.backends.is_empty() {
            return Err(LlmError::Configuration {
                message: "No backends configured".to_string(),
            });
        }
        let backend = self.select_backend_for_model(model);
        OpenAI::new(backend, &self.secrets)
            .chat_with_tools(messages, tools, resolved_model)
            .await
    }
}
