use crate::extension::caps::CapabilityKind;
use crate::tool::PresentationMode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for the chaz bot
#[derive(Debug, Deserialize, Clone, Default)]
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
    /// Web search tool configuration. If omitted, web search defaults to
    /// DuckDuckGo HTML scraping (no API key required).
    pub web_search: Option<WebSearchConfig>,
    /// Optional address:port for the eidetica sync HTTP server to bind to.
    /// When omitted (default), sync uses iroh P2P transport only (stable
    /// peer identity, no address management needed). Set to e.g.
    /// `0.0.0.0:8765` to also listen on HTTP, which allows remote peers
    /// to reach you via that address even without iroh connectivity.
    pub sync_listen: Option<String>,
    /// Embedding backend used to populate `embeddings:<model-id>` subtrees
    /// alongside memory writes (Searchable Memory Stage 2). Omit to run
    /// lexical-only recall.
    pub embedding: Option<EmbeddingConfig>,
    /// CLI-specific configuration (single-shot --cli mode)
    pub cli: Option<CliConfig>,
    /// Per-capability-kind operator default-provider picks for
    /// extension-providable caps (e.g. `messenger`, `memory`). When
    /// multiple extensions register impls for the same cap kind, this
    /// map chooses which one bare consumer requests
    /// (`Messenger { provider: None }`) resolve to. Single-provider
    /// kinds auto-default without needing an entry here; kinds with
    /// multiple providers and no entry stay unresolved. Host-only
    /// kinds (e.g. `session_write`) are rejected.
    #[serde(default)]
    pub capability_defaults: HashMap<CapabilityKind, String>,
    /// Per-extension agent allowlists for the `AgentStateAdmin` cap.
    /// Each entry maps an extension name (e.g. `"schedule"`) to the
    /// list of agent display names that extension's tools may access.
    /// An absent entry means unrestricted (all hosted agents visible).
    /// An empty entry (`[]`) means the cap is effectively denied.
    #[serde(default)]
    pub agent_state_allowlist: HashMap<String, Vec<String>>,
    /// Multi-agent chat-room tuning. Omit to use built-in defaults.
    pub multi_agent: Option<MultiAgentConfig>,
}

/// Tuning for multi-agent chat-room sessions (see
/// `docs/src/design/autonomous_agents.md`).
#[derive(Debug, Deserialize, Clone)]
pub struct MultiAgentConfig {
    /// Maximum length of an agent→agent "burst" — the run of
    /// consecutive agent-authored messages since the last human message
    /// or `Directive`. Once the trailing burst reaches this, further
    /// mention-chained agent wakes are suppressed until a human (or a
    /// schedule) speaks again. This is the runaway backstop. Default 6.
    #[serde(default = "default_burst_budget")]
    pub burst_budget: usize,
}

pub(crate) fn default_burst_budget() -> usize {
    6
}

/// CLI-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CliConfig {
    /// Tools to auto-approve in CLI mode (no interactive approval possible).
    /// Default: shell, write_file
    #[serde(default = "default_cli_auto_approved")]
    pub auto_approved_tools: Vec<String>,
}

pub(crate) fn default_cli_auto_approved() -> Vec<String> {
    vec!["shell".into(), "write_file".into()]
}

/// Configuration for the embedding backend that powers semantic recall.
///
/// Example:
/// ```yaml
/// embedding:
///   backend: openai
///   model: text-embedding-3-small
///   api_base: https://api.openai.com/v1
///   api_key: "${OPENAI_API_KEY}"
/// ```
///
/// Each memory write embeds `key + " " + value` and stores the vector
/// under `embeddings:<provider>/<model>` on the same DB, syncing with
/// the memory data. Multiple model subtrees coexist — switching models
/// later just leaves the old subtree dormant until reindexed.
#[derive(Debug, Deserialize, Clone)]
pub struct EmbeddingConfig {
    /// Provider kind. Currently only `openai` (any OpenAI-compatible
    /// `/v1/embeddings` endpoint).
    #[serde(default)]
    pub backend: EmbeddingBackend,
    /// Model name as the API expects (e.g. `text-embedding-3-small`).
    pub model: String,
    /// Tag used to namespace this model in the subtree name —
    /// `embeddings:<provider>/<model>`. Defaults to `openai`.
    pub provider: Option<String>,
    /// Optional override for the `/v1` base URL.
    pub api_base: Option<String>,
    /// Raw API key from config (extracted into SecretStore at startup,
    /// then cleared). Supports `${VAR}` env references.
    pub api_key: Option<String>,
    /// Opaque reference into SecretStore (set after api_key is extracted).
    #[serde(skip)]
    pub api_key_ref: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingBackend {
    #[default]
    OpenAI,
}

impl EmbeddingConfig {
    pub fn secret_key(&self) -> String {
        format!("embedding:{}/{}", self.provider_or_default(), self.model)
    }

