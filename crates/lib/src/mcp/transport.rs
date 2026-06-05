//! Transport layer for MCP. Two concrete variants — stdio subprocess and
//! Streamable HTTP — each owning its own I/O state and read/write logic.
//! `McpServer` treats them uniformly via the `Transport` enum facade.

use crate::config::McpServerConfig;
use crate::security::SecretStore;

use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::parse::{parse_jsonrpc_response, parse_sse_body};

/// Maximum restart attempts before giving up (stdio transport).
const MAX_RESTART_ATTEMPTS: u8 = 5;

/// Base backoff delay in milliseconds (doubles each attempt: 1s, 2s, 4s, 8s, 16s).
const BASE_BACKOFF_MS: u64 = 1000;

/// Transport for communicating with an MCP server.
///
/// Two variants, each wrapping its own I/O state. All request/notification
/// handling lives on the variant types (`StdioTransport`, `HttpTransport`)
/// so `McpServer` stays focused on protocol + tool logic.
pub(super) enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

/// JSON-RPC over subprocess stdin/stdout.
pub(super) struct StdioTransport {
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    _child: Box<Mutex<Child>>,
    config: Box<McpServerConfig>,
    restart_attempts: AtomicU8,
}

/// Streamable HTTP transport (POST requests, JSON or SSE responses).
pub(super) struct HttpTransport {
    url: String,
    client: reqwest::Client,
    /// MCP session ID returned by the server (tracked across requests).
    /// Exposed at `super` visibility so integration tests can seed it
    /// without bouncing through a real initialize handshake — there's
    /// no other lever for "pretend we have a session" against a fake.
    pub(super) session_id: Mutex<Option<String>>,
    /// Protocol version negotiated during the initialize handshake.
    /// `None` before initialize completes; `Some(version)` after, at
    /// which point every subsequent request carries an
    /// `MCP-Protocol-Version` header per the Streamable HTTP spec.
    protocol_version: Mutex<Option<String>>,
}

impl Transport {
    /// Create a stdio transport by spawning a subprocess.
    pub(super) fn new_stdio(config: &McpServerConfig) -> Result<Self, String> {
        Ok(Transport::Stdio(StdioTransport::spawn(config)?))
    }

    /// Create an HTTP transport targeting the given URL.
    pub(super) fn new_http(url: &str) -> Self {
        Transport::Http(HttpTransport {
            url: url.to_string(),
            client: reqwest::Client::new(),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
        })
    }

    /// Store the negotiated MCP protocol version. Called once per session
    /// after the `initialize` handshake completes. No-op for stdio.
    pub(super) async fn set_protocol_version(&self, version: &str) {
        if let Transport::Http(h) = self {
            *h.protocol_version.lock().await = Some(version.to_string());
        }
    }

    /// Dispatch a JSON-RPC request to the concrete transport.
    pub(super) async fn send_request(
        &self,
        server_name: &str,
        request: &Value,
        id: u64,
        tools_changed: &AtomicBool,
    ) -> Result<Value, String> {
        match self {
            Transport::Stdio(s) => {
                s.send_request(server_name, request, id, tools_changed)
                    .await
            }
            Transport::Http(h) => h.send_request(server_name, request, tools_changed).await,
        }
    }

    /// Dispatch a JSON-RPC notification (no response expected) to the transport.
    pub(super) async fn send_notification(
        &self,
        server_name: &str,
        notification: &Value,
    ) -> Result<(), String> {
        match self {
            Transport::Stdio(s) => s.send_notification(server_name, notification).await,
            Transport::Http(h) => h.send_notification(server_name, notification).await,
        }
    }

    /// Check if an error indicates the subprocess has died (stdio only).
    pub(super) fn is_process_dead_error(&self, error: &str) -> bool {
        matches!(self, Transport::Stdio(_))
            && (error.contains("closed stdout")
                || error.contains("write error")
                || error.contains("read error")
                || error.contains("Broken pipe"))
    }

    /// Check if an error indicates the MCP session has been terminated by
    /// the server (HTTP only). When this fires the transport has already
    /// dropped its cached session ID; the caller re-initializes and
    /// retries.
    pub(super) fn is_session_expired_error(&self, error: &str) -> bool {
        matches!(self, Transport::Http(_)) && error.contains("session expired (HTTP 404)")
    }

