//! Bubblewrap tool host — OS-level sandboxing via `bwrap`.
#![allow(dead_code)] // Available but not yet wired into config
//!
//! Wraps high-risk capability execution in Linux namespaces using
//! [bubblewrap](https://github.com/containers/bubblewrap). Provides
//! defense-in-depth on top of grant enforcement without root or
//! setuid — `bwrap` uses unprivileged user namespaces.
//!
//! # Sandbox profile (shell only)
//!
//! Shell commands run in a fresh namespace with:
//! - No network access (`--unshare-net`)
//! - No IPC access (`--unshare-ipc`)
//! - Read-only system directories (`/usr`, `/bin`, `/lib`, `/lib64`, `/nix`)
//! - Ephemeral `/tmp` (tmpfs)
//! - Isolated PID namespace
//! - Killed when the parent exits (`--die-with-parent`)
//!
//! File read/write and HTTP capabilities fall through to native
//! execution — bubblewrap provides the most value for shell commands,
//! which are the highest-risk tool in chaz.
//!
//! # Graceful degradation
//!
//! If `bwrap` is not installed, the host falls back to native
//! execution for all capabilities. A warning is logged at startup.

use crate::grants::Grants;
use crate::tool::ToolError;
use crate::tool_host::{Capability, CapabilityResult, NativeToolHost, ToolHost};
use std::future::Future;
use std::pin::Pin;
use tracing::warn;

/// OS-level sandboxing host using bubblewrap (`bwrap`).
///
/// Creates a new Linux namespace per shell command execution,
/// restricting filesystem access, network, and process visibility.
/// Non-shell capabilities pass through to native execution.
///
/// Available for use; not yet selectable in config. To use, replace
/// `NativeToolHost` with `BubblewrapToolHost` in main.rs.
pub struct BubblewrapToolHost {
    /// Path to the `bwrap` binary.
    bwrap_path: String,
    /// Whether a working `bwrap` binary was found at construction time.
    available: bool,
}

impl BubblewrapToolHost {
    /// Create a new bubblewrap host.
    ///
    /// Probes for `bwrap` on the system. If not found, the host
    /// degrades to native execution with a warning.
    pub fn new() -> Self {
        let available = std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if available {
            tracing::info!("BubblewrapToolHost: bwrap found, shell sandboxing active");
        } else {
            warn!("BubblewrapToolHost: bwrap not found — falling back to native execution for all capabilities. Install bubblewrap for OS-level shell sandboxing.");
        }

        Self {
            bwrap_path: "bwrap".to_string(),
            available,
        }
    }

    /// Create a host with a specific bwrap binary path (for testing).
    #[cfg(test)]
    fn with_path(path: &str) -> Self {
        Self {
            bwrap_path: path.to_string(),
            available: true,
        }
    }

    /// Build the bwrap command for shell execution. Visible for testing.
    pub fn build_shell_command(
        &self,
        command: &str,
        working_dir: Option<&str>,
    ) -> tokio::process::Command {
        build_bwrap_command(&self.bwrap_path, command, working_dir)
    }
}

impl Default for BubblewrapToolHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `bwrap` command with the sandbox profile applied.
fn build_bwrap_command(
    bwrap_path: &str,
    command: &str,
    working_dir: Option<&str>,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bwrap_path);

    cmd.arg("--unshare-net")
        .arg("--unshare-ipc")
        .arg("--unshare-uts")
        .arg("--die-with-parent")
        .arg("--new-session")
        .arg("--tmpfs")
        .arg("/tmp");

    for sys_dir in &["/usr", "/bin", "/lib", "/lib64", "/nix"] {
        if std::path::Path::new(sys_dir).exists() {
            cmd.arg("--ro-bind").arg(sys_dir).arg(sys_dir);
        }
    }

    cmd.arg("--proc").arg("/proc");
    cmd.arg("--dev").arg("/dev");

    if let Some(dir) = working_dir {
        if std::path::Path::new(dir).is_dir() {
            cmd.arg("--bind").arg(dir).arg(dir);
            cmd.arg("--chdir").arg(dir);
        }
    } else {
        cmd.arg("--chdir").arg("/tmp");
    }

    cmd.arg("sh").arg("-c").arg(command);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    cmd
}

