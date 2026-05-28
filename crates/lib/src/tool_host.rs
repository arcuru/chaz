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
    pub exit_code: i32,
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
///   blocking from [`NetworkGrant`]
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
                } => exec_shell(command, working_dir.as_deref(), grants.shell.as_ref()).await,
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

    let output = cmd
        .output()
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to execute command: {e}")))?;

    Ok(CapabilityResult::Shell(ShellOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    }))
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

        // Allowlist if configured (non-empty)
        if !grant.allow.is_empty() && !grant.allow.iter().any(|a| binary.starts_with(a)) {
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

/// Read a file with optional filesystem grant enforcement.
async fn exec_file_read(path: &str, _grants: &Grants) -> Result<CapabilityResult, ToolError> {
    // FsGrant enforcement is a stub — not yet wired in config. The host
    // boundary is here so we can add it without changing tool code.
    let content = tokio::fs::read(path)
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to read file: {e}")))?;
    Ok(CapabilityResult::FileRead(content))
}

/// Write a file with optional filesystem grant enforcement.
async fn exec_file_write(
    path: &str,
    content: &str,
    _grants: &Grants,
) -> Result<CapabilityResult, ToolError> {
    // FsGrant enforcement is a stub — not yet wired in config.
    tokio::fs::write(path, content)
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to write file: {e}")))?;
    Ok(CapabilityResult::FileWrite)
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

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
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

/// Build a `NetworkPolicy` from an optional `NetworkGrant`.
fn build_network_policy(grant: Option<&NetworkGrant>) -> crate::security::NetworkPolicy {
    use crate::security::network::EndpointPattern as PolicyEndpoint;

    let (endpoints, allow_private) = match grant {
        Some(g) => (
            g.endpoints
                .iter()
                .map(|e| PolicyEndpoint {
                    host: e.host.clone(),
                    path_prefix: e.path_prefix.clone(),
                    methods: e.methods.clone(),
                })
                .collect(),
            g.allow_private,
        ),
        None => (Vec::new(), false),
    };

    crate::security::NetworkPolicy::new(endpoints, !allow_private)
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::{EndpointPattern, ShellGrant};

    // ── Shell grant checks ──

    #[test]
    fn test_shell_no_grant_is_permissive() {
        assert!(check_shell_command("rm -rf /", None).is_ok());
    }

    #[test]
    fn test_shell_empty_grant_is_permissive() {
        let g = ShellGrant::default();
        assert!(check_shell_command("git status", Some(&g)).is_ok());
    }

    #[test]
    fn test_shell_allowlist_restricts() {
        let g = ShellGrant {
            allow: vec!["git".into(), "ls".into()],
            deny: vec![],
        };
        assert!(check_shell_command("git status", Some(&g)).is_ok());
        assert!(check_shell_command("ls -la", Some(&g)).is_ok());
        assert!(check_shell_command("rm -rf /", Some(&g)).is_err());
    }

    #[test]
    fn test_shell_denylist_blocks() {
        let g = ShellGrant {
            allow: vec![],
            deny: vec!["rm".into()],
        };
        assert!(check_shell_command("ls", Some(&g)).is_ok());
        assert!(check_shell_command("rm -rf /", Some(&g)).is_err());
    }

    #[test]
    fn test_shell_denylist_beats_allowlist() {
        let g = ShellGrant {
            allow: vec!["rm".into()],
            deny: vec!["rm".into()],
        };
        assert!(check_shell_command("rm foo", Some(&g)).is_err());
    }

    #[test]
    fn test_shell_pipes_check_every_binary() {
        let g = ShellGrant {
            allow: vec!["cat".into()],
            deny: vec![],
        };
        assert!(check_shell_command("cat file.txt", Some(&g)).is_ok());
        assert!(check_shell_command("cat file.txt | grep foo", Some(&g)).is_err());
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
        assert!(p.check("https://example.com/", "GET").is_ok());
        assert!(p.check("http://127.0.0.1/", "GET").is_err());
    }

    #[test]
    fn test_network_grant_endpoint_allowlist() {
        let grant = NetworkGrant {
            endpoints: vec![EndpointPattern {
                host: "api.example.com".into(),
                path_prefix: None,
                methods: Some(vec!["GET".into()]),
            }],
            allow_private: false,
        };
        let p = build_network_policy(Some(&grant));
        assert!(p.check("https://api.example.com/foo", "GET").is_ok());
        assert!(p.check("https://api.example.com/foo", "POST").is_err());
        assert!(p.check("https://evil.com/", "GET").is_err());
    }

    #[test]
    fn test_network_allow_private_opens_internal_hosts() {
        let grant = NetworkGrant {
            endpoints: vec![],
            allow_private: true,
        };
        let p = build_network_policy(Some(&grant));
        assert!(p.check("http://127.0.0.1/", "GET").is_ok());
    }

    // ── NativeToolHost name ──

    #[test]
    fn test_native_host_name() {
        let host = NativeToolHost::new();
        assert_eq!(host.name(), "native");
    }
}
