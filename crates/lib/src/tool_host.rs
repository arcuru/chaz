//! Tool execution host — sandboxed capability boundary for tools.
//!
//! Tools request capabilities through the host rather than accessing OS
//! resources directly. The host enforces policy (grants) and provides
//! the appropriate sandboxing for its trust tier.
//!
//! # Trust tiers
//!
//! | Tier | Host | Isolation | Status |
//! |------|------|-----------|--------|
//! | Native | [`NativeToolHost`] | Grant enforcement only | ✅ |
//! | WASM | (future) | VM-enforced sandbox, capability tokens | ☐ |
//! | Bubblewrap | (future) | OS-level sandboxing via bwrap | ☐ |
//!
//! # Adding a new capability
//!
//! 1. Add a variant to [`Capability`] and a corresponding variant to
//!    [`CapabilityResult`].
//! 2. Implement enforcement + execution in [`NativeToolHost`].
//! 3. Stub the new arm in any future `ToolHost` impls (WASM, bwrap).
//! 4. Update tools that use the new capability.

use crate::grants::{Grants, NetworkGrant, ShellGrant};
use crate::tool::ToolError;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

// ── Capability types ────────────────────────────────────────────

/// A capability that a tool can request from the execution host.
///
/// Each variant maps to one kind of system resource access. The host
/// checks the provided [`Grants`] to decide whether the capability is
/// allowed, then executes it appropriately for its trust tier.
#[derive(Clone, Debug)]
pub enum Capability {
    /// Execute a shell command.
    Shell {
        command: String,
        working_dir: Option<String>,
        /// Stop the command's process group after this long and return what
        /// it printed so far. `None` waits for completion; the caller's own
        /// deadline (if any) still bounds the request.
        timeout: Option<std::time::Duration>,
    },
    /// Read a file's contents as UTF-8 text.
    FileRead { path: String },
    /// Write UTF-8 content to a file (creates or overwrites).
    FileWrite { path: String, content: String },
    /// Make an HTTP request.
    HttpRequest {
        url: String,
        method: String,
        headers: HashMap<String, String>,
        body: Option<String>,
    },
}

/// Output from a shell command executed by the host.
#[derive(Clone, Debug)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    /// Exit code, or 128 + signal number when a signal ended the shell.
    pub exit_code: i32,
    /// The requested timeout expired and the host stopped the process
    /// group; `stdout`/`stderr` hold only what arrived before that.
    pub timed_out: bool,
    /// Wall-clock time from spawn until the output was collected.
    pub elapsed: std::time::Duration,
}

/// Output from an HTTP request executed by the host.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Result of a successful capability request.
///
/// Variants carry capability-specific output data. The tool is responsible
/// for formatting these into LLM-facing strings (truncation, annotation, etc.).
#[derive(Clone, Debug)]
pub enum CapabilityResult {
    Shell(ShellOutput),
    FileRead(Vec<u8>),
    FileWrite,
    HttpResponse(HttpResponse),
}

// ── ToolHost trait ───────────────────────────────────────────────

/// The execution host for tool capabilities.
///
/// Tools request capabilities through the host rather than accessing OS
/// resources directly. This lets the host enforce policy, sandbox execution,
/// and provide different trust tiers.
///
/// # Example
///
/// ```ignore
/// let result = ctx.host().request(
///     &Capability::Shell {
///         command: "ls -la".into(),
///         working_dir: None,
///         timeout: None,
///     },
///     ctx.grants(),
/// ).await?;
/// ```
pub trait ToolHost: Send + Sync {
    /// Request a capability from the host.
    ///
    /// The host checks the provided grants to determine whether the
    /// capability is allowed, then executes it in the host's sandboxed
    /// environment.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] if the capability is denied by policy or
    /// if execution fails (network error, nonzero exit, timeout, etc.).
    fn request<'a>(
        &'a self,
        capability: &'a Capability,
        grants: &'a Grants,
    ) -> Pin<Box<dyn Future<Output = Result<CapabilityResult, ToolError>> + Send + 'a>>;

    /// Human-readable name of this host (for logging and debugging).
    fn name(&self) -> &str;
}

