use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Risk level for a tool invocation. Influences logging and approval requirements.
#[derive(Clone, Debug, Default, PartialEq)]
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
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ApprovalRequirement {
    /// Tool never needs approval
    #[default]
    Never,
    /// Needs approval unless listed in auto_approved_tools config
    UnlessAutoApproved,
    /// Always requires explicit user approval
    Always,
}

/// A tool that can be invoked by the LLM during a ReAct loop.
///
/// Tools are object-safe via boxed futures. Implement this trait to add
/// new capabilities to the agent.
pub trait Tool: Send + Sync {
    /// Unique name used by the LLM to invoke this tool
    fn name(&self) -> &str;

    /// Human-readable description shown to the LLM
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters
    fn parameters(&self) -> Value;

    /// Execute the tool with the given arguments, returning a text result
    fn execute(
        &self,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;

    /// Risk level for this invocation (may depend on arguments)
    fn risk_level(&self, _params: &Value) -> RiskLevel {
        RiskLevel::Low
    }

    /// Whether this invocation requires user approval
    fn requires_approval(&self, _params: &Value) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }

    /// Maximum execution time before the tool is killed
    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(60)
    }

    /// Parameter names whose values should be redacted in logs and LLM context
    fn sensitive_params(&self) -> &[&str] {
        &[]
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
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// Look up a tool by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// Get a filtered view of tools for a specific allowed set.
    /// If `allowed` is None, returns all tools (no filtering).
    pub fn filtered_view(&self, allowed: Option<&[String]>) -> FilteredTools<'_> {
        FilteredTools {
            registry: self,
            allowed: allowed.map(|a| a.to_vec()),
        }
    }
}

/// A filtered view of the tool registry, restricted to an agent's allowed tools.
pub struct FilteredTools<'a> {
    registry: &'a ToolRegistry,
    allowed: Option<Vec<String>>,
}

impl FilteredTools<'_> {
    pub fn is_empty(&self) -> bool {
        match &self.allowed {
            None => self.registry.is_empty(),
            Some(allowed) => !self
                .registry
                .tools
                .iter()
                .any(|t| allowed.contains(&t.name().to_string())),
        }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.registry
            .tools
            .iter()
            .filter(|t| match &self.allowed {
                None => true,
                Some(allowed) => allowed.contains(&t.name().to_string()),
            })
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
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
