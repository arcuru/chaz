use crate::tool::{Tool, ToolContext, ToolDescriptor};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Evaluates a mathematical expression
pub struct Calculate;

impl Tool for Calculate {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "calculate".to_string(),
            description: "Evaluate a mathematical expression. Supports +, -, *, /, parentheses, and common functions like sqrt, sin, cos, etc.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "expression": {
                        "type": "string",
                        "description": "The mathematical expression to evaluate, e.g. '2 + 3 * 4' or 'sqrt(16)'"
                    }
                },
                "required": ["expression"]
            }),
        }
    }

    fn execute(
        &self,
        arguments: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + '_>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fresh_session, tool_context};
    use crate::tool::ToolRegistry;
    use std::sync::Arc;

    async fn ctx() -> (eidetica::Instance, crate::tool::ToolContext) {
        let (instance, session) = fresh_session().await;
        let ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        (instance, ctx)
    }

    #[test]
    fn descriptor_advertises_calculate_name_and_required_expression() {
        let d = Calculate.descriptor();
        assert_eq!(d.name, "calculate");
        assert!(d.description.to_lowercase().contains("mathematical"));
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "expression"));
    }

    #[tokio::test]
    async fn evaluates_basic_arithmetic() {
        let (_i, c) = ctx().await;
        let out = Calculate
            .execute(serde_json::json!({ "expression": "2 + 3 * 4" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "14");
    }

    #[tokio::test]
    async fn evaluates_function_call() {
        let (_i, c) = ctx().await;
        let out = Calculate
            .execute(serde_json::json!({ "expression": "sqrt(16)" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "4");
    }

    #[tokio::test]
    async fn missing_expression_argument_errors() {
        let (_i, c) = ctx().await;
        let err = Calculate
            .execute(serde_json::json!({}), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("expression"));
    }

    #[tokio::test]
    async fn invalid_expression_returns_math_error() {
        let (_i, c) = ctx().await;
        let err = Calculate
            .execute(serde_json::json!({ "expression": "++" }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("math"));
    }
}