// ── NativeToolHost ───────────────────────────────────────────────

/// In-process tool host with grant-based policy enforcement.
///
/// This is the default host. Capabilities are executed directly in the
/// chaz process, subject to the configured grants:
///
/// - **Shell**: allowlist/denylist from [`ShellGrant`]
/// - **Network**: endpoint patterns, method restrictions, private-IP
///   blocking from [`NetworkGrant`]. Redirects are never followed; a 3xx
///   response is returned as-is with its `Location` header.
/// - **Filesystem**: read/write path restrictions from [`FsGrant`]
///
/// No sandboxing beyond grant checks — the tool has full access to
/// whatever the grants permit. For stronger isolation, use a WASM or
/// bubblewrap host (future).
pub struct NativeToolHost;

impl Default for NativeToolHost {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeToolHost {
    pub fn new() -> Self {
        Self
    }
}

impl ToolHost for NativeToolHost {
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
                    timeout,
                } => {
                    exec_shell(
                        command,
                        working_dir.as_deref(),
                        *timeout,
                        grants.shell.as_ref(),
                    )
                    .await
                }
                Capability::FileRead { path } => exec_file_read(path, grants).await,
                Capability::FileWrite { path, content } => {
                    exec_file_write(path, content, grants).await
                }
                Capability::HttpRequest {
                    url,
                    method,
                    headers,
                    body,
                } => {
                    exec_http(
                        url,
                        method,
                        headers,
                        body.as_deref(),
                        grants.network.as_ref(),
                    )
                    .await
                }
            }
        })
    }

    fn name(&self) -> &str {
        "native"
    }
}

// ── Capability implementations ───────────────────────────────────

/// Execute a shell command with grant enforcement.
async fn exec_shell(
    command: &str,
    working_dir: Option<&str>,
    timeout: Option<std::time::Duration>,
    grant: Option<&ShellGrant>,
) -> Result<CapabilityResult, ToolError> {
    check_shell_command(command, grant).map_err(|msg| {
        tracing::warn!(command = %command, "Shell command denied: {msg}");
        ToolError::Execution(msg)
    })?;

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    run_shell_process(cmd, timeout)
        .await
        .map(CapabilityResult::Shell)
        .map_err(|e| ToolError::Execution(format!("Failed to execute command: {e}")))
}

/// How long to keep draining pipes after the process group was killed. A
/// descendant that escaped the group can hold them open indefinitely.
const KILLED_DRAIN: std::time::Duration = std::time::Duration::from_millis(250);

/// Run a prepared shell process in its own process group and collect its
/// output, shared by every host that spawns a local process.
///
/// With `timeout`, the whole group is killed when it expires and the output
/// received so far is returned with `timed_out` set. Dropping the returned
/// future (user stop, an outer deadline, shutdown) kills the group too.
pub(crate) async fn run_shell_process(
    mut cmd: tokio::process::Command,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<ShellOutput> {
    use tokio::io::AsyncReadExt;

    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let started = std::time::Instant::now();
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut group = ShellProcessGroup(child.id());
    let (Some(mut stdout_pipe), Some(mut stderr_pipe)) = (child.stdout.take(), child.stderr.take())
    else {
        return Err(std::io::Error::other("shell pipes were not captured"));
    };

    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut timed_out = false;
    let status = {
        // Bytes already read stay in the buffers if this is abandoned.
        let collect = async {
            tokio::try_join!(
                stdout_pipe.read_to_end(&mut stdout),
                stderr_pipe.read_to_end(&mut stderr),
                child.wait(),
            )
            .map(|(_, _, status)| status)
        };
        tokio::pin!(collect);
        let finished = match timeout {
            Some(limit) => tokio::time::timeout(limit, &mut collect).await.ok(),
            None => Some((&mut collect).await),
        };
        match finished {
            Some(status) => Some(status?),
            None => {
                timed_out = true;
                group.kill();
                tokio::time::timeout(KILLED_DRAIN, &mut collect)
                    .await
                    .ok()
                    .transpose()?
            }
        }
    };
    let status = match status {
        Some(status) => status,
        // An escaped descendant still holds a pipe: report without it.
        None => child.wait().await?,
    };
    // Reaped: the group id may be reused, so never signal it again.
    group.0 = None;

    Ok(ShellOutput {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code: exit_code(status),
        timed_out,
        elapsed: started.elapsed(),
    })
}

/// Exit code as a shell reports it: 128 + signal for a signalled process.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(-1)
}

