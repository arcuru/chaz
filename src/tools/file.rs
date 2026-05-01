use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use crate::tool_host::Capability;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info};

/// Read the contents of a file.
///
/// Filesystem grants (read paths) are enforced by the host at the
/// capability boundary. The tool itself does not inspect grants directly.
pub struct ReadFile;

impl Tool for ReadFile {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read_file".to_string(),
            description: "Read the contents of a file at the given path".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file path to read"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        use crate::tool::ToolError;
        Box::pin(async move {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'path' argument".into()))?;

            debug!(path, "Reading file via host");

            let result = ctx
                .host()
                .request(
                    &Capability::FileRead {
                        path: path.to_string(),
                    },
                    ctx.grants(),
                )
                .await?;

            match result {
                crate::tool_host::CapabilityResult::FileRead(content) => {
                    let text = String::from_utf8_lossy(&content).into_owned();
                    debug!(path, bytes = text.len(), "File read complete");

                    // Truncate very long files
                    let t = crate::util::truncate_chars(&text, 50000);
                    if t.len() < text.len() {
                        let mut truncated = t.to_string();
                        truncated.push_str("\n[truncated]");
                        Ok(truncated)
                    } else {
                        Ok(text)
                    }
                }
                _ => Err(ToolError::Execution(
                    "Unexpected host result for file read capability".into(),
                )),
            }
        })
    }
}

/// Write content to a file.
///
/// Filesystem grants (write paths) are enforced by the host at the
/// capability boundary. The tool itself does not inspect grants directly.
pub struct WriteFile;

impl Tool for WriteFile {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "write_file".to_string(),
            description: "Write content to a file at the given path. Creates the file if it doesn't exist, overwrites if it does.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file path to write to"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Medium,
            approval: ApprovalRequirement::UnlessAutoApproved,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        use crate::tool::ToolError;
        Box::pin(async move {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'path' argument".into()))?;

            let content = arguments
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'content' argument".into()))?;

            info!(path, bytes = content.len(), "Writing file via host");

            let _result = ctx
                .host()
                .request(
                    &Capability::FileWrite {
                        path: path.to_string(),
                        content: content.to_string(),
                    },
                    ctx.grants(),
                )
                .await?;

            Ok(format!("Wrote {} bytes to {path}", content.len()))
        })
    }
}
