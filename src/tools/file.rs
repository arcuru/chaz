use crate::tool::{ApprovalRequirement, RiskLevel, Tool};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Read the contents of a file
pub struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file at the given path"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The file path to read"
                }
            },
            "required": ["path"]
        })
    }

    fn execute(
        &self,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'path' argument".to_string())?;

            let content = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| format!("Failed to read file: {e}"))?;

            // Truncate very long files
            if content.len() > 50000 {
                let mut truncated = content[..50000].to_string();
                truncated.push_str("\n[truncated]");
                Ok(truncated)
            } else {
                Ok(content)
            }
        })
    }
}

/// Write content to a file
pub struct WriteFile;

impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file at the given path. Creates the file if it doesn't exist, overwrites if it does."
    }

    fn risk_level(&self, _params: &Value) -> RiskLevel {
        RiskLevel::Medium
    }

    fn requires_approval(&self, _params: &Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
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
        })
    }

    fn execute(
        &self,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'path' argument".to_string())?;

            let content = arguments
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'content' argument".to_string())?;

            tokio::fs::write(path, content)
                .await
                .map_err(|e| format!("Failed to write file: {e}"))?;

            Ok(format!("Wrote {} bytes to {path}", content.len()))
        })
    }
}
