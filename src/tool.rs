use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Risk level for a tool invocation. Influences logging and approval requirements.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Safe, read-only, or trivial operations
    #[default]
    Low,
    /// Side effects but generally reversible (file writes, HTTP requests)
    Medium,
    /// Potentially dangerous or irreversible (shell execution, system changes)
    High,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "low"),
            RiskLevel::Medium => write!(f, "medium"),
            RiskLevel::High => write!(f, "HIGH"),
        }
    }
}

/// Whether a tool invocation requires explicit user approval before execution.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRequirement {
    /// Tool never needs approval
    #[default]
    Never,
    /// Needs approval unless listed in auto_approved_tools config
    UnlessAutoApproved,
    /// Always requires explicit user approval
    Always,
}

/// What a tool declares about itself — portable, durable metadata.
#[derive(Clone, Debug)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Host policy for a tool — risk, approval, timeout, sensitive params.
/// Configured by the admin, not declared by the tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolPolicy {
    #[serde(default)]
    pub risk: RiskLevel,
    #[serde(default)]
    pub approval: ApprovalRequirement,
    #[serde(default = "default_timeout_secs")]
    pub timeout: u64,
    #[serde(default)]
    pub sensitive_params: Vec<String>,
    /// Maximum calls per minute (None = unlimited)
    #[serde(default)]
    pub rate_limit: Option<u32>,
}

fn default_timeout_secs() -> u64 {
    60
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            risk: RiskLevel::Low,
            approval: ApprovalRequirement::Never,
            timeout: 60,
            sensitive_params: Vec::new(),
            rate_limit: None,
        }
    }
}

impl ToolPolicy {
    pub fn timeout_duration(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }
}

/// Tracks per-tool call timestamps for rate limiting within a single agent turn.
pub struct RateLimiter {
    /// Tool name → list of call timestamps (within the sliding window)
    calls: HashMap<String, Vec<std::time::Instant>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            calls: HashMap::new(),
        }
    }

    /// Check if a tool call is allowed under its rate limit.
    /// Returns Ok(()) if allowed, Err(message) if rate limited.
    pub fn check(&mut self, tool_name: &str, limit: u32) -> Result<(), String> {
        let now = std::time::Instant::now();
        let window = Duration::from_secs(60);

        let timestamps = self.calls.entry(tool_name.to_string()).or_default();

        // Prune expired entries
        timestamps.retain(|t| now.duration_since(*t) < window);

        if timestamps.len() >= limit as usize {
            let oldest = timestamps.first().unwrap();
            let retry_after = window.saturating_sub(now.duration_since(*oldest));
            return Err(format!(
                "Rate limited: {} exceeded {limit} calls/minute. Retry in {}s.",
                tool_name,
                retry_after.as_secs()
            ));
        }

        timestamps.push(now);
        Ok(())
    }
}

/// How a tool's definition is presented to the LLM.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationMode {
    /// Full name, description, and parameter schema
    #[default]
    Full,
    /// Name + first sentence of description, parameter names only (no descriptions)
    Brief,
    /// Name only — no description, minimal schema. Agent must use describe_tool to learn more.
    Summary,
    /// Not sent to LLM at all
    Hidden,
}

/// Controls how tool definitions are presented to the LLM.
///
/// Each tool resolves to a `PresentationMode` via: exact name match → glob prefix match → default.
/// Profiles are defined in config and referenced by agents, presets, or sessions.
#[derive(Clone, Debug)]
pub struct ToolProfile {
    pub default_mode: PresentationMode,
    pub tool_modes: HashMap<String, PresentationMode>,
}

impl Default for ToolProfile {
    fn default() -> Self {
        Self {
            default_mode: PresentationMode::Full,
            tool_modes: HashMap::new(),
        }
    }
}

impl ToolProfile {
    /// Resolve the presentation mode for a tool by name.
    /// Priority: exact match → glob prefix match (e.g., "filesystem.*") → default.
    pub fn resolve_mode(&self, tool_name: &str) -> &PresentationMode {
        // Exact match
        if let Some(mode) = self.tool_modes.get(tool_name) {
            return mode;
        }
        // Glob prefix match: "namespace.*" matches "namespace.anything"
        for (pattern, mode) in &self.tool_modes {
            if let Some(prefix) = pattern.strip_suffix(".*") {
                if tool_name.starts_with(prefix) && tool_name[prefix.len()..].starts_with('.') {
                    return mode;
                }
            }
        }
        &self.default_mode
    }

