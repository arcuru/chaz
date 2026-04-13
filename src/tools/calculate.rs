use crate::tool::Tool;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

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
