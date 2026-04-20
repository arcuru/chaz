use crate::role::RoleDetails;
use crate::tool::PresentationMode;
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
    /// Scheduled tasks
    pub schedules: Option<Vec<ScheduleConfig>>,
    /// MCP (Model Context Protocol) subprocess servers
    pub mcp_servers: Option<Vec<McpServerConfig>>,
    /// Named tool profiles controlling how tool definitions are presented to the LLM
    pub tool_profiles: Option<HashMap<String, ToolProfileConfig>>,
    /// Directory to scan for MCP server manifest files (.yaml/.json).
    /// Each file should contain a single McpServerConfig object.
    /// Merged with inline `mcp_servers` entries; name collisions are logged and skipped.
    pub mcp_server_dir: Option<String>,
    /// Context window management settings
    pub context: Option<ContextConfig>,
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
    /// Tool profile name (references a key in top-level tool_profiles)
    pub tool_profile: Option<String>,
    /// Override context token limit for this agent
    pub max_context_tokens: Option<usize>,
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
    /// Tool profile override (references a key in top-level tool_profiles)
    pub tool_profile: Option<String>,
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
    /// Timeout for LLM requests in seconds (default: 120)
    pub request_timeout: Option<u64>,
    /// Maximum retry attempts for transient LLM errors (default: 3)
    pub max_retries: Option<u32>,
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
            request_timeout: None,
            max_retries: None,
        }
    }

    /// LLM request timeout as Duration (default: 120s).
    pub fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.request_timeout.unwrap_or(120))
    }

    /// Maximum retry attempts for transient errors (default: 3).
    pub fn max_retries(&self) -> u32 {
        self.max_retries.unwrap_or(3)
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

/// Configuration for a scheduled task
#[derive(Debug, Deserialize, Clone)]
pub struct ScheduleConfig {
    /// Unique name for this schedule
    pub name: String,
    /// Target session identifier — can be a session name, eidetica DB root ID, or transport ID
    pub session: String,
    /// Task instructions sent as the directive content
    pub task: String,
    /// Cron expression (e.g., "0 9 * * *")
    pub cron: String,
    /// Whether this schedule is active (default: true)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Security configuration
#[derive(Debug, Deserialize, Clone, Default)]
pub struct SecurityConfig {
    /// Deprecated — use `tool_policies.web_fetch.grants.network.endpoints`.
    /// Still parsed for backward compatibility and migrated to grants at startup.
    pub allowed_endpoints: Option<Vec<EndpointConfig>>,
    /// Deprecated — use `tool_policies.shell.grants.shell.allow`.
    /// Still parsed for backward compatibility and migrated to grants at startup.
    pub shell_allowlist: Option<Vec<String>>,
    /// Deprecated — use `tool_policies.shell.grants.shell.deny`.
    /// Still parsed for backward compatibility and migrated to grants at startup.
    pub shell_denylist: Option<Vec<String>>,
    /// Tools that are auto-approved (skip approval for UnlessAutoApproved tools)
    pub auto_approved_tools: Option<Vec<String>>,
    /// Leak detection policy: "redact" (default) or "block"
    pub leak_policy: Option<String>,
    /// Per-tool policy overrides (risk, approval, timeout, sensitive_params, grants)
    pub tool_policies: Option<std::collections::HashMap<String, crate::tool::ToolPolicy>>,
}

/// Configuration for a tool profile — controls how tool definitions are presented to the LLM.
#[derive(Debug, Deserialize, Clone)]
pub struct ToolProfileConfig {
    /// Default presentation mode for tools not explicitly listed
    pub default: Option<PresentationMode>,
    /// Per-tool presentation mode overrides (supports "namespace.*" glob patterns)
    pub tools: Option<HashMap<String, PresentationMode>>,
}

/// Configuration for an MCP server (subprocess or HTTP).
///
/// Transport is determined by which fields are set:
/// - `command` (with optional `args`, `env`): stdio subprocess transport
/// - `url`: Streamable HTTP transport (POST + SSE)
///
/// At least one of `command` or `url` must be set.
#[derive(Debug, Deserialize, Clone)]
pub struct McpServerConfig {
    /// Name used as namespace prefix for tools (e.g., "filesystem" → "filesystem.read_file")
    pub name: String,
    /// Command to spawn the MCP server subprocess (stdio transport)
    #[serde(default)]
    pub command: String,
    /// Arguments for the command (stdio transport)
    pub args: Option<Vec<String>>,
    /// Environment variables for the subprocess (stdio transport)
    pub env: Option<HashMap<String, String>>,
    /// URL for Streamable HTTP transport. When set, `command` is ignored.
    pub url: Option<String>,
    /// Default policy for all tools from this server (overrides MCP baseline)
    pub default_policy: Option<crate::tool::ToolPolicy>,
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

/// Context window management configuration
#[derive(Debug, Deserialize, Clone)]
pub struct ContextConfig {
    /// Maximum tokens for the context window (default: 128000)
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,
    /// Tokens reserved for the LLM's response (default: 4096)
    #[serde(default = "default_reserved_output_tokens")]
    pub reserved_output_tokens: usize,
}

fn default_max_context_tokens() -> usize {
    128_000
}

fn default_reserved_output_tokens() -> usize {
    4096
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: default_max_context_tokens(),
            reserved_output_tokens: default_reserved_output_tokens(),
        }
    }
}
