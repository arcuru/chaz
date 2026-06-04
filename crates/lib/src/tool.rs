use crate::grants::Grants;
use crate::tool_host::ToolHost;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
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
    /// Typed capability grants (shell commands, network endpoints, fs paths).
    /// Tools read these from `ToolContext::grants()` at execute time.
    #[serde(default)]
    pub grants: Grants,
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
            grants: Grants::default(),
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

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
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
        // A limit of 0 means the tool is disabled outright — block without
        // touching `timestamps` (which may be empty and would panic on `.first()`).
        if limit == 0 {
            return Err(format!(
                "Rate limited: {tool_name} has rate_limit=0 (disabled)."
            ));
        }

        let now = std::time::Instant::now();
        let window = Duration::from_secs(60);

        let timestamps = self.calls.entry(tool_name.to_string()).or_default();

        // Prune expired entries
        timestamps.retain(|t| now.duration_since(*t) < window);

        if timestamps.len() >= limit as usize {
            // Safe: we guarded limit > 0 above, and len >= limit implies len >= 1.
            let oldest = timestamps.first().expect("non-empty per limit > 0 guard");
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
    /// Priority: exact match → glob prefix match (e.g., `"filesystem__*"`) → default.
    pub fn resolve_mode(&self, tool_name: &str) -> &PresentationMode {
        // Exact match
        if let Some(mode) = self.tool_modes.get(tool_name) {
            return mode;
        }
        // Glob prefix match: "namespace__*" matches "namespace__anything"
        for (pattern, mode) in &self.tool_modes {
            if let Some(prefix) = pattern.strip_suffix("__*")
                && tool_name.starts_with(prefix)
                && tool_name[prefix.len()..].starts_with("__")
            {
                return mode;
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
                strict: def.strict,
            }),
            PresentationMode::Summary => Some(ToolDefinition {
                name: def.name.clone(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                // Summary collapses the schema; strict has nothing left
                // to constrain, so drop the flag.
                strict: false,
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
#[derive(Clone)]
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
    /// Per-session active-extension set, used by `HookContext` to filter
    /// hook firing and by `ScopedTools` to hide tools owned by inactive
    /// extensions. Built by `Server::active_extensions_for` and threaded
    /// through `runtime::execute` from the calling agent worker.
    pub active_extensions: std::collections::HashSet<String>,
    /// Resolved capability grants for the tool currently executing.
    /// Populated by the runtime before each call with config grants merged
    /// with per-agent overlays (see `Grants::merge_over`).
    pub grants: Grants,
    /// Per-tool grant overrides from the running agent's config.
    /// The runtime merges the grants for the currently-executing tool over
    /// the tool's resolved policy grants when building each call's `grants`.
    pub agent_grants: HashMap<String, Grants>,
    /// The execution host for sandboxed capability requests.
    /// Tools go through the host for system access (shell, HTTP, filesystem)
    /// rather than calling OS APIs directly. Defaults to [`NativeToolHost`].
    pub host: Arc<dyn ToolHost>,
    /// Shared ReAct iteration budget. Seeded by the top-level Agent's
    /// `max_iterations` and inherited by descendant Workers via
    /// `spawn_worker` so nested work draws from the same pool rather than
    /// each level getting a fresh allotment. `None` = no shared budget;
    /// the runtime falls back to its built-in cap (used by tests and
    /// direct `runtime::execute` callers that don't go through the
    /// spawn machinery).
    pub iteration_budget: Option<Arc<AtomicU32>>,
}

impl ToolContext {
    /// Read the resolved capability grants for the currently-executing tool.
    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    /// Access the execution host for sandboxed capability requests.
    pub fn host(&self) -> &dyn ToolHost {
        self.host.as_ref()
    }
}

/// A tool that can be invoked by the LLM during a ReAct loop.
///
/// Tools are object-safe via boxed futures. Implement this trait to add
/// new capabilities to the agent.
/// Typed failure mode for a tool execution.
///
/// Variants carry enough information for the runtime to decide whether to
/// retry (Network), re-prompt the model with a clarification (InvalidArgument),
/// surface to the user (ApprovalDenied), or just log-and-move-on (Execution).
///
/// Tools returning plain `String` errors are converted to `ToolError::Execution`
/// via `From<String>`, so migrating a tool to a more specific variant is
/// always opt-in.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The tool didn't produce a result before its configured timeout.
    /// Usually constructed by the runtime's `tokio::time::timeout` wrapper;
    /// tools may also emit this for internal deadlines. Retryable.
    #[error("tool timed out after {secs}s")]
    Timeout {
        /// Seconds the tool was allowed before being cut off.
        secs: u64,
    },

    /// The user (or approval gate) rejected the tool call. Not retryable.
    #[error("tool approval denied")]
    ApprovalDenied,

    /// Network-level failure (HTTP, DNS, TLS). Retryable in principle — the
    /// model's next turn might choose to retry, or the runtime could do so
    /// automatically.
    #[error("network error: {0}")]
    Network(String),

    /// The LLM supplied arguments that didn't match the tool's schema or
    /// semantic expectations. Not retryable without new arguments; surfaces
    /// to the model so it can correct its call.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The tool ran but the operation itself failed (shell nonzero exit,
    /// MCP `isError: true`, filesystem permission denied, etc). Not
    /// transparently retryable.
    #[error("{0}")]
    Execution(String),
}

impl From<String> for ToolError {
    fn from(s: String) -> Self {
        ToolError::Execution(s)
    }
}

impl From<&str> for ToolError {
    fn from(s: &str) -> Self {
        ToolError::Execution(s.to_string())
    }
}

impl ToolError {
    /// True for errors where retrying the same call later might succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ToolError::Timeout { .. } | ToolError::Network(_))
    }
}

pub trait Tool: Send + Sync {
    /// Static metadata: name, description, JSON Schema parameters.
    fn descriptor(&self) -> ToolDescriptor;

    /// Execute the tool with the given arguments and runtime context.
    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>>;

    /// Default policy for this tool. Used when no config override exists.
    /// Built-in tools override this with sensible defaults (e.g., shell → High/Always/30s).
    /// Config-level policy always takes precedence.
    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    /// Whether this tool's parameter schema satisfies OpenAI strict-mode
    /// rules: every declared property listed in `required`, every nested
    /// object closed with `additionalProperties: false`, no unsupported
    /// keywords. Default false. Override to true on tools whose schemas
    /// already match. MCP tools never opt in — third-party schemas can't be
    /// assumed strict-compatible.
    fn strict_schema(&self) -> bool {
        false
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
    /// True when the schema is OpenAI-strict-compatible and the wire
    /// format should set `strict: true`. Sourced from [`Tool::strict_schema`].
    /// Reset to false by [`ToolProfile::apply`] for Summary/Hidden modes
    /// where the params are stripped.
    pub strict: bool,
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

/// One row in the ToolRegistry: the tool itself plus the extension that
/// contributed it (`None` for MCP-loaded tools, which don't yet flow
/// through the extension hub).
pub struct RegistryEntry {
    pub tool: std::sync::Arc<dyn Tool>,
    pub owner: Option<&'static str>,
}

impl RegistryEntry {
    pub fn descriptor(&self) -> ToolDescriptor {
        self.tool.descriptor()
    }
}

/// Registry of available tools. Holds `Arc<dyn Tool>` so the extension hub
/// can share ownership of each tool with the registry — built from the hub
/// at startup in `main.rs`. Owner attribution lets `ScopedTools` filter by
/// per-session active-extension set.
pub struct ToolRegistry {
    tools: Vec<RegistryEntry>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Debug-only sanity check on a `ToolDescriptor.parameters` value at
/// registration time. Catches the class of bug where a tool ships with
/// `parameters: json!({})` — technically a valid JSON Schema but
/// OpenRouter and likely others 400 with "schema must be a JSON
/// schema… got type null". A parameter-less tool should use
/// `{"type": "object", "properties": {}}` instead.
///
/// Off in release builds — the wire still rejects bad schemas, this
/// is just a louder failure mode in tests.
fn debug_assert_valid_parameters(desc: &ToolDescriptor) {
    debug_assert!(
        desc.parameters.is_object(),
        "tool {:?} registered with non-object parameters: {:?}",
        desc.name,
        desc.parameters,
    );
    debug_assert!(
        desc.parameters.get("type").is_some(),
        "tool {:?} registered with a parameters schema missing the `type` field: {} \
         — use `{{\"type\": \"object\", \"properties\": {{}}}}` for parameter-less tools",
        desc.name,
        desc.parameters,
    );
}

/// Debug-only check that a tool which opts into strict mode (via
/// [`Tool::strict_schema`]) actually has a strict-compatible schema:
/// root is `type: object`, every declared property listed in `required`,
/// `additionalProperties: false`, no unsupported keywords. Recurses
/// into nested object properties. Returns `Err(reason)` on violation.
///
/// Kept private; called from `register*` under `debug_assert!`.
fn validate_strict_schema(name: &str, params: &Value) -> Result<(), String> {
    fn check_object(path: &str, obj: &Value) -> Result<(), String> {
        let map = obj
            .as_object()
            .ok_or_else(|| format!("{path}: not an object"))?;
        let ty = map.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if ty != "object" {
            return Err(format!("{path}: expected type=object, got {ty:?}"));
        }
        match map.get("additionalProperties") {
            Some(Value::Bool(false)) => {}
            _ => {
                return Err(format!(
                    "{path}: strict mode requires `additionalProperties: false`"
                ));
            }
        }
        let empty_map = serde_json::Map::new();
        let props = map
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap_or(&empty_map);
        let required: Vec<&str> = map
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        for key in props.keys() {
            if !required.iter().any(|r| r == key) {
                return Err(format!(
                    "{path}: property {key:?} missing from `required` \
                     (strict mode forbids optional properties)"
                ));
            }
        }
        // Recurse into nested object properties — array items get the
        // same treatment when they're objects.
        for (key, sub) in props.iter() {
            let sub_path = format!("{path}.{key}");
            let sub_type = sub.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if sub_type == "object" {
                check_object(&sub_path, sub)?;
            } else if sub_type == "array"
                && let Some(items) = sub.get("items")
                && items.get("type").and_then(|v| v.as_str()) == Some("object")
            {
                check_object(&format!("{sub_path}[]"), items)?;
            }
        }
        Ok(())
    }
    check_object(name, params)
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        let desc = tool.descriptor();
        debug_assert_valid_parameters(&desc);
        if tool.strict_schema() {
            debug_assert!(
                validate_strict_schema(&desc.name, &desc.parameters).is_ok(),
                "tool {:?} opts into strict_schema but its parameters violate strict rules: {}",
                desc.name,
                validate_strict_schema(&desc.name, &desc.parameters).unwrap_err()
            );
        }
        self.tools.push(RegistryEntry {
            tool: std::sync::Arc::new(tool),
            owner: None,
        });
    }

    pub fn register_boxed(&mut self, tool: Box<dyn Tool>) {
        let desc = tool.descriptor();
        debug_assert_valid_parameters(&desc);
        if tool.strict_schema() {
            debug_assert!(
                validate_strict_schema(&desc.name, &desc.parameters).is_ok(),
                "tool {:?} opts into strict_schema but its parameters violate strict rules: {}",
                desc.name,
                validate_strict_schema(&desc.name, &desc.parameters).unwrap_err()
            );
        }
        self.tools.push(RegistryEntry {
            tool: std::sync::Arc::from(tool),
            owner: None,
        });
    }

    /// Add a tool already wrapped in an `Arc` (e.g. one held by the
    /// extension hub) attributed to its owner extension. `None` owner
    /// means "always available regardless of per-session active set".
    pub fn register_arc_owned(
        &mut self,
        tool: std::sync::Arc<dyn Tool>,
        owner: Option<&'static str>,
    ) {
        let desc = tool.descriptor();
        debug_assert_valid_parameters(&desc);
        if tool.strict_schema() {
            debug_assert!(
                validate_strict_schema(&desc.name, &desc.parameters).is_ok(),
                "tool {:?} opts into strict_schema but its parameters violate strict rules: {}",
                desc.name,
                validate_strict_schema(&desc.name, &desc.parameters).unwrap_err()
            );
        }
        self.tools.push(RegistryEntry { tool, owner });
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Look up a tool by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|e| e.tool.descriptor().name == name)
            .map(|e| e.tool.as_ref())
    }

    /// Owner extension of a tool, if any. `None` for MCP/un-attributed.
    pub fn owner_of(&self, name: &str) -> Option<&'static str> {
        self.tools
            .iter()
            .find(|e| e.tool.descriptor().name == name)
            .and_then(|e| e.owner)
    }
}

/// Check if a tool name matches an allowlist pattern.
///
/// Supports exact matches and glob-style `prefix__*` patterns.
/// `"filesystem__*"` matches `"filesystem__read_file"` but not
/// `"filesystemx"`. The `__` separator mirrors the MCP namespacing used
/// in `mcp::server::discover_and_wrap_tools` and the Anthropic Agent SDK
/// convention (`mcp__server__tool`).
fn pattern_matches(pattern: &str, tool_name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("__*") {
        tool_name.starts_with(prefix) && tool_name[prefix.len()..].starts_with("__")
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
/// Allowlist entries can be exact names or glob patterns (`"filesystem__*"`).
/// Narrowing via `narrow()` produces a new ScopedTools with a tighter allowlist,
/// enabling transitive tool restriction down the agent spawn tree.
///
/// `active_extensions` adds a per-session filter layered on top: a tool
/// whose `owner` extension isn't in the active set is hidden, *unless*
/// the tool has no owner (MCP / direct registration) — those are always
/// available because they're not subject to the extension lifecycle.
#[derive(Clone)]
pub struct ScopedTools {
    registry: Arc<ToolRegistry>,
    allowed: Option<Vec<String>>,
    active_extensions: Option<std::collections::HashSet<String>>,
}

impl ScopedTools {
    pub fn new(registry: Arc<ToolRegistry>, allowed: Option<Vec<String>>) -> Self {
        Self {
            registry,
            allowed,
            active_extensions: None,
        }
    }

    /// Apply a per-session active-extension filter. Tools owned by an
    /// extension not in `active` are hidden from `definitions` / `get`.
    /// Tools without an owner (MCP, etc.) pass regardless. Passing
    /// `None` clears the filter.
    pub fn with_active_extensions(
        mut self,
        active: Option<std::collections::HashSet<String>>,
    ) -> Self {
        self.active_extensions = active;
        self
    }

    fn passes_extension_filter(&self, owner: Option<&'static str>) -> bool {
        match (&self.active_extensions, owner) {
            (None, _) => true,
            (Some(_), None) => true, // un-attributed (MCP / direct) — always allowed
            (Some(active), Some(o)) => active.contains(o),
        }
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
                    if child_pattern.ends_with("__*") {
                        // Child glob: expand to matching registry tools, keep if parent allows
                        for entry in &self.registry.tools {
                            let name = entry.descriptor().name;
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
            active_extensions: self.active_extensions.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.registry
            .tools
            .iter()
            .filter(|e| match &self.allowed {
                None => true,
                Some(allowed) => is_allowed_by(allowed, &e.tool.descriptor().name),
            })
            .filter(|e| self.passes_extension_filter(e.owner))
            .count()
            == 0
    }

    pub fn definitions(&self, profile: &ToolProfile) -> Vec<ToolDefinition> {
        self.registry
            .tools
            .iter()
            .filter(|e| match &self.allowed {
                None => true,
                Some(allowed) => is_allowed_by(allowed, &e.tool.descriptor().name),
            })
            .filter(|e| self.passes_extension_filter(e.owner))
            .filter_map(|e| {
                let desc = e.tool.descriptor();
                let def = ToolDefinition {
                    name: desc.name,
                    description: desc.description,
                    parameters: desc.parameters,
                    strict: e.tool.strict_schema(),
                };
                profile.apply(&def)
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        if let Some(allowed) = &self.allowed
            && !is_allowed_by(allowed, name)
        {
            return None;
        }
        if !self.passes_extension_filter(self.registry.owner_of(name)) {
            return None;
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
            tool_modes: HashMap::from([("filesystem__*".to_string(), PresentationMode::Summary)]),
        };
        assert_eq!(
            profile.resolve_mode("filesystem__read_file"),
            &PresentationMode::Summary
        );
        assert_eq!(
            profile.resolve_mode("filesystem__write_file"),
            &PresentationMode::Summary
        );
        // Not matching — no `__` separator after prefix
        assert_eq!(profile.resolve_mode("filesystemx"), &PresentationMode::Full);
        assert_eq!(profile.resolve_mode("github__pr"), &PresentationMode::Full);
    }

    #[test]
    fn test_profile_apply_full() {
        let profile = ToolProfile::default();
        let def = ToolDefinition {
            name: "shell".to_string(),
            description: "Execute a shell command. Dangerous.".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string", "description": "The command"}}}),
            strict: false,
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
            strict: false,
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
            strict: false,
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
            strict: false,
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
        assert!(pattern_matches("filesystem__*", "filesystem__read_file"));
        assert!(pattern_matches("filesystem__*", "filesystem__write"));
        // Must have `__` separator after the prefix
        assert!(!pattern_matches("filesystem__*", "filesystemx"));
        assert!(!pattern_matches("filesystem__*", "filesystem"));
        // Different namespace
        assert!(!pattern_matches("filesystem__*", "github__pr"));
    }

    #[test]
    fn test_is_allowed_by() {
        let allowed = vec!["shell".to_string(), "filesystem__*".to_string()];
        assert!(is_allowed_by(&allowed, "shell"));
        assert!(is_allowed_by(&allowed, "filesystem__read_file"));
        assert!(is_allowed_by(&allowed, "filesystem__write"));
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

    #[test]
    fn test_rate_limiter_zero_limit_blocks_without_panic() {
        // Regression: limit=0 previously called timestamps.first().unwrap() on
        // an empty vec and panicked. Should return an error cleanly.
        let mut rl = RateLimiter::new();
        let err = rl.check("shell", 0).unwrap_err();
        assert!(err.contains("disabled"), "unexpected error: {err}");
    }
}
