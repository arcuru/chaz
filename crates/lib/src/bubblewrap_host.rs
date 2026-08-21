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
//! - A private PID namespace (`--unshare-pid`): host PIDs are not
//!   addressable from inside, so host processes cannot be signalled
//! - An empty environment (`--clearenv`) restored with only fixed
//!   `PATH`, `HOME`, and `LANG` values — provider keys and other host
//!   secrets never reach a shell command
//! - Read-only `/usr`, `/bin`, `/lib`, `/lib64`, `/nix/store`, and
//!   `/run/current-system`. `/nix/store` rather than all of `/nix`, so
//!   the Nix daemon socket under `/nix/var/nix` is not exposed
//!   (read-only mounts still permit Unix-socket connects)
//! - Ephemeral `/tmp` (tmpfs) and a minimal `/dev`
//! - No `/proc`: mounting a procfs for the new PID namespace is refused
//!   with `EPERM` on some hardened kernels, and its absence is strictly
//!   stronger confinement — the host process list and the
//!   `/proc/<pid>/root` escape hatch are simply not reachable
//! - Killed when the parent exits (`--die-with-parent`)
//! - A writable working directory **only** when the requested directory
//!   canonicalizes inside one of the configured writable roots
//!   (`tool_host_writable_roots`). Symlinks and `..` are resolved before
//!   the check, so they cannot smuggle a path outside a root. A root that
//!   canonicalizes to `/` is refused at construction — a writable root of
//!   `/` would mount the entire host filesystem read-write. The writable
//!   working-dir bind is applied *before* the read-only system binds, so
//!   the read-only mounts always win even if a working dir overlaps an
//!   ancestor of one of them. With no roots configured, commands run
//!   against the ephemeral `/tmp` and no host path is mounted read-write.
//!
//! File read/write and HTTP capabilities fall through to the native
//! host's grant-checked execution — bubblewrap provides the most value
//! for shell commands, which are the highest-risk tool in chaz.
//!
//! # Fail-closed selection
//!
//! Selecting `tool_host: bubblewrap` requires a working `bwrap`: the
//! probe at startup actually creates a sandbox, and if that fails the
//! server refuses to start rather than silently running unsandboxed.
//! Degrading to native execution is a separate, explicit choice
//! (`tool_host: bubblewrap-unsafe-fallback`).

use crate::grants::Grants;
use crate::tool::ToolError;
use crate::tool_host::{Capability, CapabilityResult, NativeToolHost, ToolHost};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tracing::warn;

/// Fixed environment restored inside the sandbox after `--clearenv`.
const SANDBOX_PATH: &str = "/run/current-system/sw/bin:/usr/bin:/bin";
const SANDBOX_HOME: &str = "/tmp";
const SANDBOX_LANG: &str = "C.UTF-8";

/// Read-only system directories bound into every sandbox (when present).
const RO_BINDS: &[&str] = &[
    "/usr",
    "/bin",
    "/lib",
    "/lib64",
    // The store only — all of /nix would expose the daemon socket under
    // /nix/var/nix/daemon-socket, and a read-only mount still allows
    // connecting to a Unix socket.
    "/nix/store",
    "/run/current-system",
];

/// OS-level sandboxing host using bubblewrap (`bwrap`).
///
/// Creates a new Linux namespace per shell command execution,
/// restricting filesystem access, network, and process visibility.
/// Non-shell capabilities (file read/write, HTTP) are not namespaced —
/// they pass through to the native host's grant-checked execution.
///
/// Selected via `tool_host: bubblewrap` in config. Construction probes
/// for a *working* `bwrap`; if the probe fails the host reports itself
/// unavailable and shell requests error out — the server decides at
/// startup whether that is fatal (`bubblewrap`) or an explicit,
/// acknowledged degradation (`bubblewrap-unsafe-fallback`).
pub struct BubblewrapToolHost {
    /// Path to the `bwrap` binary.
    bwrap_path: String,
    /// Whether a working `bwrap` was found at construction time.
    available: bool,
    /// Canonical directories a working dir may live in. Empty means no
    /// host path is ever mounted read-write.
    writable_roots: Vec<PathBuf>,
}