/// Dropping a shell wait (user stop, timeout, or shutdown) must also stop
/// descendants. Use the native shell's kill builtin, with the child in its
/// own process group; kill_on_drop reaps the direct child through Tokio.
struct ShellProcessGroup(Option<u32>);

impl ShellProcessGroup {
    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            let result = std::process::Command::new("sh")
                .args([
                    "-c",
                    "kill -KILL -- -\"$1\"",
                    "kill-shell-group",
                    &pid.to_string(),
                ])
                .stderr(std::process::Stdio::null())
                .status();
            if let Err(error) = result {
                tracing::warn!(%error, pid, "Could not kill shell process group");
            }
        }
    }
}

impl Drop for ShellProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Check a shell command against allowlist/denylist from a `ShellGrant`.
/// An empty `allow` list means allow-all. Denylist entries always apply.
/// Check a shell command against allowlist/denylist from a `ShellGrant`.
/// An empty `allow` list means allow-all. Denylist entries always apply.
///
/// Public so other host implementations (e.g., bubblewrap) can enforce
/// the same grant checks before executing via their own mechanism.
pub fn check_shell_command(command: &str, grant: Option<&ShellGrant>) -> Result<(), String> {
    let binaries = extract_command_binaries(command);
    let grant = match grant {
        Some(g) => g,
        None => return Ok(()), // no grant configured → permissive
    };

    for binary in &binaries {
        // Denylist first
        for denied in &grant.deny {
            if binary.starts_with(denied) {
                return Err(format!("Command '{binary}' is denied by security policy"));
            }
        }

        // Allowlist: Permissive = allow-all; Only(list) requires a prefix match
        // (an empty list matches nothing → deny-all).
        if let crate::grants::Allowlist::Only(list) = &grant.allow
            && !list.iter().any(|a| binary.starts_with(a))
        {
            return Err(format!(
                "Command '{binary}' is not in the allowed commands list"
            ));
        }
    }

    Ok(())
}

/// Extract individual command binaries from a shell command string.
/// Splits on pipes, `&&`, `||`, `;`, and `$()` subshells.
fn extract_command_binaries(command: &str) -> Vec<String> {
    let mut binaries = Vec::new();
    for segment in command.split(['|', '&', ';']) {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(first_word) = trimmed.split_whitespace().next() {
            let clean = first_word.trim_start_matches("$(").trim_start_matches('(');
            if !clean.is_empty() {
                binaries.push(clean.to_string());
            }
        }
    }
    binaries
}

/// Read a file, enforcing the `FsGrant.allow_read` bound if one is configured.
async fn exec_file_read(path: &str, grants: &Grants) -> Result<CapabilityResult, ToolError> {
    check_fs_path(path, grants.fs.as_ref().map(|f| &f.allow_read), "read")?;
    let content = tokio::fs::read(path)
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to read file: {e}")))?;
    Ok(CapabilityResult::FileRead(content))
}