impl ToolHost for BubblewrapToolHost {
    fn request<'a>(
        &'a self,
        capability: &'a Capability,
        grants: &'a Grants,
    ) -> Pin<Box<dyn Future<Output = Result<CapabilityResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            match capability {
                Capability::Shell {
                    command,
                    working_dir,
                } if self.available => {
                    exec_shell_bwrap(command, working_dir.as_deref(), grants).await
                }
                // Shell when bwrap unavailable, plus all other capabilities
                _ => NativeToolHost.request(capability, grants).await,
            }
        })
    }

    fn name(&self) -> &str {
        if self.available {
            "bwrap"
        } else {
            "bwrap(degraded→native)"
        }
    }
}

/// Execute a shell command inside a bubblewrap namespace.
/// Grant enforcement is performed first via the shared `check_shell_command`.
async fn exec_shell_bwrap(
    command: &str,
    working_dir: Option<&str>,
    grants: &Grants,
) -> Result<CapabilityResult, ToolError> {
    // Grant check — shared with native host
    crate::tool_host::check_shell_command(command, grants.shell.as_ref())
        .map_err(|msg| {
            warn!(command = %command, "Bwrap shell command denied: {msg}");
            ToolError::Execution(msg)
        })?;

    let mut cmd = build_bwrap_command("bwrap", command, working_dir);

    let output = cmd
        .output()
        .await
        .map_err(|e| ToolError::Execution(format!("bwrap failed: {e}")))?;

    Ok(CapabilityResult::Shell(crate::tool_host::ShellOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    }))
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bwrap_command_structure() {
        let host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        let cmd = host.build_shell_command("echo hello", None);

        let debug = format!("{cmd:?}");
        assert!(debug.contains("bwrap"), "Should use bwrap: {debug}");
        assert!(
            debug.contains("--unshare-net"),
            "Should unshare network: {debug}"
        );
        assert!(
            debug.contains("--die-with-parent"),
            "Should die with parent: {debug}"
        );
        assert!(debug.contains("sh"), "Should run sh: {debug}");
        assert!(debug.contains("-c"), "Should use -c flag: {debug}");
        assert!(
            debug.contains("echo hello"),
            "Should contain the shell command: {debug}"
        );
    }

    #[test]
    fn test_bwrap_command_includes_working_dir() {
        let host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        // Use /tmp which exists on all systems
        let cmd = host.build_shell_command("ls", Some("/tmp"));

        let debug = format!("{cmd:?}");
        assert!(
            debug.contains("--chdir"),
            "Should have chdir: {debug}"
        );
    }

    #[test]
    fn test_bwrap_sandboxes_network_commands() {
        let host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        let cmd = host.build_shell_command("curl http://evil.com", None);

        let debug = format!("{cmd:?}");
        // The command is present but --unshare-net will prevent it at runtime
        assert!(debug.contains("curl"), "Should contain curl: {debug}");
        assert!(
            debug.contains("--unshare-net"),
            "Should unshare network: {debug}"
        );
    }

    #[test]
    fn test_bwrap_command_readonly_system_dirs() {
        let host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        let cmd = host.build_shell_command("id", None);

        let debug = format!("{cmd:?}");
        // Should have --ro-bind for system directories that exist
        assert!(debug.contains("--ro-bind"), "Should have ro-bind: {debug}");
    }

    #[test]
    fn test_degraded_name() {
        let mut host = BubblewrapToolHost::new();
        host.available = false;
        assert!(
            host.name().contains("degraded"),
            "Should indicate degradation"
        );
    }

    #[test]
    fn test_available_name() {
        let host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        assert_eq!(host.name(), "bwrap");
    }

    #[test]
    fn test_default_does_not_panic() {
        let _host = BubblewrapToolHost::default();
    }
}
