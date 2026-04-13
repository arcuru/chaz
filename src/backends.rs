/// Manage all the backends for chaz.
///
/// This module is responsible for handling dispatch, validation, and general management for all the different backends
use openai_api_rs::v1::chat_completion::MessageRole;

use crate::{config::Backend, openai::OpenAI, role::RoleDetails};

pub trait LLMBackend {
    fn list_models(&self) -> Vec<String>;
    fn default_model(&self) -> Option<String>;
    async fn execute(&self, context: &ChatContext) -> Result<String, String>;
}

pub struct BackendManager {
    backends: Vec<Backend>,
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
    pub fn new(backends: &Option<Vec<Backend>>) -> Self {
        Self {
            backends: backends.as_ref().cloned().unwrap_or_default(),
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
            OpenAI::new(&self.backends[0]).list_models()
        } else {
            self.backends
                .iter()
                .flat_map(|backend| {
                    let prefix = backend.get_name();
                    OpenAI::new(backend)
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
        let model = OpenAI::new(backend).default_model()?;
        if self.backends.len() == 1 {
            Some(model)
        } else {
            Some(format!("{}:{}", backend.get_name(), model))
        }
    }

    /// Select the backend based on the model name in the context
    fn select_backend(&self, context: &ChatContext) -> &Backend {
        if let Some(model) = &context.model {
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

    /// Execute the ChatContext
    pub async fn execute(&self, context: &ChatContext) -> Result<String, String> {
        if self.backends.is_empty() {
            return Err("No backends configured".to_string());
        }
        let backend = self.select_backend(context);
        OpenAI::new(backend).execute(context).await
    }

    /// Get an OpenAI backend for the selected context (for tool-aware execution)
    pub fn get_openai_backend(&self, context: &ChatContext) -> Option<OpenAI> {
        if self.backends.is_empty() {
            return None;
        }
        Some(OpenAI::new(self.select_backend(context)))
    }
}
