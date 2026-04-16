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
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
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
            {
                if !props.is_empty() {
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
            }

            Ok(output)
        })
    }
}