    /// Attempt to restart the subprocess (stdio only). No-op for HTTP.
    pub(super) async fn restart(&self, name: &str) -> Result<(), String> {
        match self {
            Transport::Stdio(s) => s.restart(name).await,
            Transport::Http(_) => Ok(()),
        }
    }

    /// Reset restart counter (stdio only). Called on successful request.
    pub(super) fn reset_restart_counter(&self) {
        if let Transport::Stdio(s) = self {
            s.restart_attempts.store(0, Ordering::Relaxed);
        }
    }
}

impl StdioTransport {
    /// Spawn a subprocess for stdio transport.
    fn spawn(config: &McpServerConfig) -> Result<Self, String> {
        let (child, stdin, stdout) = Self::spawn_process(config)?;
        Ok(StdioTransport {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            _child: Box::new(Mutex::new(child)),
            config: Box::new(config.clone()),
            restart_attempts: AtomicU8::new(0),
        })
    }

    fn spawn_process(config: &McpServerConfig) -> Result<(Child, ChildStdin, ChildStdout), String> {
        let mut cmd = tokio::process::Command::new(&config.command);
        if let Some(args) = &config.args {
            let resolved: Vec<String> = args
                .iter()
                .map(|a| SecretStore::resolve_env(a).unwrap_or_else(|_| a.clone()))
                .collect();
            cmd.args(&resolved);
        }
        if let Some(env) = &config.env {
            for (k, v) in env {
                let resolved = SecretStore::resolve_env(v).unwrap_or_else(|_| v.clone());
                cmd.env(k, resolved);
            }
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{}': {e}", config.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("No stdin for MCP server '{}'", config.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("No stdout for MCP server '{}'", config.name))?;

        Ok((child, stdin, stdout))
    }

    async fn restart(&self, name: &str) -> Result<(), String> {
        let attempt = self.restart_attempts.fetch_add(1, Ordering::Relaxed);
        if attempt >= MAX_RESTART_ATTEMPTS {
            return Err(format!(
                "MCP server '{name}' exceeded max restart attempts ({MAX_RESTART_ATTEMPTS})"
            ));
        }

        let backoff_ms = BASE_BACKOFF_MS * (1u64 << attempt.min(5));
        warn!(
            "MCP server '{name}' restarting (attempt {}/{MAX_RESTART_ATTEMPTS}, backoff {backoff_ms}ms)",
            attempt + 1
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;

        let (child, new_stdin, new_stdout) = Self::spawn_process(&self.config)?;
        *self._child.lock().await = child;
        *self.stdin.lock().await = new_stdin;
        *self.stdout.lock().await = BufReader::new(new_stdout);

        Ok(())
    }

    /// Write a JSON-RPC message to stdin with trailing newline + flush.
    async fn write_frame(&self, server_name: &str, payload: &Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let payload_str = serde_json::to_string(payload).map_err(|e| e.to_string())?;
        debug!("MCP '{server_name}' → {payload_str}");
        stdin
            .write_all(payload_str.as_bytes())
            .await
            .map_err(|e| format!("MCP '{server_name}' write error: {e}"))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("MCP '{server_name}' write error: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("MCP '{server_name}' flush error: {e}"))?;
        Ok(())
    }

    async fn send_request(
        &self,
        server_name: &str,
        request: &Value,
        id: u64,
        tools_changed: &AtomicBool,
    ) -> Result<Value, String> {
        self.write_frame(server_name, request).await?;

        let mut stdout = self.stdout.lock().await;
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = stdout
                .read_line(&mut line)
                .await
                .map_err(|e| format!("MCP '{server_name}' read error: {e}"))?;

            if bytes_read == 0 {
                return Err(format!("MCP server '{server_name}' closed stdout"));
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let parsed: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    warn!("MCP '{server_name}' non-JSON line: {e}");
                    continue;
                }
            };

            debug!("MCP '{server_name}' ← {trimmed}");

            // Handle notifications (no id field)
            let resp_id = match parsed.get("id") {
                Some(id_val) => id_val.as_u64(),
                None => {
                    let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    if method == "notifications/tools/list_changed" {
                        info!("MCP '{server_name}' signaled tools/list_changed");
                        tools_changed.store(true, Ordering::Relaxed);
                    } else {
                        debug!("MCP '{server_name}' notification: {trimmed}");
                    }
                    continue;
                }
            };

            if resp_id == Some(id) {
                if let Some(err) = parsed.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error");
                    return Err(format!("MCP '{server_name}' error: {msg}"));
                }
                return parsed
                    .get("result")
                    .cloned()
                    .ok_or_else(|| format!("MCP '{server_name}': response missing result"));
            }

            warn!("MCP '{server_name}' unexpected response id {resp_id:?} (expected {id})");
        }
    }