    pub fn provider_or_default(&self) -> String {
        self.provider.clone().unwrap_or_else(|| match self.backend {
            EmbeddingBackend::OpenAI => "openai".to_string(),
        })
    }
}

/// Configuration for the `web_search` tool. Holds an ordered list of
/// backends; the tool tries them in order and falls through to the next on
/// any error. The last entry is the final answer.
///
/// Example:
/// ```yaml
/// web_search:
///   backends:
///     - type: tavily
///       api_key: "${TAVILY_API_KEY}"
///     - type: duckduckgo
/// ```
///
/// Omit the whole section to use the keyless DuckDuckGo fallback alone.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct WebSearchConfig {
    /// Ordered preference list. Empty or missing → `[duckduckgo]`.
    #[serde(default)]
    pub backends: Vec<WebSearchBackendEntry>,
}

/// One entry in `web_search.backends`. The required `type` selects the
/// provider; `api_key` / `url` are keyed by the provider's needs (Kagi/
/// Tavily/Brave/Serper need `api_key`; SearxNG needs `url`; DuckDuckGo
/// needs neither).
#[derive(Debug, Deserialize, Clone)]
pub struct WebSearchBackendEntry {
    #[serde(rename = "type")]
    pub kind: WebSearchBackendKind,
    /// Raw API key from config (extracted into SecretStore at startup, then cleared).
    /// Supports `${VAR}`/`$VAR` env references.
    pub api_key: Option<String>,
    /// Opaque reference ID into SecretStore (set after api_key is extracted).
    #[serde(skip)]
    pub api_key_ref: Option<String>,
    /// Base URL for self-hosted backends (SearxNG). Ignored for other kinds.
    pub url: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchBackendKind {
    Kagi,
    Tavily,
    Brave,
    Serper,
    /// SearxNG instance — self-hosted or public. Requires `url:` on the entry.
    #[serde(alias = "searx")]
    Searxng,
    #[default]
    DuckDuckGo,
}

/// Configuration for an agent
#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    /// Name of the agent
    pub name: String,
    /// System prompt string. The resolved text that becomes the LLM's
    /// system message. When combined with system_prompt_files, the
    /// files are concatenated first, then this string's content appended.
    pub system_prompt: Option<String>,
    /// Paths to files whose content is concatenated into the system prompt.
    pub system_prompt_files: Option<Vec<String>>,
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
    /// Whether this agent can run without user input (scheduled/schedule)
    #[serde(default)]
    pub autonomous: bool,
    /// Named override bundles for spawn-time configuration
    pub presets: Option<HashMap<String, AgentPreset>>,
    /// Tool profile name (references a key in top-level tool_profiles)
    pub tool_profile: Option<String>,
    /// Override context token limit for this agent
    pub max_context_tokens: Option<usize>,
    /// Per-tool grant overrides for this agent. Merged per-kind over
    /// `security.tool_policies.<tool>.grants`: if the agent sets `shell`
    /// grant for a tool, it replaces the config grant; unset kinds fall
    /// through to the config/default.
    pub grants: Option<HashMap<String, crate::grants::Grants>>,
    /// Memory banks to auto-attach at agent bootstrap. Each name must
    /// match a bank created via `/memory new` or listed in the hosted
    /// index. Missing banks are logged at warn and skipped — a typo in
    /// config doesn't fail startup.
    pub default_memory_banks: Option<Vec<String>>,
    /// Skill banks to auto-attach at agent bootstrap. Mirrors
    /// `default_memory_banks` — missing banks are logged at warn and
    /// skipped, auto-created on first reference if appropriate.
    pub default_skill_banks: Option<Vec<String>>,
}

