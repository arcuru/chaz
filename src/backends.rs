use matrix_sdk::media::MediaFileHandle;
use openai_api_rs::v1::chat_completion::MessageRole;

use crate::{
    aichat::AiChat,
    openai::OpenAI,
    role::{prepend_role, RoleDetails},
    Backend, BackendType,
};

/// Manage all the backends for chaz.
///
/// This module is responsible for handling dispatch, validation, and general management for all the different backends

pub trait LLMBackend {
    fn list_models(&self) -> Vec<String>;
    fn default_model(&self) -> Option<String>;
    async fn execute(&self, context: &ChatContext) -> Result<String, String>;
}

pub struct BackendManager {
    backends: Vec<Backend>,
}

/// A generic Message
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
    // TODO: consider making this the OpenAI format for a ChatCompletion request.
    pub messages: Vec<Message>,
    pub model: Option<String>,
    pub media: Vec<MediaFileHandle>,
    pub role: Option<RoleDetails>,
}

impl ChatContext {
    /// Convert messages into a single string.
    pub fn string_prompt(&self) -> String {
        // TODO: consider making this markdown
        let mut prompt = String::new();
        for message in self.messages.iter() {
            prompt.push_str(&format!("{}\n", message))
        }
        // Indicate that the assistant needs to speak next
        prompt.push_str("ASSISTANT: ");
        prompt
    }

    /// Convert messages into a single string with the role prepended
    pub fn string_prompt_with_role(&self) -> String {
        let prompt = self.string_prompt();
        if let Some(role) = &self.role {
            prepend_role(prompt, role)
        } else {
            prompt
        }
    }
}

impl BackendManager {
    /// Create a new backend manager
    ///
    /// If no backends are provided, it will default to an AIChat backend for backwards compat.
    pub fn new(backends: &Option<Vec<Backend>>) -> Self {
        Self {
            backends: backends
                .as_ref()
                .map_or_else(|| vec![Backend::new(BackendType::AIChat)], |v| v.clone()),
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
        // TODO: Cache/memoize this
        if self.backends.len() == 1 {
            // Don't prepend the names if there is only 1 backend
            let backend = &self.backends[0];
            match backend.backend_type {
                BackendType::AIChat => AiChat::new(backend).list_models(),
                BackendType::OpenAICompatible => OpenAI::new(backend).list_models(),
            }
        } else {
            let mut models = Vec::new();
            for backend in &self.backends {
                match backend.backend_type {
                    BackendType::AIChat => {
                        let mut backend_models = AiChat::new(backend).list_models();
                        backend_models = backend_models
                            .into_iter()
                            .map(|model| {
                                format!("{}:{}", backend.name.as_deref().unwrap_or("aichat"), model)
                            })
                            .collect();
                        models.append(&mut backend_models);
                    }
                    BackendType::OpenAICompatible => {
                        let mut backend_models = OpenAI::new(backend).list_models();
                        backend_models = backend_models
                            .into_iter()
                            .map(|model| {
                                format!("{}:{}", backend.name.as_deref().unwrap_or("openai"), model)
                            })
                            .collect();
                        models.append(&mut backend_models);
                    }
                }
            }
            models
        }
    }

    /// Returns true if the model is known
    ///
    /// This doesn't mean the model is invalid, just that there is no information on the model locally.
    pub fn is_known_model(&self, model: &str) -> bool {
        self.list_known_models().contains(&model.to_string())
    }

    /// Validate that the model name is valid
    ///
    /// Models can have invalid names, they must be prefixed by the name of the backend if more than 1 backend exists.
    pub fn validate_model(&self, model: &str) -> Result<(), String> {
        if self.is_known_model(model) {
            Ok(())
        } else {
            // Might still be ok, let's validate the name
            if self.backends.len() == 1 {
                // No need to prepend the name / no real way to validate
                Ok(())
            } else {
                // The name must be prefixed by the backend name
                for backend in &self.backends {
                    if model.starts_with(&format!("{}:", backend.name.as_deref().unwrap_or(""))) {
                        return Ok(());
                    }
                }
                Err("Multiple backends exist, please specify the model name with the backend prepended, e.g. openai:gpt-4o or aichat:ollama:llama3".to_string())
            }
        }
    }

    /// Get the default model
    pub fn default_model(&self) -> Option<String> {
        if self.backends.is_empty() {
            None
        } else {
            let backend = &self.backends[0];
            if self.backends.len() == 1 {
                match backend.backend_type {
                    BackendType::AIChat => AiChat::new(backend).default_model(),
                    BackendType::OpenAICompatible => OpenAI::new(backend).default_model(),
                }
            } else {
                match backend.backend_type {
                    BackendType::AIChat => AiChat::new(backend).default_model().map(|s| {
                        format!(
                            "{}:{}",
                            backend.name.clone().unwrap_or("aichat".to_string()),
                            s
                        )
                    }),
                    BackendType::OpenAICompatible => {
                        OpenAI::new(backend).default_model().map(|s| {
                            format!(
                                "{}:{}",
                                backend.name.clone().unwrap_or("openai".to_string()),
                                s
                            )
                        })
                    }
                }
            }
        }
    }

    /// Execute the ChatContext
    ///
    /// If no model is provided in the ChatContext, it will hand it off to the default model.
    pub async fn execute(&self, context: &ChatContext) -> Result<String, String> {
        if self.backends.is_empty() {
            return Err("No backends configured".to_string());
        }

        // Pick the backend to use based on the model name given in the ChatContext
        let backend = if let Some(model) = &context.model {
            self.backends
                .iter()
                .find(|backend| {
                    backend.name.as_deref() == Some(model.split(":").next().unwrap_or(""))
                })
                .unwrap_or(&self.backends[0])
        } else {
            &self.backends[0]
        };
        match backend.backend_type {
            BackendType::AIChat => AiChat::new(backend).execute(context).await,
            BackendType::OpenAICompatible => OpenAI::new(backend).execute(context).await,
        }
    }
}
