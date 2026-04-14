use crate::role::RoleDetails;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    /// Agent definitions
    pub agents: Option<Vec<AgentConfig>>,
    /// Security settings
    pub security: Option<SecurityConfig>,
}

/// Configuration for an agent
#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    /// Name of the agent
    pub name: String,
    /// Default role (system prompt) for this agent
    pub role: Option<String>,
    /// Default model for this agent
    pub model: Option<String>,
    /// List of tool names this agent is allowed to use (None = all tools)
    pub tools: Option<Vec<String>>,
    /// Which agent definitions this agent can spawn
    pub can_spawn: Option<Vec<String>>,
    /// Which agents are allowed to spawn this one (None/empty = any with can_spawn permission)
    pub allowed_callers: Option<Vec<String>>,
    /// Maximum ReAct loop iterations (default: 10)
    pub max_iterations: Option<u32>,
    /// Whether this agent can run without user input (scheduled/heartbeat)
    #[serde(default)]
    pub autonomous: bool,
    /// Named override bundles for spawn-time configuration
    pub presets: Option<HashMap<String, AgentPreset>>,
}

/// A named bundle of overrides that can be selected at spawn time.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentPreset {
    /// Override the model
    pub model: Option<String>,
    /// Override max iterations
    pub max_iterations: Option<u32>,
    /// Restrict tools (must be subset of agent definition's tools)
    pub tools: Option<Vec<String>>,
    /// Appended to the base system prompt
    pub role_suffix: Option<String>,
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
    /// The API key from config (extracted into SecretStore at startup, then cleared).
    /// Supports env var references: `"${VAR_NAME}"` or `"$VAR_NAME"`.
    pub api_key: Option<String>,
    /// Opaque reference ID into SecretStore (set after api_key is extracted)
    #[serde(skip)]
    pub api_key_ref: Option<String>,
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
            api_key_ref: None,
            models: None,
            name: None,
            config_dir: None,
        }
    }

    /// Generate a SecretStore reference key for this backend's API key.
    pub fn secret_key(&self) -> String {
        format!("backend:{}", self.get_name())
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

/// Security configuration
#[derive(Debug, Deserialize, Clone, Default)]
pub struct SecurityConfig {
    /// Allowed endpoints for web_fetch (empty = allow all, non-empty = deny-all default)
    pub allowed_endpoints: Option<Vec<EndpointConfig>>,
    /// Shell commands allowed (if set, only these command prefixes are permitted)
    pub shell_allowlist: Option<Vec<String>>,
    /// Shell commands denied (blocked even without an allowlist)
    pub shell_denylist: Option<Vec<String>>,
    /// Tools that are auto-approved (skip approval for UnlessAutoApproved tools)
    pub auto_approved_tools: Option<Vec<String>>,
    /// Leak detection policy: "redact" (default) or "block"
    pub leak_policy: Option<String>,
}

/// An allowed endpoint for network policy
#[derive(Debug, Deserialize, Clone)]
pub struct EndpointConfig {
    /// Host to match (exact or wildcard like "*.example.com")
    pub host: String,
    /// Optional path prefix restriction
    pub path_prefix: Option<String>,
    /// Allowed HTTP methods (None = all)
    pub methods: Option<Vec<String>>,
}