    async fn send_notification(
        &self,
        server_name: &str,
        notification: &Value,
    ) -> Result<(), String> {
        self.write_frame(server_name, notification).await
    }
}

impl HttpTransport {
    /// Build a POST request carrying a JSON-RPC payload + the optional
    /// session header + the negotiated `MCP-Protocol-Version` (post-init).
    ///
    /// The protocol version header is required on every request after
    /// the initialize handshake — see Streamable HTTP §Protocol Version
    /// Header. Before initialize completes the field is None and the
    /// header is omitted; older servers fall back to assuming
    /// `2025-03-26` per spec, which is harmless.
    async fn build_post(&self, payload_str: String) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(payload_str);

        if let Some(sid) = self.session_id.lock().await.as_ref() {
            req = req.header("Mcp-Session-Id", sid);
        }
        if let Some(ver) = self.protocol_version.lock().await.as_ref() {
            req = req.header("MCP-Protocol-Version", ver);
        }
        req
    }

    async fn send_request(
        &self,
        server_name: &str,
        request: &Value,
        tools_changed: &AtomicBool,
    ) -> Result<Value, String> {
        let request_str = serde_json::to_string(request).map_err(|e| e.to_string())?;
        debug!("MCP '{server_name}' → {request_str}");

        let resp = self
            .build_post(request_str)
            .await
            .send()
            .await
            .map_err(|e| format!("MCP '{server_name}' HTTP error: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            // 404 on a request that carried a session ID means the
            // server has terminated the session (spec §Session
            // Management). Drop the stale session/version so the
            // McpServer-level retry can re-initialize cleanly.
            if status == reqwest::StatusCode::NOT_FOUND {
                let had_session = self.session_id.lock().await.is_some();
                if had_session {
                    *self.session_id.lock().await = None;
                    *self.protocol_version.lock().await = None;
                    return Err(format!("MCP '{server_name}' session expired (HTTP 404)"));
                }
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("MCP '{server_name}' HTTP {status}: {body}"));
        }

        // Track session ID from response headers
        if let Some(sid) = resp.headers().get("mcp-session-id")
            && let Ok(sid_str) = sid.to_str()
        {
            *self.session_id.lock().await = Some(sid_str.to_string());
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = resp
            .text()
            .await
            .map_err(|e| format!("MCP '{server_name}' HTTP body error: {e}"))?;

        if content_type.contains("text/event-stream") {
            parse_sse_body(server_name, &body, tools_changed)
        } else {
            debug!("MCP '{server_name}' ← {body}");
            parse_jsonrpc_response(server_name, &body)
        }
    }

    async fn send_notification(
        &self,
        server_name: &str,
        notification: &Value,
    ) -> Result<(), String> {
        let notif_str = serde_json::to_string(notification).map_err(|e| e.to_string())?;
        debug!("MCP '{server_name}' → {notif_str}");

        let resp = self
            .build_post(notif_str)
            .await
            .send()
            .await
            .map_err(|e| format!("MCP '{server_name}' HTTP error: {e}"))?;

        if !resp.status().is_success() {
            warn!(
                "MCP '{server_name}' notification HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_transport_never_detects_process_death() {
        let transport = Transport::new_http("http://example.com");
        assert!(!transport.is_process_dead_error("closed stdout"));
        assert!(!transport.is_process_dead_error("write error"));
        assert!(!transport.is_process_dead_error("Broken pipe"));
    }
}
