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
