// FIXME: Shell execution is completely unsandboxed. Commands run with the
// bot's full user permissions. This MUST be sandboxed before any untrusted
// users can interact with the bot. Options: seccomp, bubblewrap, WASM,
// container isolation, or an explicit allowlist of commands.

use crate::tool::Tool;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Execute a shell command and return its output
pub struct ShellExec;

impl Tool for ShellExec {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its stdout, stderr, and exit code"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
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
        })
    }

    fn execute(
        &self,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'command' argument".to_string())?;

            let working_dir = arguments
                .get("working_dir")
                .and_then(|v| v.as_str())
                .map(String::from);

            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);

            if let Some(dir) = &working_dir {
                cmd.current_dir(dir);
            }

            let output = cmd
                .output()
                .await
                .map_err(|e| format!("Failed to execute command: {e}"))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let mut result = String::new();
            if !stdout.is_empty() {
                result.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(&format!("[stderr] {stderr}"));
            }
            if exit_code != 0 {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(&format!("[exit code {exit_code}]"));
            }
            if result.is_empty() {
                result.push_str("[no output]");
            }

            // Truncate very long output
            if result.len() > 10000 {
                result.truncate(10000);
                result.push_str("\n[truncated]");
            }

            Ok(result)
        })
    }
}
