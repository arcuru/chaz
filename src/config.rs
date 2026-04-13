use crate::role::RoleDetails;
use serde::Deserialize;

/// Configuration for the chaz bot
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// Matrix homeserver URL (required for Matrix gateway)
    #[serde(default)]
    pub homeserver_url: String,
    /// Matrix username (required for Matrix gateway)
    #[serde(default)]
    pub username: String,
    /// Optionally specify the password, if not set it will be asked for on cmd line
    pub password: Option<String>,
    /// Allow list of which accounts we will respond to
    pub allow_list: Option<String>,
    /// Per-account message limit while the bot is running
    pub message_limit: Option<u64>,
    /// Room size limit to respond to
    pub room_size_limit: Option<usize>,
    /// Set the state directory for chaz
    pub state_dir: Option<String>,
    /// Model to use for summarizing chats
    pub chat_summary_model: Option<String>,
    /// Default role
    pub role: Option<String>,
    /// Definitions of roles
    pub roles: Option<Vec<RoleDetails>>,
    /// Backend configuration
    pub backends: Option<Vec<Backend>>,
}

/// Configuration info for a backend
#[derive(Debug, Deserialize, Clone)]
pub struct Backend {
    /// The type of backend (kept for config compat, only "openaicompatible" supported)
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub backend_type: BackendType,
    /// The base URL for the API
    pub api_base: Option<String>,
    /// The API key to use for the API
    pub api_key: Option<String>,
    /// Available models for this backend
    pub models: Option<Vec<Model>>,
    /// Name of this backend
    pub name: Option<String>,
    /// Set the config directory
    #[allow(dead_code)]
    pub config_dir: Option<String>,
}

impl Backend {
    pub fn new(backend_type: BackendType) -> Self {
        Backend {
            backend_type,
            api_base: None,
            api_key: None,
            models: None,
            name: None,
            config_dir: None,
        }
    }

    /// Get the name for this backend
    pub fn get_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| "openai".to_string())
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Model {
    /// The name of the model
    pub name: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum BackendType {
    OpenAICompatible,
}
