use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info, warn};

/// Execute a shell command and return its output.
///
/// Security: High risk, always requires approval. Supports command
/// allowlist/denylist filtering via SecurityConfig.
pub struct ShellExec {
    /// If set, only commands starting with these prefixes are allowed
    allowlist: Option<Vec<String>>,
    /// Commands starting with these prefixes are always denied
    denylist: Vec<String>,
}

impl ShellExec {
    pub fn new(allowlist: Option<Vec<String>>, denylist: Option<Vec<String>>) -> Self {
        Self {
            allowlist,
            denylist: denylist.unwrap_or_default(),
        }
    }

    /// Check if a command is allowed by the allowlist/denylist.
    fn check_command(&self, command: &str) -> Result<(), String> {
        // Extract all command tokens (handles pipes, &&, ||, ;, $())
        let commands = Self::extract_commands(command);

        for cmd in &commands {
            let binary = cmd.trim();
            if binary.is_empty() {
                continue;
            }

            // Check denylist first
            for denied in &self.denylist {
                if binary.starts_with(denied) {
                    return Err(format!("Command '{binary}' is denied by security policy"));
                }
            }

            // Check allowlist if configured
            if let Some(allowlist) = &self.allowlist {
                if !allowlist.iter().any(|allowed| binary.starts_with(allowed)) {
                    return Err(format!(
                        "Command '{binary}' is not in the allowed commands list"
                    ));
                }
            }
        }

        Ok(())
    }

    /// Extract individual command binaries from a shell command string.
    /// Handles pipes, &&, ||, ;, and $() subshells.
    fn extract_commands(command: &str) -> Vec<String> {
        let mut commands = Vec::new();
        // Split on shell operators
        for segment in command.split(['|', '&', ';']) {
            let trimmed = segment.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Take just the first word (the binary name)
            if let Some(first_word) = trimmed.split_whitespace().next() {
                // Strip leading $( or ( for subshells
                let clean = first_word.trim_start_matches("$(").trim_start_matches('(');
                if !clean.is_empty() {
                    commands.push(clean.to_string());
                }
            }
        }
        commands
    }
}

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

    fn execute(
        &self,
        arguments: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'command' argument".to_string())?;

            // Check against allowlist/denylist
            self.check_command(command).map_err(|e| {
                warn!(command = %command, "Shell command denied: {e}");
                e
            })?;

            let working_dir = arguments
                .get("working_dir")
                .and_then(|v| v.as_str())
                .map(String::from);

            info!(command = %command, "Executing shell command");
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
            debug!(exit_code, output_len = result.len(), "Shell command completed");

            // Truncate very long output
            if result.len() > 10000 {
                result.truncate(10000);
                result.push_str("\n[truncated]");
            }

            Ok(result)
        })
    }
}
