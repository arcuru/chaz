use crate::grants::ShellGrant;
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info, warn};

/// Execute a shell command and return its output.
///
/// Security: High risk, always requires approval. The runtime supplies a
/// `ShellGrant` via `ToolContext::grants()` at execute time; if the grant is
/// absent, commands are unrestricted (matching the "no allowlist configured"
/// behavior). Denylist entries always apply.
pub struct ShellExec;

impl ShellExec {
    /// Check if a command is allowed by the grant's allowlist/denylist.
    /// An empty `allow` list means allow-all (no allowlist enforced).
    fn check_command(command: &str, grant: Option<&ShellGrant>) -> Result<(), String> {
        // Extract all command tokens (handles pipes, &&, ||, ;, $())
        let commands = Self::extract_commands(command);

        for cmd in &commands {
            let binary = cmd.trim();
            if binary.is_empty() {
                continue;
            }

            if let Some(g) = grant {
                // Check denylist first
                for denied in &g.deny {
                    if binary.starts_with(denied) {
                        return Err(format!("Command '{binary}' is denied by security policy"));
                    }
                }

                // Check allowlist if configured (non-empty)
                if !g.allow.is_empty() && !g.allow.iter().any(|allowed| binary.starts_with(allowed))
                {
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

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'command' argument".to_string())?;

            // Check against allowlist/denylist from the resolved grant
            Self::check_command(command, ctx.grants().shell.as_ref()).map_err(|e| {
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
            debug!(
                exit_code,
                output_len = result.len(),
                "Shell command completed"
            );

            // Truncate very long output
            if result.len() > 10000 {
                result.truncate(10000);
                result.push_str("\n[truncated]");
            }

            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_grant_is_permissive() {
        assert!(ShellExec::check_command("rm -rf /", None).is_ok());
    }

    #[test]
    fn test_empty_grant_is_permissive() {
        let g = ShellGrant::default();
        assert!(ShellExec::check_command("git status", Some(&g)).is_ok());
    }

    #[test]
    fn test_allowlist_restricts() {
        let g = ShellGrant {
            allow: vec!["git".to_string(), "ls".to_string()],
            deny: vec![],
        };
        assert!(ShellExec::check_command("git status", Some(&g)).is_ok());
        assert!(ShellExec::check_command("ls -la", Some(&g)).is_ok());
        assert!(ShellExec::check_command("rm -rf /", Some(&g)).is_err());
    }

    #[test]
    fn test_denylist_blocks() {
        let g = ShellGrant {
            allow: vec![],
            deny: vec!["rm".to_string()],
        };
        assert!(ShellExec::check_command("ls", Some(&g)).is_ok());
        assert!(ShellExec::check_command("rm -rf /", Some(&g)).is_err());
    }

    #[test]
    fn test_denylist_beats_allowlist() {
        let g = ShellGrant {
            allow: vec!["rm".to_string()],
            deny: vec!["rm".to_string()],
        };
        assert!(ShellExec::check_command("rm foo", Some(&g)).is_err());
    }

    #[test]
    fn test_pipes_check_every_command() {
        let g = ShellGrant {
            allow: vec!["cat".to_string()],
            deny: vec![],
        };
        assert!(ShellExec::check_command("cat file.txt", Some(&g)).is_ok());
        // A piped disallowed command is rejected
        assert!(ShellExec::check_command("cat file.txt | grep foo", Some(&g)).is_err());
    }
}
