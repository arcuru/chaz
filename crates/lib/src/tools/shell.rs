use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use crate::tool_host::Capability;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info};

/// Execute a shell command and return its output.
///
/// Security: High risk, always requires approval. The host enforces shell
/// grants (allowlist/denylist) at the capability boundary — the tool itself
/// does not inspect grants directly.
pub struct ShellExec;

impl Tool for ShellExec {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "shell".to_string(),
            description: "Execute a shell command and return its stdout, stderr, and exit code"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Optional working directory for the command"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::High,
            approval: ApprovalRequirement::Always,
            timeout: 30,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    crate::tool::ToolError::InvalidArgument("Missing 'command' argument".into())
                })?;

            let working_dir = arguments
                .get("working_dir")
                .and_then(|v| v.as_str())
                .map(String::from);

            info!(command = %command, "Executing shell command via host");

            let result = ctx
                .host()
                .request(
                    &Capability::Shell {
                        command: command.to_string(),
                        working_dir,
                    },
                    ctx.grants(),
                )
                .await?;

            match result {
                crate::tool_host::CapabilityResult::Shell(output) => {
                    let stdout = &output.stdout;
                    let stderr = &output.stderr;
                    let exit_code = output.exit_code;

                    let mut formatted = String::new();
                    if !stdout.is_empty() {
                        formatted.push_str(stdout);
                    }
                    if !stderr.is_empty() {
                        if !formatted.is_empty() {
                            formatted.push('\n');
                        }
                        formatted.push_str(&format!("[stderr] {stderr}"));
                    }
                    if exit_code != 0 {
                        if !formatted.is_empty() {
                            formatted.push('\n');
                        }
                        formatted.push_str(&format!("[exit code {exit_code}]"));
                    }
                    if formatted.is_empty() {
                        formatted.push_str("[no output]");
                    }

                    debug!(
                        exit_code,
                        output_len = formatted.len(),
                        "Shell command completed"
                    );

                    // Truncate very long output
                    if formatted.len() > 10000 {
                        formatted.truncate(10000);
                        formatted.push_str("\n[truncated]");
                    }

                    Ok(formatted)
                }
                _ => Err(crate::tool::ToolError::Execution(
                    "Unexpected host result for shell capability".into(),
                )),
            }
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
    fn default_policy_is_high_risk_and_requires_approval() {
        let p = ShellExec.default_policy();
        assert!(matches!(p.risk, RiskLevel::High));
        assert!(matches!(p.approval, ApprovalRequirement::Always));
        assert_eq!(p.timeout, 30);
    }

    #[test]
    fn descriptor_lists_command_required_and_working_dir_optional() {
        let d = ShellExec.descriptor();
        assert_eq!(d.name, "shell");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "command"));
        assert!(!required.iter().any(|v| v == "working_dir"));
    }

    #[tokio::test]
    async fn missing_command_argument_errors() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host.clone()).await;
        let err = ShellExec
            .execute(serde_json::json!({}), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("command"));
        // Host should not have been touched if argument validation fails first.
        assert!(host.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn forwards_command_and_working_dir_to_host() {
        let host = Arc::new(MockHost::new());
        host.push_shell("ok", "", 0);
        let (_i, c) = ctx_with(host.clone()).await;
        ShellExec
            .execute(
                serde_json::json!({ "command": "ls -la", "working_dir": "/tmp" }),
                &c,
            )
            .await
            .unwrap();
        match host.last_call().expect("one host call") {
            Capability::Shell {
                command,
                working_dir,
            } => {
                assert_eq!(command, "ls -la");
                assert_eq!(working_dir.as_deref(), Some("/tmp"));
            }
            other => panic!("unexpected capability: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stdout_only_passes_through_unannotated() {
        let host = Arc::new(MockHost::new());
        host.push_shell("hello\n", "", 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "echo hello" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "hello\n");
    }

    #[tokio::test]
    async fn stderr_is_tagged_and_appended() {
        let host = Arc::new(MockHost::new());
        host.push_shell("out", "err", 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(out.contains("out"));
        assert!(
            out.contains("[stderr] err"),
            "stderr should be tagged, got: {out}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_code_is_appended() {
        let host = Arc::new(MockHost::new());
        host.push_shell("", "", 7);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(out.contains("[exit code 7]"), "got: {out}");
    }

    #[tokio::test]
    async fn empty_output_returns_placeholder() {
        let host = Arc::new(MockHost::new());
        host.push_shell("", "", 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "true" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "[no output]");
    }

    #[tokio::test]
    async fn very_long_stdout_is_truncated_with_marker() {
        let host = Arc::new(MockHost::new());
        host.push_shell(&"a".repeat(15_000), "", 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(
            out.ends_with("[truncated]"),
            "expected truncation marker, last chars: {}",
            &out[out.len().saturating_sub(20)..]
        );
        assert!(
            out.len() < 11_000,
            "expected truncation below 11K chars, got {}",
            out.len()
        );
    }
}