/// Write a file, enforcing the `FsGrant.allow_write` bound if one is configured.
async fn exec_file_write(
    path: &str,
    content: &str,
    grants: &Grants,
) -> Result<CapabilityResult, ToolError> {
    check_fs_path(path, grants.fs.as_ref().map(|f| &f.allow_write), "write")?;
    tokio::fs::write(path, content)
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to write file: {e}")))?;
    Ok(CapabilityResult::FileWrite)
}

/// Check a filesystem path against an allowlist of permitted roots.
///
/// `None` (no fs grant) or `Permissive` means allow-all; `Only(roots)` requires
/// the path to lexically resolve to a location at or under one of the roots —
/// an empty `Only([])` therefore denies everything.
///
/// This is **advisory** confinement: paths are normalized lexically (resolving
/// `.`/`..`) but symlinks are not followed, so a symlink under an allowed root
/// can still point outside it. True confinement is the sandboxed host's job
/// (mounts / WASI preopens); this is the cheap first pass.
pub fn check_fs_path(
    path: &str,
    allow: Option<&crate::grants::Allowlist<String>>,
    op: &str,
) -> Result<(), String> {
    let roots = match allow {
        None | Some(crate::grants::Allowlist::Permissive) => return Ok(()),
        Some(crate::grants::Allowlist::Only(roots)) => roots,
    };
    let target = normalize_lexical(std::path::Path::new(path));
    for root in roots {
        let root_norm = normalize_lexical(std::path::Path::new(root));
        if target == root_norm || target.starts_with(&root_norm) {
            return Ok(());
        }
    }
    Err(format!(
        "Filesystem {op} of '{path}' is outside the allowed paths"
    ))
}

/// Lexically normalize a path: drop `.`, resolve `..` against prior components,
/// without touching the filesystem. Component-wise so `starts_with` is a true
/// path-boundary check (`/work` does not match `/workother`).
fn normalize_lexical(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a normal segment; keep `..` only when it can't be resolved
                // (leading `..` on a relative path).
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Execute an HTTP request with network grant enforcement.
async fn exec_http(
    url: &str,
    method: &str,
    headers: &HashMap<String, String>,
    body: Option<&str>,
    grant: Option<&NetworkGrant>,
) -> Result<CapabilityResult, ToolError> {
    // Build a NetworkPolicy from the grant and check the URL+method
    let policy = build_network_policy(grant);
    policy.check(url, method).map_err(ToolError::Execution)?;

    tracing::info!(%method, %url, "HTTP request via NativeToolHost");

    // Never follow redirects: the grant was checked against `url` only, and a
    // 3xx `Location` may name a path, host, or private address the grant does
    // not cover. The 3xx status and its `Location` header are returned to the
    // caller; a follow-up request is a new capability request and passes the
    // grant check like any other.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ToolError::Execution(format!("Failed to create HTTP client: {e}")))?;

    let mut req = match method.to_uppercase().as_str() {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        _ => client.get(url),
    };

    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }

    if let Some(b) = body {
        req = req.body(b.to_string());
    }

    let response = req
        .send()
        .await
        .map_err(|e| ToolError::Network(format!("HTTP request failed: {e}")))?;

    let status = response.status().as_u16();
    let resp_headers: HashMap<String, String> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let resp_body = response
        .bytes()
        .await
        .map_err(|e| ToolError::Network(format!("Failed to read response body: {e}")))?;

    tracing::debug!(status, body_len = resp_body.len(), %url, "HTTP response received");

    Ok(CapabilityResult::HttpResponse(HttpResponse {
        status,
        headers: resp_headers,
        body: resp_body.to_vec(),
    }))
}

