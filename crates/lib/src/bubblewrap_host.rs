//! Bubblewrap tool host — OS-level sandboxing via `bwrap`.
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
//! - The working directory (when supplied) bind-mounted read-write; every
//!   other path is simply absent from the mount namespace
//! - No `/proc` — the host's PID namespace and `/proc/<pid>/root` escape
//!   hatch are not exposed
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
/// Selectable via `tool_host: bubblewrap` in config; the default is
/// [`NativeToolHost`]. When `bwrap` is absent from `PATH` the host
/// degrades to native execution for every capability.
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
            warn!(
                "BubblewrapToolHost: bwrap not found — falling back to native execution for all capabilities. Install bubblewrap for OS-level shell sandboxing."
            );
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

    // `/proc` is deliberately *not* mounted: without `--unshare-pid` a fresh
    // procfs still shows the host's PID namespace, leaking the host process
    // list and re-exposing the host root through `/proc/<pid>/root`.
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
    crate::tool_host::check_shell_command(command, grants.shell.as_ref()).map_err(|msg| {
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
        assert!(debug.contains("--chdir"), "Should have chdir: {debug}");
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

// ── Integration tests (real bwrap execution) ───────────────────
//
// These run a *real* `bwrap` sandbox and assert on the denials — the whole
// point of the host. They skip (rather than fail) when bwrap is absent from
// PATH or the kernel refuses unprivileged user namespaces, so they stay green
// on systems without bubblewrap while still exercising the boundary where it
// is available.
#[cfg(test)]
mod integration {
    use super::*;
    use crate::tool_host::ShellOutput;

    /// Run `command` through a freshly-built bwrap host and return the shell
    /// output. Panics if the host is degraded or returns a non-shell result —
    /// a degraded host silently falls through to native execution, which would
    /// make every "denied" assertion below a false pass.
    async fn run(command: &str, working_dir: Option<&str>) -> ShellOutput {
        let host = BubblewrapToolHost::new();
        assert!(host.available, "bwrap must be usable for this test");
        let result = host
            .request(
                &Capability::Shell {
                    command: command.to_string(),
                    working_dir: working_dir.map(String::from),
                },
                &Grants::default(),
            )
            .await
            .expect("host request should not error");
        match result {
            CapabilityResult::Shell(o) => o,
            other => panic!("expected shell output, got {other:?}"),
        }
    }

    /// Probe whether a real sandbox can be created here: bwrap on PATH *and*
    /// unprivileged user namespaces enabled. Binds the host root read-only so
    /// the probe exercises the same mount-namespace path the host uses rather
    /// than a no-op `true`.
    fn bwrap_usable() -> bool {
        std::process::Command::new("bwrap")
            .args([
                "--unshare-net",
                "--unshare-ipc",
                "--ro-bind",
                "/",
                "/",
                "--chdir",
                "/",
                "true",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn network_access_is_denied_with_a_useful_error() {
        if !bwrap_usable() {
            eprintln!("skipping: bwrap sandbox unavailable");
            return;
        }
        // `/dev/tcp` is a bash builtin: a successful connect would print
        // CONNECTED. `--unshare-net` must make the connect fail instead, and
        // the failure should carry a diagnostic on stderr.
        let out = run(
            "cat < /dev/tcp/1.1.1.1/80 2>/dev/null && echo CONNECTED || echo BLOCKED",
            None,
        )
        .await;
        assert!(
            !out.stdout.contains("CONNECTED"),
            "a sandboxed shell reached the network: {}",
            out.stdout
        );
        assert!(out.stdout.contains("BLOCKED"), "got: {}", out.stdout);
        assert!(
            !out.stderr.is_empty(),
            "a denied connection should surface a diagnostic on stderr"
        );
    }

    #[tokio::test]
    async fn write_to_readonly_system_dir_is_denied() {
        if !bwrap_usable() {
            eprintln!("skipping: bwrap sandbox unavailable");
            return;
        }
        let out = run("echo PWNED > /nix/bwrap-escape-test", None).await;
        assert_ne!(out.exit_code, 0, "write to /nix must fail, got {out:?}");
        assert!(
            !out.stderr.is_empty(),
            "a denied write should surface a useful error on stderr"
        );
    }

    #[tokio::test]
    async fn write_outside_working_dir_is_denied() {
        if !bwrap_usable() {
            eprintln!("skipping: bwrap sandbox unavailable");
            return;
        }
        // `/etc` is not mounted into the namespace at all, so the path is
        // absent and the write fails without touching the host.
        let out = run("echo PWNED > /etc/bwrap-escape-test", None).await;
        assert_ne!(out.exit_code, 0, "write to /etc must fail, got {out:?}");
        assert!(
            !out.stderr.is_empty(),
            "a denied write should surface a useful error on stderr"
        );
    }

    #[tokio::test]
    async fn write_to_working_dir_is_allowed() {
        if !bwrap_usable() {
            eprintln!("skipping: bwrap sandbox unavailable");
            return;
        }
        let dir = std::env::temp_dir().join(format!("chaz-bwrap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = run("echo hello > out.txt", Some(dir.to_str().unwrap())).await;
        assert_eq!(
            out.exit_code, 0,
            "write inside workdir must succeed: {out:?}"
        );
        // The working dir is bind-mounted read-write, so the file is visible
        // on the host at the same path.
        let content = std::fs::read_to_string(dir.join("out.txt")).unwrap();
        assert_eq!(content.trim(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn host_pid_namespace_is_not_exposed() {
        if !bwrap_usable() {
            eprintln!("skipping: bwrap sandbox unavailable");
            return;
        }
        // `/proc` is deliberately not mounted; `test -d /proc` must report it
        // absent rather than listing the host's processes.
        let out = run(
            "test -d /proc && echo PROC_EXISTS || echo PROC_ABSENT",
            None,
        )
        .await;
        assert!(
            out.stdout.contains("PROC_ABSENT"),
            "/proc must not be mounted, got: {}",
            out.stdout
        );
    }
}