impl BubblewrapToolHost {
    /// Create a host with no writable roots: shell commands always run
    /// against the ephemeral `/tmp`.
    pub fn new() -> Self {
        Self::with_roots(Vec::new())
            .expect("no writable roots configured, so construction cannot fail")
    }

    /// Create a host whose shell commands may be given a working dir
    /// under one of `roots` (canonicalized here). Each root must exist;
    /// missing roots are dropped with a warning rather than silently
    /// widening later. A root that canonicalizes to the filesystem root
    /// `/` is refused with an error — it would mount the entire host
    /// filesystem read-write, nullifying the sandbox.
    pub fn with_roots(roots: Vec<PathBuf>) -> Result<Self, String> {
        let mut writable_roots = Vec::new();
        for r in roots {
            match r.canonicalize() {
                Ok(c) if c == Path::new("/") => {
                    return Err(format!(
                        "tool_host_writable_roots entry {:?} canonicalizes to the filesystem root `/` — \
                         a writable root of `/` would mount the entire host filesystem read-write \
                         inside the sandbox; configure narrower roots",
                        r.display()
                    ));
                }
                Ok(c) if c.is_dir() => writable_roots.push(c),
                Ok(_) => {
                    warn!(root = %r.display(), "tool_host_writable_roots entry is not a directory — ignored");
                }
                Err(e) => {
                    warn!(root = %r.display(), error = %e, "tool_host_writable_roots entry does not exist — ignored");
                }
            }
        }

        let available = probe_bwrap();

        if available {
            tracing::info!("BubblewrapToolHost: bwrap sandbox probe succeeded");
        } else {
            warn!(
                "BubblewrapToolHost: bwrap could not create a sandbox — the host will deny shell execution. Install bubblewrap and ensure unprivileged user namespaces work, or select tool_host: bubblewrap-unsafe-fallback to acknowledge native fallback."
            );
        }

        Ok(Self {
            bwrap_path: "bwrap".to_string(),
            available,
            writable_roots,
        })
    }

    /// Whether a working bwrap was found at construction time.
    pub fn is_available(&self) -> bool {
        self.available
    }

    /// Build the bwrap command for shell execution. Visible for testing.
    ///
    /// Fails when `working_dir` cannot be confined: it must canonicalize
    /// (symlinks and `..` resolved) to a directory inside one of the
    /// configured writable roots.
    pub fn build_shell_command(
        &self,
        command: &str,
        working_dir: Option<&str>,
    ) -> Result<std::process::Command, String> {
        let resolved = match working_dir {
            Some(dir) => Some(resolve_working_dir(dir, &self.writable_roots)?),
            None => None,
        };
        build_bwrap_command(&self.bwrap_path, command, resolved.as_deref())
    }
}