/// A named bundle of overrides that can be selected at spawn time.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
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
    /// Target session identifier — session name or eidetica DB root ID.
    /// The owning agent is woken into this session (Pinned) when the
    /// schedule fires.
    pub session: String,
    /// Owning agent (display name or DB id). The schedule lives in this
    /// agent's DB and that agent is the one woken on fire. Omit to use
    /// the peer's default agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// Task instructions handed to the agent as the wake prompt
    pub task: String,
    /// Cron expression (e.g., "0 9 * * *")
    pub cron: String,
    /// Whether this schedule is active (default: true)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Optional: retire the schedule after this many fires.
    #[serde(default)]
    pub max_fires: Option<u32>,
    /// Optional: RFC 3339 timestamp after which the schedule stops
    /// firing. Whichever of `max_fires`/`expires_at` is hit first wins.
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        // The only two nominally required fields both have #[serde(default)],
        // so an empty document parses cleanly.
        let cfg: Config = serde_yaml::from_str("").unwrap();
        assert!(cfg.homeserver_url.is_empty());
        assert!(cfg.username.is_empty());
        assert!(cfg.password.is_none());
        assert!(cfg.agents.is_none());
    }

    #[test]
    fn parse_full_matrix_stub() {
        let yaml = r#"
homeserver_url: "https://matrix.org"
username: "@bot:matrix.org"
password: "s3cret"
allow_list: "@alice:matrix.org|@bob:matrix.org"
message_limit: 500
room_size_limit: 100
state_dir: "/var/lib/chaz"
chat_summary_model: "gpt-4"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.homeserver_url, "https://matrix.org");
        assert_eq!(cfg.username, "@bot:matrix.org");
        assert_eq!(cfg.password.as_deref(), Some("s3cret"));
        assert_eq!(cfg.message_limit, Some(500));
    }

    #[test]
    fn parse_agents_section() {
        let yaml = r#"
agents:
  - name: researcher
    system_prompt: "You are a research analyst."
    model: gpt-4
    tools: [web_fetch, calculate]
    max_iterations: 20
    autonomous: true
  - name: default
    max_iterations: 5
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let agents = cfg.agents.unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "researcher");
        assert_eq!(
            agents[0].system_prompt.as_deref(),
            Some("You are a research analyst.")
        );
        assert_eq!(agents[0].model.as_deref(), Some("gpt-4"));
        assert_eq!(agents[0].max_iterations, Some(20));
        assert!(agents[0].autonomous);
        assert_eq!(agents[0].tools.as_ref().unwrap().len(), 2);
        // `autonomous` defaults to false when unset
        assert!(!agents[1].autonomous);
    }

    #[test]
    fn parse_backend_roundtrip_and_defaults() {
        let yaml = r#"
backends:
  - type: openaicompatible
    name: openrouter
    api_base: "https://openrouter.ai/api/v1"
    api_key: "${OPENROUTER_KEY}"
    models:
      - name: "gpt-4"
      - name: "claude-3"
    request_timeout: 60
    max_retries: 5
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let backends = cfg.backends.unwrap();
        assert_eq!(backends.len(), 1);
        let b = &backends[0];
        assert_eq!(b.get_name(), "openrouter");
        assert_eq!(b.api_base.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(b.api_key.as_deref(), Some("${OPENROUTER_KEY}"));
        assert_eq!(b.request_timeout().as_secs(), 60);
        assert_eq!(b.max_retries(), 5);
        assert_eq!(b.models.as_ref().unwrap().len(), 2);
        // secret_key scopes to backend name
        assert_eq!(b.secret_key(), "backend:openrouter");
    }

    #[test]
    fn backend_defaults_when_fields_missing() {
        let b = Backend::new(BackendType::OpenAICompatible);
        assert_eq!(b.get_name(), "openai"); // default when name is None
        assert_eq!(b.request_timeout().as_secs(), 120);
        assert_eq!(b.max_retries(), 3);
    }

    #[test]
    fn parse_schedule_defaults_enabled_to_true() {
        let yaml = r#"
schedules:
  - name: daily_report
    session: "tui"
    task: "Generate the daily report."
    cron: "0 9 * * * *"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let schedules = cfg.schedules.unwrap();
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].name, "daily_report");
        // enabled defaults to true
        assert!(schedules[0].enabled);
    }

    #[test]
    fn parse_schedule_explicit_disabled() {
        let yaml = r#"
schedules:
  - name: paused_job
    session: "tui"
    task: "noop"
    cron: "0 0 * * * *"
    enabled: false
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.schedules.unwrap()[0].enabled);
    }

    #[test]
    fn parse_mcp_servers_stdio_and_http() {
        let yaml = r#"
mcp_servers:
  - name: filesystem
    command: npx
    args: ["-y", "@mcp/server-filesystem", "/home"]
    env:
      NODE_ENV: production
  - name: remote
    url: "http://localhost:8080/mcp"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let servers = cfg.mcp_servers.unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "filesystem");
        assert_eq!(servers[0].command, "npx");
        assert_eq!(servers[0].args.as_ref().unwrap().len(), 3);
        assert_eq!(
            servers[0].env.as_ref().unwrap().get("NODE_ENV").unwrap(),
            "production"
        );
        assert_eq!(servers[1].url.as_deref(), Some("http://localhost:8080/mcp"));
        // command defaults to empty string when unset
        assert_eq!(servers[1].command, "");
    }

    #[test]
    fn parse_context_config_defaults() {
        let yaml = "context: {}";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let ctx = cfg.context.unwrap();
        assert_eq!(ctx.max_context_tokens, 128_000);
        assert_eq!(ctx.reserved_output_tokens, 4096);
    }

    #[test]
    fn parse_context_config_overrides() {
        let yaml = r#"
context:
  max_context_tokens: 32000
  reserved_output_tokens: 2048
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let ctx = cfg.context.unwrap();
        assert_eq!(ctx.max_context_tokens, 32000);
        assert_eq!(ctx.reserved_output_tokens, 2048);
    }

    #[test]
    fn parse_security_with_legacy_fields() {
        // Legacy fields still deserialize — migration happens at startup.
        let yaml = r#"
security:
  shell_allowlist: ["git", "ls"]
  shell_denylist: ["rm -rf"]
  leak_policy: "block"
  auto_approved_tools: ["calculate", "get_time"]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let sec = cfg.security.unwrap();
        assert_eq!(sec.shell_allowlist.as_ref().unwrap().len(), 2);
        assert_eq!(sec.shell_denylist.as_ref().unwrap().len(), 1);
        assert_eq!(sec.leak_policy.as_deref(), Some("block"));
        assert_eq!(sec.auto_approved_tools.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn parse_embedding_config_minimal() {
        // Minimal: backend defaults to openai, provider falls back to
        // the backend's name.
        let yaml = r#"
embedding:
  model: text-embedding-3-small
  api_key: "${OPENAI_API_KEY}"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let e = cfg.embedding.unwrap();
        assert_eq!(e.backend, EmbeddingBackend::OpenAI);
        assert_eq!(e.model, "text-embedding-3-small");
        assert!(e.api_base.is_none());
        assert_eq!(e.provider_or_default(), "openai");
        assert_eq!(e.secret_key(), "embedding:openai/text-embedding-3-small");
        assert_eq!(e.api_key.as_deref(), Some("${OPENAI_API_KEY}"));
    }

    #[test]
    fn parse_embedding_config_with_provider_and_base() {
        let yaml = r#"
embedding:
  backend: openai
  model: nomic-embed-text
  provider: ollama
  api_base: http://localhost:11434/v1
  api_key: "ignored-by-ollama"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let e = cfg.embedding.unwrap();
        assert_eq!(e.provider_or_default(), "ollama");
        assert_eq!(e.secret_key(), "embedding:ollama/nomic-embed-text");
        assert_eq!(e.api_base.as_deref(), Some("http://localhost:11434/v1"));
    }

    #[test]
    fn agent_preset_round_trip() {
        // AgentPreset is the only config type with Serialize (for storage).
        let preset = AgentPreset {
            model: Some("gpt-4".into()),
            max_iterations: Some(10),
            tools: Some(vec!["shell".into()]),
            role_suffix: Some("be concise".into()),
            tool_profile: Some("brief".into()),
        };
        let yaml = serde_yaml::to_string(&preset).unwrap();
        let parsed: AgentPreset = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, preset);
    }

    #[test]
    fn agent_preset_defaults_all_none() {
        let preset = AgentPreset::default();
        assert!(preset.model.is_none());
        assert!(preset.max_iterations.is_none());
        assert!(preset.tools.is_none());
    }

    #[test]
    fn parse_cli_config_defaults() {
        // When no cli section is present, Config.cli is None.
        let cfg: Config = serde_yaml::from_str("").unwrap();
        assert!(cfg.cli.is_none());
    }

    #[test]
    fn parse_cli_config_empty_section() {
        // An empty `cli:` section uses the serde default (shell + write_file).
        let yaml = "cli: {}";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let cli = cfg.cli.unwrap();
        assert_eq!(cli.auto_approved_tools, vec!["shell", "write_file"]);
    }

    #[test]
    fn parse_cli_config_custom_tools() {
        let yaml = r#"
cli:
  auto_approved_tools: [shell, write_file, web_fetch]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let cli = cfg.cli.unwrap();
        assert_eq!(
            cli.auto_approved_tools,
            vec!["shell", "write_file", "web_fetch"]
        );
    }

    #[test]
    fn default_cli_auto_approved_returns_shell_and_write_file() {
        let defaults = default_cli_auto_approved();
        assert!(defaults.contains(&"shell".to_string()));
        assert!(defaults.contains(&"write_file".to_string()));
        assert_eq!(defaults.len(), 2);
    }
}