/// Build a `NetworkPolicy` from an optional `NetworkGrant`. `None` (no grant)
/// and `Permissive` endpoints both mean allow-all (public); `Only([])` means
/// deny-all (no egress).
fn build_network_policy(grant: Option<&NetworkGrant>) -> crate::security::NetworkPolicy {
    use crate::grants::Allowlist;
    use crate::security::network::EndpointPattern as PolicyEndpoint;

    let convert = |list: &[crate::grants::EndpointPattern]| -> Vec<PolicyEndpoint> {
        list.iter()
            .map(|e| PolicyEndpoint {
                host: e.host.clone(),
                path_prefix: e.path_prefix.clone(),
                methods: e.methods.clone(),
            })
            .collect()
    };

    let (endpoints, allow_private) = match grant {
        None => (Allowlist::Permissive, false),
        Some(g) => {
            let eps = match &g.endpoints {
                Allowlist::Permissive => Allowlist::Permissive,
                Allowlist::Only(list) => Allowlist::Only(convert(list)),
            };
            (eps, g.allow_private)
        }
    };

    crate::security::NetworkPolicy::new(endpoints, !allow_private)
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::{Allowlist, EndpointPattern, ShellGrant};

    fn only(roots: &[&str]) -> Allowlist<String> {
        Allowlist::Only(roots.iter().map(|s| s.to_string()).collect())
    }

    // ── Filesystem grant checks ──

    #[test]
    fn test_fs_no_allowlist_is_permissive() {
        check_fs_path("/etc/passwd", None, "read")
            .expect("path should be permitted by the filesystem grant");
        check_fs_path("/etc/passwd", Some(&Allowlist::Permissive), "read")
            .expect("path should be permitted by the filesystem grant");
    }

    #[test]
    fn test_fs_empty_only_is_deny_all() {
        // The new capability: an explicit empty allowlist denies everything.
        let deny_all = Allowlist::Only(vec![]);
        assert!(check_fs_path("/etc/passwd", Some(&deny_all), "read").is_err());
        assert!(check_fs_path("/work/x", Some(&deny_all), "write").is_err());
    }

    #[test]
    fn test_fs_within_allowed_root_ok() {
        let allow = only(&["/work"]);
        check_fs_path("/work/file.txt", Some(&allow), "write")
            .expect("path should be permitted by the filesystem grant");
        check_fs_path("/work/sub/dir/x", Some(&allow), "write")
            .expect("path should be permitted by the filesystem grant");
        check_fs_path("/work", Some(&allow), "write")
            .expect("path should be permitted by the filesystem grant");
    }

    #[test]
    fn test_fs_outside_allowed_root_denied() {
        let allow = only(&["/work"]);
        assert!(check_fs_path("/etc/passwd", Some(&allow), "read").is_err());
        // Prefix-but-not-path-boundary must not match.
        assert!(check_fs_path("/workother/x", Some(&allow), "read").is_err());
    }

    #[test]
    fn test_fs_parent_traversal_resolved() {
        let allow = only(&["/work"]);
        // `..` escapes the allowed root → denied.
        assert!(check_fs_path("/work/../etc/passwd", Some(&allow), "read").is_err());
        // `..` that stays within is fine.
        check_fs_path("/work/sub/../file", Some(&allow), "write")
            .expect("path should be permitted by the filesystem grant");
    }

    // ── Shell process lifecycle ──

    #[cfg(unix)]
    fn sh(command: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_shell_reports_output_status_and_elapsed() {
        let out = run_shell_process(
            sh("echo out; echo err >&2; sleep 0.2; exit 4"),
            Some(std::time::Duration::from_secs(10)),
        )
        .await
        .unwrap();
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str()),
            ("out\n", "err\n")
        );
        assert_eq!(out.exit_code, 4);
        assert!(!out.timed_out);
        assert!(out.elapsed >= std::time::Duration::from_millis(200));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_signalled_shell_reports_128_plus_signal() {
        let out = run_shell_process(sh("kill -TERM $$"), None).await.unwrap();
        assert_eq!(out.exit_code, 128 + 15);
        assert!(!out.timed_out);
    }

    /// The timeout kills the group, including a backgrounded descendant and
    /// a shell ignoring TERM, and keeps the output that arrived first.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_kills_the_group_and_keeps_partial_output() {
        let directory = tempfile::tempdir().unwrap();
        let leaked = directory.path().join("leaked");
        let command = format!(
            "trap '' TERM; echo before; echo warn >&2; (sleep 1; echo leaked > '{}') & sleep 30; echo after",
            leaked.display()
        );
        let started = std::time::Instant::now();
        let out = run_shell_process(sh(&command), Some(std::time::Duration::from_millis(300)))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str()),
            ("before\n", "warn\n")
        );
        assert!(out.elapsed >= std::time::Duration::from_millis(300));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(!leaked.exists(), "a timed-out descendant survived");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_host_forwards_the_shell_timeout() {
        let result = NativeToolHost
            .request(
                &Capability::Shell {
                    command: "echo hi; sleep 30".into(),
                    working_dir: None,
                    timeout: Some(std::time::Duration::from_millis(200)),
                },
                &Grants::default(),
            )
            .await
            .unwrap();
        let CapabilityResult::Shell(out) = result else {
            panic!("expected shell output");
        };
        assert!(out.timed_out);
        assert_eq!(out.stdout, "hi\n");
    }

    // ── Shell grant checks ──

    #[test]
    fn test_shell_no_grant_is_permissive() {
        check_shell_command("rm -rf /", None)
            .expect("command should be permitted by the shell grant");
    }

    #[test]
    fn test_shell_empty_grant_is_permissive() {
        let g = ShellGrant::default();
        check_shell_command("git status", Some(&g))
            .expect("command should be permitted by the shell grant");
    }

    #[test]
    fn test_shell_allowlist_restricts() {
        let g = ShellGrant {
            allow: only(&["git", "ls"]),
            deny: vec![],
        };
        check_shell_command("git status", Some(&g))
            .expect("command should be permitted by the shell grant");
        check_shell_command("ls -la", Some(&g))
            .expect("command should be permitted by the shell grant");
        assert_eq!(
            check_shell_command("rm -rf /", Some(&g)),
            Err("Command 'rm' is not in the allowed commands list".into())
        );
    }

    #[test]
    fn test_shell_empty_allowlist_is_deny_all() {
        // The new capability: an explicit empty allowlist denies everything.
        let g = ShellGrant {
            allow: Allowlist::Only(vec![]),
            deny: vec![],
        };
        assert_eq!(
            check_shell_command("ls", Some(&g)),
            Err("Command 'ls' is not in the allowed commands list".into())
        );
        assert_eq!(
            check_shell_command("git status", Some(&g)),
            Err("Command 'git' is not in the allowed commands list".into())
        );
    }

    #[test]
    fn test_shell_denylist_blocks() {
        let g = ShellGrant {
            allow: Allowlist::Permissive,
            deny: vec!["rm".into()],
        };
        check_shell_command("ls", Some(&g))
            .expect("command should be permitted by the shell grant");
        assert_eq!(
            check_shell_command("rm -rf /", Some(&g)),
            Err("Command 'rm' is denied by security policy".into())
        );
    }

    #[test]
    fn test_shell_denylist_beats_allowlist() {
        let g = ShellGrant {
            allow: only(&["rm"]),
            deny: vec!["rm".into()],
        };
        assert_eq!(
            check_shell_command("rm foo", Some(&g)),
            Err("Command 'rm' is denied by security policy".into())
        );
    }

    #[test]
    fn test_shell_pipes_check_every_binary() {
        let g = ShellGrant {
            allow: only(&["cat"]),
            deny: vec![],
        };
        check_shell_command("cat file.txt", Some(&g))
            .expect("command should be permitted by the shell grant");
        assert_eq!(
            check_shell_command("cat file.txt | grep foo", Some(&g)),
            Err("Command 'grep' is not in the allowed commands list".into())
        );
    }

    // ── extract_command_binaries ──

    #[test]
    fn test_extract_simple_command() {
        let bins = extract_command_binaries("git status");
        assert_eq!(bins, vec!["git"]);
    }

    #[test]
    fn test_extract_piped_commands() {
        let bins = extract_command_binaries("cat file | grep foo | wc -l");
        assert_eq!(bins, vec!["cat", "grep", "wc"]);
    }

    #[test]
    fn test_extract_logical_operators() {
        let bins = extract_command_binaries("make build && cargo test");
        assert_eq!(bins, vec!["make", "cargo"]);
    }

    #[test]
    fn test_extract_semicolon() {
        let bins = extract_command_binaries("cd /tmp; ls");
        assert_eq!(bins, vec!["cd", "ls"]);
    }

    #[test]
    fn test_extract_subshell_strips_prefix() {
        // "$(whoami)" as the command: trim_start_matches("$(") strips the prefix,
        // leaving "whoami)" (the ")" is not trimmed — known edge case from the
        // original shell.rs implementation, preserved here).
        let bins = extract_command_binaries("$(whoami)");
        assert_eq!(bins, vec!["whoami)"]); // $( stripped, ) remains
    }

    // ── Network policy (ported from web.rs tests) ──

    #[test]
    fn test_network_no_grant_blocks_private_ips_allows_public() {
        let p = build_network_policy(None);
        p.check("https://example.com/", "GET")
            .expect("endpoint should be permitted by the network grant");
        assert_eq!(
            p.check("http://127.0.0.1/", "GET"),
            Err("SSRF protection: private IP 127.0.0.1 not allowed".into())
        );
    }

    #[test]
    fn test_network_grant_endpoint_allowlist() {
        let grant = NetworkGrant {
            endpoints: Allowlist::Only(vec![EndpointPattern {
                host: "api.example.com".into(),
                path_prefix: None,
                methods: Some(vec!["GET".into()]),
            }]),
            allow_private: false,
        };
        let p = build_network_policy(Some(&grant));
        p.check("https://api.example.com/foo", "GET")
            .expect("endpoint should be permitted by the network grant");
        assert_eq!(
            p.check("https://api.example.com/foo", "POST"),
            Err(
                "Network policy denied: POST https://api.example.com/foo not in allowed endpoints"
                    .into()
            )
        );
        assert_eq!(
            p.check("https://evil.com/", "GET"),
            Err("Network policy denied: GET https://evil.com/ not in allowed endpoints".into())
        );
    }

    #[test]
    fn test_network_empty_only_is_deny_all() {
        // The new capability: an empty endpoint allowlist denies all egress.
        let grant = NetworkGrant {
            endpoints: Allowlist::Only(vec![]),
            allow_private: false,
        };
        let p = build_network_policy(Some(&grant));
        assert_eq!(
            p.check("https://example.com/", "GET"),
            Err("Network policy denied: GET https://example.com/ not in allowed endpoints".into())
        );
    }

    #[test]
    fn test_network_allow_private_opens_internal_hosts() {
        let grant = NetworkGrant {
            endpoints: Allowlist::Permissive,
            allow_private: true,
        };
        let p = build_network_policy(Some(&grant));
        p.check("http://127.0.0.1/", "GET")
            .expect("endpoint should be permitted by the network grant");
    }

    // ── Redirect containment (live loopback listeners) ──

    type Requests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// Disposable HTTP/1.1 listener on 127.0.0.1. Records every request line
    /// it receives and answers each with `respond(path)` (status, extra headers).
    async fn listener(
        respond: impl Fn(&str) -> (u16, Vec<(&'static str, String)>) + Send + Sync + 'static,
    ) -> (std::net::SocketAddr, Requests) {
        let respond = std::sync::Arc::new(respond);
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let sock = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let seen: Requests = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = sock.accept().await {
                let log = log.clone();
                let respond = respond.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match conn.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let line = head.lines().next().unwrap_or("").to_string();
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    log.lock().unwrap().push(line);
                    let (status, headers) = respond(&path);
                    let mut resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 4\r\nConnection: close\r\n"
                    );
                    for (k, v) in headers {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str("\r\nbody");
                    let _ = conn.write_all(resp.as_bytes()).await;
                });
            }
        });
        (addr, seen)
    }

    fn loopback_grant(path_prefix: &str) -> Grants {
        Grants {
            network: Some(NetworkGrant {
                endpoints: Allowlist::Only(vec![EndpointPattern {
                    host: "127.0.0.1".into(),
                    path_prefix: Some(path_prefix.into()),
                    methods: None,
                }]),
                allow_private: true,
            }),
            ..Grants::default()
        }
    }

    async fn get(url: String, grants: &Grants) -> Result<CapabilityResult, ToolError> {
        NativeToolHost::new()
            .request(
                &Capability::HttpRequest {
                    url,
                    method: "GET".into(),
                    headers: HashMap::new(),
                    body: None,
                },
                grants,
            )
            .await
    }

    fn http(result: Result<CapabilityResult, ToolError>) -> HttpResponse {
        match result {
            Ok(CapabilityResult::HttpResponse(r)) => r,
            other => panic!("expected HTTP response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_http_authorized_request_returns_response() {
        let (addr, seen) = listener(|_| (200, vec![])).await;
        let r = http(get(format!("http://{addr}/ok/a"), &loopback_grant("/ok")).await);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"body");
        assert_eq!(*seen.lock().unwrap(), vec!["GET /ok/a HTTP/1.1"]);
    }

    #[tokio::test]
    async fn test_http_direct_denial_sends_nothing() {
        let (addr, seen) = listener(|_| (200, vec![])).await;
        let err = get(format!("http://{addr}/secret"), &loopback_grant("/ok"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Network policy denied"), "{err}");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_http_same_host_redirect_outside_path_is_not_followed() {
        let (addr, seen) = listener(|path| match path {
            "/ok" => (302, vec![("Location", "/secret".to_string())]),
            _ => (200, vec![]),
        })
        .await;
        let r = http(get(format!("http://{addr}/ok"), &loopback_grant("/ok")).await);
        assert_eq!(r.status, 302);
        assert_eq!(
            r.headers.get("location").map(String::as_str),
            Some("/secret")
        );
        // Give a (buggy) follow-up request time to land before asserting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(*seen.lock().unwrap(), vec!["GET /ok HTTP/1.1"]);
    }

    #[tokio::test]
    async fn test_http_cross_host_private_redirect_is_not_followed() {
        // Forbidden target: a different host (`localhost`) that is outside the
        // endpoint allowlist and is an internal hostname.
        let (target, target_seen) = listener(|_| (200, vec![])).await;
        let forbidden = format!("http://localhost:{}/admin", target.port());
        let err = get(forbidden.clone(), &loopback_grant("/"))
            .await
            .expect_err("localhost is outside the loopback IP allowlist");
        assert!(
            matches!(err, ToolError::Execution(ref message) if message == &format!(
                "Network policy denied: GET {forbidden} not in allowed endpoints"
            )),
            "expected endpoint denial, got {err:?}"
        );

        let location = forbidden.clone();
        let (addr, seen) = listener(move |_| (301, vec![("Location", location.clone())])).await;
        let r = http(get(format!("http://{addr}/go"), &loopback_grant("/")).await);
        assert_eq!(r.status, 301);
        assert_eq!(r.headers.get("location"), Some(&forbidden));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(*seen.lock().unwrap(), vec!["GET /go HTTP/1.1"]);
        assert!(target_seen.lock().unwrap().is_empty());
    }

    // ── NativeToolHost name ──

    #[test]
    fn test_native_host_name() {
        let host = NativeToolHost::new();
        assert_eq!(host.name(), "native");
    }
}