impl Default for BubblewrapToolHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Probe that `bwrap` can actually create a sandbox here — not merely
/// that the binary exists. Runs the real profile with a trivial command.
fn probe_bwrap() -> bool {
    let Ok(mut cmd) = build_bwrap_command("bwrap", "true", None) else {
        return false;
    };
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Resolve a model-supplied working dir to a confineable path.
///
/// `canonicalize` resolves symlinks and `..` components, so a symlink
/// planted under an allowed root or a `..` walk cannot land outside it:
/// the check runs on the final target, not the requested spelling.
fn resolve_working_dir(dir: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let canonical = Path::new(dir)
        .canonicalize()
        .map_err(|e| format!("working dir {dir:?} is not accessible: {e}"))?;
    if !canonical.is_dir() {
        return Err(format!("working dir {dir:?} is not a directory"));
    }
    if roots.iter().any(|r| canonical.starts_with(r)) {
        Ok(canonical)
    } else {
        Err(format!(
            "working dir {dir:?} resolves outside every configured writable root — refusing to bind it read-write"
        ))
    }
}

/// Build a `bwrap` command with the sandbox profile applied.
fn build_bwrap_command(
    bwrap_path: &str,
    command: &str,
    working_dir: Option<&Path>,
) -> Result<std::process::Command, String> {
    let mut cmd = std::process::Command::new(bwrap_path);

    cmd.args([
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        // Private PID namespace: host PIDs become unaddressable, so a
        // same-UID host process cannot be signalled from inside.
        "--unshare-pid",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--setenv",
        "PATH",
        SANDBOX_PATH,
        "--setenv",
        "HOME",
        SANDBOX_HOME,
        "--setenv",
        "LANG",
        SANDBOX_LANG,
        "--tmpfs",
        "/tmp",
        "--dev",
        "/dev",
    ]);

    // The writable working-dir bind is emitted *before* the read-only
    // system binds: bwrap applies mounts in argv order, and a writable
    // bind mounted after a read-only one would shadow it — a working dir
    // that is an ancestor of a system dir would turn that system dir
    // writable. Mounted first, the read-only binds land on top and win.
    if let Some(dir) = working_dir {
        cmd.arg("--bind").arg(dir).arg(dir);
    }

    for sys_dir in RO_BINDS {
        if Path::new(sys_dir).exists() {
            cmd.arg("--ro-bind").arg(sys_dir).arg(sys_dir);
        }
    }

    // `/proc` is deliberately *not* mounted: mounting a procfs for the
    // fresh PID namespace is refused with EPERM on some kernels, and its
    // absence is stronger confinement anyway — no host process list and
    // no `/proc/<pid>/root` escape hatch.
    if let Some(dir) = working_dir {
        cmd.arg("--chdir").arg(dir);
    } else {
        cmd.arg("--chdir").arg("/tmp");
    }

    cmd.arg("sh").arg("-c").arg(command);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    Ok(cmd)
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
                } => exec_shell_bwrap(self, command, working_dir.as_deref(), grants).await,
                // File and HTTP capabilities run on the native host's
                // grant-checked paths; the namespace adds nothing for them.
                _ => NativeToolHost.request(capability, grants).await,
            }
        })
    }

    fn name(&self) -> &str {
        if self.available {
            "bwrap"
        } else {
            "bwrap(unavailable)"
        }
    }
}

