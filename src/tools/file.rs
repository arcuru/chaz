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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockHost, fresh_session, tool_context_with_host};
    use crate::tool::ToolRegistry;
    use crate::tool_host::Capability;
    use std::sync::Arc;

    async fn ctx_with(host: Arc<MockHost>) -> (eidetica::Instance, ToolContext) {
        let (instance, session) = fresh_session().await;
        let ctx = tool_context_with_host(session, Arc::new(ToolRegistry::new()), host);
        (instance, ctx)
    }

    #[test]
    fn read_file_descriptor_requires_path() {
        let d = ReadFile.descriptor();
        assert_eq!(d.name, "read_file");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "path"));
    }

    #[test]
    fn write_file_descriptor_requires_path_and_content() {
        let d = WriteFile.descriptor();
        assert_eq!(d.name, "write_file");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "path"));
        assert!(required.iter().any(|v| v == "content"));
    }

    #[test]
    fn write_file_default_policy_is_medium_and_requires_approval_unless_auto() {
        let p = WriteFile.default_policy();
        assert!(matches!(p.risk, RiskLevel::Medium));
        assert!(matches!(p.approval, ApprovalRequirement::UnlessAutoApproved));
    }

    #[tokio::test]
    async fn read_file_missing_path_errors_without_host_call() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host.clone()).await;
        let err = ReadFile.execute(serde_json::json!({}), &c).await.unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("path"));
        assert!(host.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn read_file_returns_host_content_as_utf8() {
        let host = Arc::new(MockHost::new());
        host.push_file_read(b"hello\nworld".to_vec());
        let (_i, c) = ctx_with(host.clone()).await;
        let out = ReadFile
            .execute(serde_json::json!({ "path": "/etc/hosts" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "hello\nworld");
        match host.last_call().unwrap() {
            Capability::FileRead { path } => assert_eq!(path, "/etc/hosts"),
            other => panic!("unexpected capability: {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_file_truncates_huge_content_with_marker() {
        let host = Arc::new(MockHost::new());
        let big = vec![b'x'; 60_000];
        host.push_file_read(big);
        let (_i, c) = ctx_with(host).await;
        let out = ReadFile
            .execute(serde_json::json!({ "path": "/big" }), &c)
            .await
            .unwrap();
        assert!(out.ends_with("[truncated]"), "got tail: {}", &out[out.len() - 20..]);
    }

    #[tokio::test]
    async fn write_file_missing_path_errors() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host).await;
        let err = WriteFile
            .execute(serde_json::json!({ "content": "hi" }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("path"));
    }

    #[tokio::test]
    async fn write_file_missing_content_errors() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host).await;
        let err = WriteFile
            .execute(serde_json::json!({ "path": "/x" }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("content"));
    }

    #[tokio::test]
    async fn write_file_forwards_path_and_content_and_reports_byte_count() {
        let host = Arc::new(MockHost::new());
        host.push_file_write();
        let (_i, c) = ctx_with(host.clone()).await;
        let out = WriteFile
            .execute(
                serde_json::json!({ "path": "/tmp/out", "content": "hi" }),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(out, "Wrote 2 bytes to /tmp/out");
        match host.last_call().unwrap() {
            Capability::FileWrite { path, content } => {
                assert_eq!(path, "/tmp/out");
                assert_eq!(content, "hi");
            }
            other => panic!("unexpected capability: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unexpected_host_result_variant_is_execution_error() {
        // Host returns a Shell result for a FileRead request — defensive path.
        let host = Arc::new(MockHost::new());
        host.push_shell("oops", "", 0);
        let (_i, c) = ctx_with(host).await;
        let err = ReadFile
            .execute(serde_json::json!({ "path": "/x" }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("unexpected"));
    }
}
