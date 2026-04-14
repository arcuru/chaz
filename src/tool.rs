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
        }
    }
}

impl ToolPolicy {
    pub fn timeout_duration(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }
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

/// Owned, narrowable view of the tool registry.
///
/// Carries an Arc to the full registry plus an optional allowlist.
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
    pub fn narrow(&self, child_allowed: Option<&[String]>) -> Self {
        let narrowed = match (&self.allowed, child_allowed) {
            (None, None) => None,
            (None, Some(c)) => Some(c.to_vec()),
            (Some(p), None) => Some(p.clone()),
            (Some(p), Some(c)) => Some(
                c.iter()
                    .filter(|t| p.contains(t))
                    .cloned()
                    .collect(),
            ),
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
                .any(|t| allowed.contains(&t.descriptor().name)),
        }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.registry
            .tools
            .iter()
            .filter(|t| match &self.allowed {
                None => true,
                Some(allowed) => allowed.contains(&t.descriptor().name),
            })
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

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        if let Some(allowed) = &self.allowed {
            if !allowed.contains(&name.to_string()) {
                return None;
            }
        }
        self.registry.get(name)
    }
}