/// Execute a shell command inside a bubblewrap namespace.
/// Grant enforcement is performed first via the shared `check_shell_command`.
async fn exec_shell_bwrap(
    host: &BubblewrapToolHost,
    command: &str,
    working_dir: Option<&str>,
    grants: &Grants,
) -> Result<CapabilityResult, ToolError> {
    if !host.available {
        return Err(ToolError::Execution(
            "shell execution denied: tool_host is bubblewrap but no working bwrap sandbox \
             is available on this system"
                .to_string(),
        ));
    }

    // Grant check — shared with native host
    crate::tool_host::check_shell_command(command, grants.shell.as_ref()).map_err(|msg| {
        warn!(command = %command, "Bwrap shell command denied: {msg}");
        ToolError::Execution(msg)
    })?;

    let mut cmd = tokio::process::Command::from(build_shell_cmd(host, command, working_dir)?);

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

/// Validate the working dir and build the command, mapping confinement
/// failures onto [`ToolError`].
fn build_shell_cmd(
    host: &BubblewrapToolHost,
    command: &str,
    working_dir: Option<&str>,
) -> Result<std::process::Command, ToolError> {
    host.build_shell_command(command, working_dir)
        .map_err(|msg| {
            warn!(command = %command, "Bwrap working dir rejected: {msg}");
            ToolError::Execution(msg)
        })
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn host_with_root(root: &str) -> BubblewrapToolHost {
        BubblewrapToolHost {
            bwrap_path: "/usr/bin/bwrap".to_string(),
            available: true,
            writable_roots: vec![PathBuf::from(root)],
        }
    }

    #[test]
    fn test_bwrap_command_structure() {
        let host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        let cmd = host.build_shell_command("echo hello", None).unwrap();

        let debug = format!("{cmd:?}");
        assert!(debug.contains("bwrap"), "Should use bwrap: {debug}");
        assert!(
            debug.contains("--unshare-net"),
            "Should unshare network: {debug}"
        );
        assert!(
            debug.contains("--unshare-pid"),
            "Should unshare PIDs: {debug}"
        );
        assert!(
            debug.contains("--clearenv"),
            "Should clear the environment: {debug}"
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
    fn test_environment_is_minimal_and_fixed() {
        let host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        let cmd = host.build_shell_command("env", None).unwrap();
        let debug = format!("{cmd:?}");
        assert!(debug.contains("--clearenv"), "{debug}");
        assert!(debug.contains("\"PATH\""), "{debug}");
        assert!(debug.contains(SANDBOX_PATH), "{debug}");
        assert!(debug.contains("\"HOME\""), "{debug}");
        assert!(debug.contains("\"LANG\""), "{debug}");
        // No way for caller-supplied env to leak: the builder never reads
        // the parent environment at all.
    }

    #[test]
    fn test_nix_store_bound_not_all_of_nix() {
        let host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        let cmd = host.build_shell_command("id", None).unwrap();
        let debug = format!("{cmd:?}");
        assert!(debug.contains("/nix/store"), "{debug}");
        assert!(
            !debug.contains("\"/nix\""),
            "all of /nix must not be bound (daemon socket exposure): {debug}"
        );
    }

    #[test]
    fn test_bwrap_command_includes_confined_working_dir() {
        let dir = std::env::temp_dir();
        let host = host_with_root(dir.to_str().unwrap());
        let cmd = host
            .build_shell_command("ls", Some(dir.to_str().unwrap()))
            .unwrap();
        let debug = format!("{cmd:?}");
        assert!(debug.contains("--chdir"), "Should have chdir: {debug}");
        assert!(debug.contains("--bind"), "Should bind the workdir: {debug}");
    }

    #[test]
    fn test_working_dir_outside_roots_is_rejected() {
        let host = host_with_root("/nonexistent-root-for-tests");
        // Paths that exist on every Linux host (the Nix build sandbox has
        // no /home) but outside the configured root.
        for dir in ["/", "/etc"] {
            let err = host
                .build_shell_command("ls", Some(dir))
                .expect_err("must reject dir outside roots");
            assert!(err.contains("writable root"), "{dir}: {err}");
        }
    }

    #[test]
    fn test_root_writable_root_is_rejected() {
        // A configured writable root that canonicalizes to `/` would mount
        // the entire host filesystem read-write — refuse it at construction,
        // whatever spelling the operator used.
        for root in ["/", "/./", "/tmp/.."] {
            let err = BubblewrapToolHost::with_roots(vec![PathBuf::from(root)])
                .err()
                .expect("writable root canonicalizing to / must be rejected");
            assert!(
                err.contains("filesystem root"),
                "{root}: expected root-refusal error, got: {err}"
            );
        }
    }

    #[test]
    fn test_writable_bind_precedes_readonly_system_binds() {
        // bwrap applies mounts in argv order, so a writable working-dir
        // bind mounted after a read-only system bind would shadow it — a
        // working dir that is an ancestor of a system dir would turn that
        // system dir writable. The writable bind must therefore come first.
        let workdir = std::env::temp_dir();
        let host = host_with_root(workdir.to_str().unwrap());
        let cmd = host
            .build_shell_command("id", Some(workdir.to_str().unwrap()))
            .unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let workdir_str = workdir.to_str().unwrap();
        let writable_idx = args
            .windows(3)
            .position(|w| w[0] == "--bind" && w[1] == workdir_str && w[2] == workdir_str)
            .expect("writable working-dir bind must be present");

        let ro_idxs: Vec<usize> = args
            .windows(3)
            .enumerate()
            .filter(|(_, w)| w[0] == "--ro-bind")
            .map(|(i, _)| i)
            .collect();
        assert!(
            !ro_idxs.is_empty(),
            "read-only system binds must be present: {args:?}"
        );
        for ro in ro_idxs {
            assert!(
                writable_idx < ro,
                "writable bind at {writable_idx} must precede read-only bind at {ro}: {args:?}"
            );
        }
    }

    #[test]
    fn test_working_dir_dotdot_escape_is_rejected() {
        // A root of /tmp plus a ".." walk must not widen past the root:
        // canonicalization happens before the containment check.
        let host = host_with_root("/tmp");
        let err = host
            .build_shell_command("ls", Some("/tmp/../etc"))
            .expect_err(".. escape must be rejected");
        assert!(err.contains("writable root"), "{err}");
    }

    #[test]
    fn test_working_dir_symlink_escape_is_rejected() {
        let base = std::env::temp_dir().join(format!("chaz-bwrap-sym-{}", std::process::id()));
        let inside = base.join("inside");
        let outside = std::env::temp_dir().join("chaz-bwrap-sym-outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = inside.join("escape");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let host = host_with_root(base.to_str().unwrap());
        let err = host
            .build_shell_command("ls", Some(link.to_str().unwrap()))
            .expect_err("symlink escape must be rejected");
        assert!(err.contains("writable root"), "{err}");

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn test_unavailable_host_denies_shell() {
        let mut host = BubblewrapToolHost::with_path("/usr/bin/bwrap");
        host.available = false;
        assert_eq!(host.name(), "bwrap(unavailable)");
    }

    #[test]
    fn test_default_does_not_panic() {
        let _host = BubblewrapToolHost::default();
    }
}

// Keep the test-only constructor next to its users.
impl BubblewrapToolHost {
    /// Create a host with a specific bwrap binary path (for testing).
    #[cfg(test)]
    fn with_path(path: &str) -> Self {
        Self {
            bwrap_path: path.to_string(),
            available: true,
            writable_roots: Vec::new(),
        }
    }

    /// Create a host with explicit availability (for testing host
    /// *selection* in other modules without probing the real system).
    #[cfg(test)]
    pub(crate) fn test_host(available: bool) -> Self {
        Self {
            bwrap_path: "/usr/bin/bwrap".to_string(),
            available,
            writable_roots: Vec::new(),
        }
    }
}

// ── Integration tests (real bwrap execution) ───────────────────
//
// These run a *real* `bwrap` sandbox and assert on the denials — the whole
// point of the host. They compile and hard-fail on Linux: CI builds there
// with bwrap on PATH, so a silent skip would be a hole in the gate. On
// non-Linux targets the module is not compiled at all.
#[cfg(all(test, target_os = "linux"))]
mod integration {
    use super::*;
    use crate::tool_host::ShellOutput;

    /// Run `command` through a freshly-built bwrap host and return the shell
    /// output. Panics unless the host probed healthy — an unavailable host
    /// denies everything, which would make every "denied" assertion below a
    /// false pass.
    async fn run(command: &str, working_dir: Option<&str>) -> ShellOutput {
        let host = BubblewrapToolHost::new();
        assert!(
            host.is_available(),
            "bwrap must be usable for these tests: install bubblewrap / enable unprivileged user namespaces"
        );
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

    fn require_bwrap() {
        assert!(
            bwrap_usable(),
            "these tests require a working bwrap sandbox on Linux"
        );
    }

    #[tokio::test]
    async fn network_access_is_denied() {
        require_bwrap();
        // Portable across shells (no bashisms): with --unshare-net the
        // namespace has no interfaces beyond loopback, so anything listed
        // under /sys/class/net beyond `lo` means the sandbox leaked the
        // host network namespace.
        let out = run(
            "for i in /sys/class/net/* /sys/class/net/.[!.]*; do \
                 [ -e \"$i\" ] && echo \"${i##*/}\"; \
             done",
            None,
        )
        .await;
        let interfaces: Vec<&str> = out.stdout.split_whitespace().collect();
        assert!(
            interfaces.iter().all(|i| *i == "lo"),
            "a sandboxed shell saw non-loopback interfaces: {:?}",
            interfaces
        );
    }

    #[tokio::test]
    async fn write_to_readonly_system_dir_is_denied() {
        require_bwrap();
        let out = run("echo PWNED > /nix/store/bwrap-escape-test", None).await;
        assert_ne!(
            out.exit_code, 0,
            "write to /nix/store must fail, got {out:?}"
        );
        assert!(
            !out.stderr.is_empty(),
            "a denied write should surface a useful error on stderr"
        );
    }

    #[tokio::test]
    async fn write_outside_working_dir_is_denied() {
        require_bwrap();
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
    async fn working_dir_outside_configured_roots_is_denied_at_runtime() {
        require_bwrap();
        // No writable roots configured → "/" must be refused outright
        // rather than bound read-write.
        let host = BubblewrapToolHost::new();
        let result = host
            .request(
                &Capability::Shell {
                    command: "echo PWNED > /PWNED".to_string(),
                    working_dir: Some("/".to_string()),
                },
                &Grants::default(),
            )
            .await;
        assert!(
            result.is_err(),
            "working_dir=/ must be rejected: {result:?}"
        );
    }

    #[tokio::test]
    async fn write_to_working_dir_is_allowed() {
        require_bwrap();
        let dir = std::env::temp_dir().join(format!("chaz-bwrap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let host = BubblewrapToolHost::with_roots(vec![std::env::temp_dir()])
            .expect("the temp dir is a valid writable root");
        let result = host
            .request(
                &Capability::Shell {
                    command: "echo hello > out.txt".to_string(),
                    working_dir: Some(dir.to_str().unwrap().to_string()),
                },
                &Grants::default(),
            )
            .await
            .expect("confined working dir must be accepted");
        let out = match result {
            CapabilityResult::Shell(o) => o,
            other => panic!("expected shell output, got {other:?}"),
        };
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
    async fn host_pids_cannot_be_signalled_from_inside() {
        require_bwrap();
        // The test process's own PID is a live host PID. Inside the private
        // PID namespace that number addresses nothing, so kill must fail —
        // this is exactly the same-UID kill escape the namespace closes.
        let host_pid = std::process::id();
        let out = run(
            &format!("kill -0 {host_pid} 2>/dev/null && echo SIGNAL_OK || echo SIGNAL_FAIL"),
            None,
        )
        .await;
        assert_eq!(out.exit_code, 0, "probe itself must run cleanly: {out:?}");
        assert!(
            out.stdout.contains("SIGNAL_FAIL"),
            "a host PID was signalled from inside the sandbox: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn proc_is_not_mounted() {
        require_bwrap();
        // Mounting a pid-namespaced procfs is EPERM on some kernels; its
        // absence is deliberate. `test -d /proc` must report it absent
        // rather than exposing the host's process tree.
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

    #[tokio::test]
    async fn environment_does_not_leak_provider_keys() {
        require_bwrap();
        // Any variable exported into the test process (CI secrets, API keys)
        // must not be visible inside the sandbox: only the fixed PATH, HOME
        // and LANG come back after --clearenv.
        let out = run("env | cut -d= -f1 | sort | tr '\\n' ' '", None).await;
        let names: Vec<&str> = out.stdout.split_whitespace().collect();
        for name in names {
            assert!(
                matches!(name, "PATH" | "HOME" | "LANG" | "PWD" | "_" | "SHLVL"),
                "unexpected variable leaked into the sandbox: {name}"
            );
        }
    }
}