    /// Transform a tool definition according to its presentation mode.
    /// Returns None for Hidden tools.
    pub fn apply(&self, def: &ToolDefinition) -> Option<ToolDefinition> {
        match self.resolve_mode(&def.name) {
            PresentationMode::Full => Some(def.clone()),
            PresentationMode::Brief => Some(ToolDefinition {
                name: def.name.clone(),
                description: first_sentence(&def.description),
                parameters: strip_param_descriptions(&def.parameters),
            }),
            PresentationMode::Summary => Some(ToolDefinition {
                name: def.name.clone(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }),
            PresentationMode::Hidden => None,
        }
    }
}

/// Extract the first sentence from a description.
fn first_sentence(desc: &str) -> String {
    // Split on ". " or ".\n" to find first sentence
    if let Some(pos) = desc.find(". ").or_else(|| desc.find(".\n")) {
        desc[..=pos].to_string()
    } else {
        desc.to_string()
    }
}

/// Strip description fields from parameter properties, keeping only type and name info.
fn strip_param_descriptions(params: &Value) -> Value {
    let mut result = params.clone();
    if let Some(props) = result.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (_key, schema) in props.iter_mut() {
            if let Some(obj) = schema.as_object_mut() {
                obj.remove("description");
            }
        }
    }
    result
}

/// Context provided by the runtime to tools during execution.
pub struct ToolContext {
    /// Name of the agent currently executing
    pub agent_name: String,
    /// Current spawn depth (0 = root agent from gateway)
    pub call_depth: usize,
    /// Maximum allowed spawn depth
    pub max_call_depth: usize,
    /// Scoped tool set for this agent — narrowed transitively down the spawn tree
    pub tools: ScopedTools,
    /// Controls how tool definitions are presented to the LLM
    pub profile: ToolProfile,
    /// Handle to the current session (for tools that need to write entries, e.g. compact)
    pub session: std::sync::Arc<tokio::sync::Mutex<crate::session::Session>>,
}

/// A tool that can be invoked by the LLM during a ReAct loop.
///
/// Tools are object-safe via boxed futures. Implement this trait to add
/// new capabilities to the agent.
pub trait Tool: Send + Sync {
    /// Static metadata: name, description, JSON Schema parameters.
    fn descriptor(&self) -> ToolDescriptor;

    /// Execute the tool with the given arguments and runtime context.
    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

    /// Default policy for this tool. Used when no config override exists.
    /// Built-in tools override this with sensible defaults (e.g., shell → High/Always/30s).
    /// Config-level policy always takes precedence.
    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }
}

/// Information about a tool call presented to the user for approval.
#[derive(Clone, Debug)]
pub struct ToolApprovalInfo {
    pub name: String,
    /// Redacted display version of the arguments
    pub arguments_display: String,
    pub risk_level: RiskLevel,
}

/// Serializable tool definition for sending to the LLM
#[derive(Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Resolves effective policy for tools: config overrides > tool defaults.
pub struct ToolPolicyRegistry {
    overrides: HashMap<String, ToolPolicy>,
}

impl ToolPolicyRegistry {
    pub fn new(overrides: HashMap<String, ToolPolicy>) -> Self {
        Self { overrides }
    }

    pub fn empty() -> Self {
        Self {
            overrides: HashMap::new(),
        }
    }

    /// Get the effective policy for a tool: config override if present, else tool's default.
    pub fn resolve(&self, tool: &dyn Tool) -> ToolPolicy {
        let desc = tool.descriptor();
        self.overrides
            .get(&desc.name)
            .cloned()
            .unwrap_or_else(|| tool.default_policy())
    }
}

/// Registry of available tools
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.push(Box::new(tool));
    }

    pub fn register_boxed(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Get tool definitions for sending to the LLM
    #[allow(dead_code)]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|t| {
                let desc = t.descriptor();
                ToolDefinition {
                    name: desc.name,
                    description: desc.description,
                    parameters: desc.parameters,
                }
            })
            .collect()
    }

    /// Look up a tool by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.descriptor().name == name)
            .map(|t| t.as_ref())
    }
}

/// Check if a tool name matches an allowlist pattern.
///
/// Supports exact matches and glob-style `prefix.*` patterns.
/// `"filesystem.*"` matches `"filesystem.read_file"` but not `"filesystemx"`.
fn pattern_matches(pattern: &str, tool_name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(".*") {
        tool_name.starts_with(prefix) && tool_name[prefix.len()..].starts_with('.')
    } else {
        pattern == tool_name
    }
}

