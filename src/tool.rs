use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

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
}

// === Built-in tools ===

/// Returns the current date and time in UTC
pub struct GetTime;

impl Tool for GetTime {
    fn name(&self) -> &str {
        "get_time"
    }

    fn description(&self) -> &str {
        "Get the current date and time in UTC"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn execute(
        &self,
        _arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async { Ok(chrono::Utc::now().to_rfc3339()) })
    }
}

/// Evaluates a mathematical expression
pub struct Calculate;

impl Tool for Calculate {
    fn name(&self) -> &str {
        "calculate"
    }

    fn description(&self) -> &str {
        "Evaluate a mathematical expression. Supports +, -, *, /, parentheses, and common functions like sqrt, sin, cos, etc."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "The mathematical expression to evaluate, e.g. '2 + 3 * 4' or 'sqrt(16)'"
                }
            },
            "required": ["expression"]
        })
    }

    fn execute(
        &self,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let expr = arguments
                .get("expression")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'expression' argument".to_string())?;

            let result: f64 = meval::eval_str(expr).map_err(|e| format!("Math error: {e}"))?;
            Ok(result.to_string())
        })
    }
}
