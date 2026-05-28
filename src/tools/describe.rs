use crate::tool::{Tool, ToolContext, ToolDescriptor};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Returns the full description and parameter schema for a tool in the agent's scope.
///
/// Useful when tools are presented in `summary` or `brief` mode — the agent
/// can call this to discover full details before invoking the tool.
pub struct DescribeTool;

impl Tool for DescribeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "describe_tool".to_string(),
            description: "Get the full description and parameter schema for a tool. Use this to learn about tools before calling them.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the tool to describe"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let name = arguments
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| "Missing required parameter: name".to_string())?;

            let tool = ctx
                .tools
                .get(name)
                .ok_or_else(|| format!("Tool '{name}' not found or not available"))?;

            let desc = tool.descriptor();
            let mut output = format!("## {}\n\n{}", desc.name, desc.description);

            // Format parameters
            if let Some(props) = desc
                .parameters
                .get("properties")
                .and_then(|p| p.as_object())
                && !props.is_empty()
            {
                let required: Vec<&str> = desc
                    .parameters
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                output.push_str("\n\nParameters:\n");
                for (param_name, schema) in props {
                    let type_str = schema.get("type").and_then(|t| t.as_str()).unwrap_or("any");
                    let req = if required.contains(&param_name.as_str()) {
                        "required"
                    } else {
                        "optional"
                    };
                    let param_desc = schema
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");

                    if param_desc.is_empty() {
                        output.push_str(&format!("  - {param_name} ({type_str}, {req})\n"));
                    } else {
                        output.push_str(&format!(
                            "  - {param_name} ({type_str}, {req}): {param_desc}\n"
                        ));
                    }
                }
            }

            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fresh_session, tool_context};
    use crate::tool::{Tool, ToolError, ToolRegistry};
    use std::pin::Pin;
    use std::sync::Arc;

    /// Helper tool with one required and one optional parameter for verifying
    /// the `Parameters:` section rendering.
    struct TwoParamTool;
    impl Tool for TwoParamTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "twop".to_string(),
                description: "A tool with two params.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "req": { "type": "string", "description": "required one" },
                        "opt": { "type": "number" }
                    },
                    "required": ["req"]
                }),
            }
        }
        fn execute<'a>(
            &'a self,
            _arguments: Value,
            _ctx: &'a ToolContext,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
        {
            Box::pin(async { Ok(String::new()) })
        }
    }

    /// Tool with no parameters; exercises the empty-properties branch
    /// (no `Parameters:` section emitted).
    struct NoParamTool;
    impl Tool for NoParamTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "nop".to_string(),
                description: "No params here.".to_string(),
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
            }
        }
        fn execute<'a>(
            &'a self,
            _arguments: Value,
            _ctx: &'a ToolContext,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
        {
            Box::pin(async { Ok(String::new()) })
        }
    }

    async fn ctx_with(tools: Vec<Box<dyn Tool>>) -> (eidetica::Instance, ToolContext) {
        let (instance, session) = fresh_session().await;
        let mut reg = ToolRegistry::new();
        for t in tools {
            reg.register_boxed(t);
        }
        let ctx = tool_context(session, Arc::new(reg));
        (instance, ctx)
    }

    #[test]
    fn descriptor_advertises_describe_tool_name() {
        let d = DescribeTool.descriptor();
        assert_eq!(d.name, "describe_tool");
        assert!(
            d.parameters["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "name")
        );
    }

    #[tokio::test]
    async fn missing_name_argument_errors() {
        let (_i, c) = ctx_with(vec![]).await;
        let err = DescribeTool
            .execute(serde_json::json!({}), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("name"));
    }

    #[tokio::test]
    async fn unknown_tool_name_returns_not_found_error() {
        let (_i, c) = ctx_with(vec![]).await;
        let err = DescribeTool
            .execute(serde_json::json!({ "name": "ghost" }), &c)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ghost"));
        assert!(msg.to_lowercase().contains("not found"));
    }

    #[tokio::test]
    async fn renders_parameters_with_required_and_optional_marker() {
        let (_i, c) = ctx_with(vec![Box::new(TwoParamTool)]).await;
        let out = DescribeTool
            .execute(serde_json::json!({ "name": "twop" }), &c)
            .await
            .unwrap();
        assert!(out.starts_with("## twop"), "header line, got: {out}");
        assert!(out.contains("A tool with two params."));
        assert!(out.contains("Parameters:"));
        assert!(
            out.contains("req (string, required): required one"),
            "expected req row with description, got: {out}"
        );
        assert!(
            out.contains("opt (number, optional)"),
            "expected opt row without description, got: {out}"
        );
    }

    #[tokio::test]
    async fn no_parameters_omits_parameters_section() {
        let (_i, c) = ctx_with(vec![Box::new(NoParamTool)]).await;
        let out = DescribeTool
            .execute(serde_json::json!({ "name": "nop" }), &c)
            .await
            .unwrap();
        assert!(out.starts_with("## nop"));
        assert!(out.contains("No params here."));
        assert!(
            !out.contains("Parameters:"),
            "expected no Parameters block, got: {out}"
        );
    }
}