/// Check if a tool name is allowed by any pattern in the allowlist.
fn is_allowed_by(allowed: &[String], tool_name: &str) -> bool {
    allowed.iter().any(|p| pattern_matches(p, tool_name))
}

/// Owned, narrowable view of the tool registry.
///
/// Carries an Arc to the full registry plus an optional allowlist.
/// Allowlist entries can be exact names or glob patterns (`"filesystem.*"`).
/// Narrowing via `narrow()` produces a new ScopedTools with a tighter allowlist,
/// enabling transitive tool restriction down the agent spawn tree.
#[derive(Clone)]
pub struct ScopedTools {
    registry: Arc<ToolRegistry>,
    allowed: Option<Vec<String>>,
}

impl ScopedTools {
    pub fn new(registry: Arc<ToolRegistry>, allowed: Option<Vec<String>>) -> Self {
        Self { registry, allowed }
    }

    /// Narrow this scope further for a child agent.
    ///
    /// Returns a new ScopedTools whose allowlist is the intersection of
    /// this scope's allowlist and the child's allowed_tools.
    ///
    /// For glob patterns, a child entry is kept if the parent allows it
    /// (either by exact match or by a parent glob that covers it).
    /// For exact names, both parent and child must allow the tool.
    pub fn narrow(&self, child_allowed: Option<&[String]>) -> Self {
        let narrowed = match (&self.allowed, child_allowed) {
            (None, None) => None,
            (None, Some(c)) => Some(c.to_vec()),
            (Some(p), None) => Some(p.clone()),
            (Some(parent), Some(child)) => {
                // Keep child entries that are covered by at least one parent pattern.
                // For glob entries in child, expand them against the registry to find
                // concrete tool names, then intersect with parent.
                let mut result: Vec<String> = Vec::new();
                for child_pattern in child {
                    if child_pattern.ends_with(".*") {
                        // Child glob: expand to matching registry tools, keep if parent allows
                        for tool in &self.registry.tools {
                            let name = tool.descriptor().name;
                            if pattern_matches(child_pattern, &name)
                                && is_allowed_by(parent, &name)
                                && !result.contains(&name)
                            {
                                result.push(name);
                            }
                        }
                    } else {
                        // Exact name: keep if parent allows it
                        if is_allowed_by(parent, child_pattern) && !result.contains(child_pattern) {
                            result.push(child_pattern.clone());
                        }
                    }
                }
                Some(result)
            }
        };
        Self {
            registry: self.registry.clone(),
            allowed: narrowed,
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.allowed {
            None => self.registry.is_empty(),
            Some(allowed) => !self
                .registry
                .tools
                .iter()
                .any(|t| is_allowed_by(allowed, &t.descriptor().name)),
        }
    }

    pub fn definitions(&self, profile: &ToolProfile) -> Vec<ToolDefinition> {
        self.registry
            .tools
            .iter()
            .filter(|t| match &self.allowed {
                None => true,
                Some(allowed) => is_allowed_by(allowed, &t.descriptor().name),
            })
            .filter_map(|t| {
                let desc = t.descriptor();
                let def = ToolDefinition {
                    name: desc.name,
                    description: desc.description,
                    parameters: desc.parameters,
                };
                profile.apply(&def)
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        if let Some(allowed) = &self.allowed {
            if !is_allowed_by(allowed, name) {
                return None;
            }
        }
        self.registry.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_resolve_exact_match() {
        let profile = ToolProfile {
            default_mode: PresentationMode::Full,
            tool_modes: HashMap::from([("shell".to_string(), PresentationMode::Hidden)]),
        };
        assert_eq!(profile.resolve_mode("shell"), &PresentationMode::Hidden);
        assert_eq!(profile.resolve_mode("recall"), &PresentationMode::Full);
    }

    #[test]
    fn test_profile_resolve_glob_prefix() {
        let profile = ToolProfile {
            default_mode: PresentationMode::Full,
            tool_modes: HashMap::from([("filesystem.*".to_string(), PresentationMode::Summary)]),
        };
        assert_eq!(
            profile.resolve_mode("filesystem.read_file"),
            &PresentationMode::Summary
        );
        assert_eq!(
            profile.resolve_mode("filesystem.write_file"),
            &PresentationMode::Summary
        );
        // Not matching — no dot after prefix
        assert_eq!(profile.resolve_mode("filesystemx"), &PresentationMode::Full);
        assert_eq!(profile.resolve_mode("github.pr"), &PresentationMode::Full);
    }

    #[test]
    fn test_profile_apply_full() {
        let profile = ToolProfile::default();
        let def = ToolDefinition {
            name: "shell".to_string(),
            description: "Execute a shell command. Dangerous.".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string", "description": "The command"}}}),
        };
        let result = profile.apply(&def).unwrap();
        assert_eq!(result.description, def.description);
    }

    #[test]
    fn test_profile_apply_brief() {
        let profile = ToolProfile {
            default_mode: PresentationMode::Brief,
            tool_modes: HashMap::new(),
        };
        let def = ToolDefinition {
            name: "shell".to_string(),
            description: "Execute a shell command. This is dangerous and should be used carefully."
                .to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string", "description": "The command to run"}}}),
        };
        let result = profile.apply(&def).unwrap();
        assert_eq!(result.description, "Execute a shell command.");
        // Parameter description should be stripped
        assert!(
            result.parameters["properties"]["cmd"]
                .get("description")
                .is_none()
        );
        // But type should remain
        assert_eq!(result.parameters["properties"]["cmd"]["type"], "string");
    }

    #[test]
    fn test_profile_apply_summary() {
        let profile = ToolProfile {
            default_mode: PresentationMode::Summary,
            tool_modes: HashMap::new(),
        };
        let def = ToolDefinition {
            name: "shell".to_string(),
            description: "Execute a command".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
        };
        let result = profile.apply(&def).unwrap();
        assert_eq!(result.name, "shell");
        assert!(result.description.is_empty());
        assert!(
            result.parameters["properties"]
                .as_object()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_profile_apply_hidden() {
        let profile = ToolProfile {
            default_mode: PresentationMode::Hidden,
            tool_modes: HashMap::new(),
        };
        let def = ToolDefinition {
            name: "shell".to_string(),
            description: "Execute a command".to_string(),
            parameters: serde_json::json!({}),
        };
        assert!(profile.apply(&def).is_none());
    }

    #[test]
    fn test_first_sentence() {
        assert_eq!(first_sentence("Hello world. More text."), "Hello world.");
        assert_eq!(first_sentence("No period here"), "No period here");
        assert_eq!(first_sentence("End.\nNew line."), "End.");
    }

    #[test]
    fn test_pattern_matches_exact() {
        assert!(pattern_matches("shell", "shell"));
        assert!(!pattern_matches("shell", "shell2"));
        assert!(!pattern_matches("shell", "shel"));
    }

    #[test]
    fn test_pattern_matches_glob() {
        assert!(pattern_matches("filesystem.*", "filesystem.read_file"));
        assert!(pattern_matches("filesystem.*", "filesystem.write"));
        // Must have a dot after the prefix
        assert!(!pattern_matches("filesystem.*", "filesystemx"));
        assert!(!pattern_matches("filesystem.*", "filesystem"));
        // Different namespace
        assert!(!pattern_matches("filesystem.*", "github.pr"));
    }

    #[test]
    fn test_is_allowed_by() {
        let allowed = vec!["shell".to_string(), "filesystem.*".to_string()];
        assert!(is_allowed_by(&allowed, "shell"));
        assert!(is_allowed_by(&allowed, "filesystem.read_file"));
        assert!(is_allowed_by(&allowed, "filesystem.write"));
        assert!(!is_allowed_by(&allowed, "web_fetch"));
        assert!(!is_allowed_by(&allowed, "filesystemx"));
    }

    #[test]
    fn test_rate_limiter_allows_within_limit() {
        let mut rl = RateLimiter::new();
        assert!(rl.check("shell", 3).is_ok());
        assert!(rl.check("shell", 3).is_ok());
        assert!(rl.check("shell", 3).is_ok());
        // 4th call exceeds limit of 3
        assert!(rl.check("shell", 3).is_err());
    }

    #[test]
    fn test_rate_limiter_independent_tools() {
        let mut rl = RateLimiter::new();
        assert!(rl.check("shell", 1).is_ok());
        assert!(rl.check("shell", 1).is_err());
        // Different tool has its own counter
        assert!(rl.check("web_fetch", 1).is_ok());
    }
}
