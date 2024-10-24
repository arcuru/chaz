use openai_api_rs::v1::{
    api::OpenAIClient,
    chat_completion::{self, ChatCompletionMessage, ChatCompletionRequest, MessageRole},
};

/// OpenAI Compatible Backend
///
/// Communicates over the OpenAI API as a backend for chaz.
use crate::{backends::LLMBackend, Backend, ChatContext};

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
}

impl LLMBackend for OpenAI {
    /// List the models available to this backend
    ///
    /// We can't query this, so it's just read from the config.
    fn list_models(&self) -> Vec<String> {
        // TODO: Embed a list of known models by backend ala https://github.com/sigoden/aichat/blob/main/models.yaml
        let mut models = Vec::new();
        for model in &self.backend.models.clone().unwrap_or_default() {
            models.push(model.name.clone());
        }
        models
    }

    /// Get the default model for this backend
    ///
    /// It's the first model in the list
    fn default_model(&self) -> Option<String> {
        if let Some(models) = &self.backend.models {
            if !models.is_empty() {
                return Some(models[0].name.clone());
            }
        }
        None
    }

    /// Execute a chat request with this backend
    async fn execute(&self, context: &ChatContext) -> Result<String, String> {
        let api_key = match self.backend.api_key.clone() {
            Some(key) => key,
            None => return Err("API key doesn't exist".to_string()),
        };
        let api_base = match self.backend.api_base.clone() {
            Some(base) => base,
            None => return Err("API base doesn't exist".to_string()),
        };

        let client = OpenAIClient::new_with_endpoint(api_base, api_key);
        let model_prefix = self.backend.name.clone().unwrap_or("openai".to_string());
        let request =
            convert_to_chatcompletionrequest(context, &model_prefix, &self.default_model());
        eprintln!("ASDF: {:?}", request);

        let response = client.chat_completion(request).await;

        let response = response.map_err(|e| e.to_string())?;

        Ok(response.choices[0]
            .message
            .content
            .clone()
            .unwrap_or("Error retrieving response".to_string()))
    }
}

fn convert_to_chatcompletionrequest(
    context: &ChatContext,
    model_prefix: &String,
    default_model: &Option<String>,
) -> ChatCompletionRequest {
    let mut messages = Vec::new();
    // Add the role
    if let Some(role) = &context.role {
        messages.push(ChatCompletionMessage {
            role: MessageRole::system,
            content: chat_completion::Content::Text(role.get_prompt()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    // Add all the messages
    for message in &context.messages {
        messages.push(ChatCompletionMessage {
            role: message.role.clone(),
            content: chat_completion::Content::Text(message.content.clone()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    // Get the appropriately scoped model name
    let mut model = context.model.clone().unwrap_or_default();
    model = model
        .trim_start_matches(&format!("{}:", model_prefix))
        .to_string();
    if model.is_empty() {
        model = default_model.clone().unwrap_or_default();
    }

    ChatCompletionRequest::new(model, messages)
}
