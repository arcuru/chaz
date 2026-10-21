//! Transport layer for MCP. Each stdio transport retains its complete I/O
//! generation until explicit reap, including when close or restart is cancelled.

use crate::config::McpServerConfig;
use crate::security::SecretStore;

use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, Instant, timeout_at};
use tracing::{debug, info, warn};

use super::parse::{parse_jsonrpc_response, parse_sse_body};

const MAX_RESTART_ATTEMPTS: u8 = 5;
const BASE_BACKOFF_MS: u64 = 1000;
const CLOSE_PHASE: Duration = Duration::from_secs(2);

/// One monotonic per-child budget, including startup joins and lock acquisition.
#[derive(Clone, Copy)]
pub(crate) struct CloseBudget {
    eof: Instant,
    #[cfg(unix)]
    term: Instant,
    pub(crate) end: Instant,
}

impl CloseBudget {
    pub(crate) fn new() -> Self {
        let start = Instant::now();
        Self {
            eof: start + CLOSE_PHASE,
            #[cfg(unix)]
            term: start + CLOSE_PHASE * 2,
            end: start + CLOSE_PHASE * if cfg!(unix) { 3 } else { 2 },
        }
    }
}

pub(super) enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

/// Child and pipes are installed together, without an await at publication.
struct StdioGeneration {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

pub(super) struct StdioTransport {
    generation: Box<Mutex<Option<StdioGeneration>>>,
    /// Serializes the synchronous spawn/publication boundary with terminal close.
    closed: std::sync::Mutex<bool>,
    /// Interrupt readers/writers *before* close/restart waits for their lock.
    interrupt: watch::Sender<u64>,
    restarting: Mutex<()>,
    generation_id: AtomicU64,
    config: Box<McpServerConfig>,
    restart_attempts: AtomicU8,
    #[cfg(test)]
    failure: AtomicU8,
    #[cfg(test)]
    exit_success: AtomicBool,
}

/// Cancellation during replacement is terminal, never a half-installed generation.
struct RestartGuard<'a> {
    transport: &'a StdioTransport,
    complete: bool,
}

impl Drop for RestartGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.transport.begin_close();
        }
    }
}

pub(super) struct HttpTransport {
    url: String,
    client: reqwest::Client,
    pub(super) session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
}

impl Transport {
    pub(super) fn new_stdio(config: &McpServerConfig) -> Result<Self, String> {
        Ok(Transport::Stdio(StdioTransport::spawn(config)?))
    }

    pub(super) fn new_http(url: &str) -> Self {
        Transport::Http(HttpTransport {
            url: url.to_string(),
            client: reqwest::Client::new(),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
        })
    }

    pub(super) async fn set_protocol_version(&self, version: &str) {
        if let Transport::Http(h) = self {
            *h.protocol_version.lock().await = Some(version.to_string());
        }
    }

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

    pub(super) fn is_process_dead_error(&self, error: &str) -> bool {
        matches!(self, Transport::Stdio(_))
            && (error.contains("closed stdout")
                || error.contains("write error")
                || error.contains("read error")
                || error.contains("Broken pipe"))
    }

    pub(super) fn is_session_expired_error(&self, error: &str) -> bool {
        matches!(self, Transport::Http(_)) && error.contains("session expired (HTTP 404)")
    }

    pub(super) async fn restart(&self, name: &str) -> Result<(), String> {
        match self {
            Transport::Stdio(s) => s.restart(name).await,
            Transport::Http(_) => Ok(()),
        }
    }

    pub(super) fn reset_restart_counter(&self) {
        if let Transport::Stdio(s) = self {
            s.restart_attempts.store(0, Ordering::Relaxed);
        }
    }

    pub(super) fn begin_close(&self) {
        if let Transport::Stdio(s) = self {
            s.begin_close();
        }
    }

    /// A bounded return is not proof of cleanup. On error the generation stays
    /// owned here, closed to requests/replacement, for a subsequent close attempt.
    pub(super) async fn shutdown(&self, budget: CloseBudget) -> Result<(), String> {
        if let Transport::Stdio(s) = self {
            s.begin_close();
            let mut generation = timeout_at(budget.end, s.generation.lock())
                .await
                .map_err(|_| "incomplete MCP cleanup: I/O generation lock timed out".to_string())?;
            s.reap(&mut generation, budget).await?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn inject_cleanup_failure(&self, failure: u8) {
        if let Transport::Stdio(s) = self {
            s.failure.store(failure, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub(super) async fn process_id(&self) -> Option<u32> {
        match self {
            Transport::Stdio(s) => s
                .generation
                .lock()
                .await
                .as_ref()
                .and_then(|g| g.child.id()),
            Transport::Http(_) => None,
        }
    }
}

impl StdioTransport {
    fn spawn(config: &McpServerConfig) -> Result<Self, String> {
        Ok(Self {
            generation: Box::new(Mutex::new(Some(Self::spawn_process(config)?))),
            closed: std::sync::Mutex::new(false),
            interrupt: watch::channel(0).0,
            restarting: Mutex::new(()),
            generation_id: AtomicU64::new(0),
            config: Box::new(config.clone()),
            restart_attempts: AtomicU8::new(0),
            #[cfg(test)]
            failure: AtomicU8::new(0),
            #[cfg(test)]
            exit_success: AtomicBool::new(false),
        })
    }

    fn spawn_process(config: &McpServerConfig) -> Result<StdioGeneration, String> {
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
        cmd.kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{}': {e}", config.name))?;
        // Both pipes were requested above; no fallible/awaited step may lose a
        // successfully spawned child before it is installed in its owner.
        let stdin = child.stdin.take().expect("piped MCP stdin");
        let stdout = child.stdout.take().expect("piped MCP stdout");
        Ok(StdioGeneration {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        })
    }

    fn begin_close(&self) {
        *self.closed.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.interrupt.send_modify(|epoch| *epoch += 1);
    }

    fn ensure_open(&self) -> Result<(), String> {
        if *self.closed.lock().unwrap_or_else(|p| p.into_inner()) {
            Err(format!("MCP server '{}' is closed", self.config.name))
        } else {
            Ok(())
        }
    }

    fn reaped(&self, generation: &mut Option<StdioGeneration>, status: std::process::ExitStatus) {
        debug!(server = %self.config.name, %status, "MCP direct child reaped");
        #[cfg(test)]
        self.exit_success.store(status.success(), Ordering::Relaxed);
        *generation = None;
    }

    /// Reap only the direct child. Never wait for stdout EOF: wrappers may
    /// leave descendants holding those pipes, outside this transport's scope.
    async fn reap(
        &self,
        generation: &mut Option<StdioGeneration>,
        budget: CloseBudget,
    ) -> Result<(), String> {
        let Some(g) = generation.as_mut() else {
            return Ok(());
        };
        g.stdin.take();
        #[cfg(test)]
        if self.failure.load(Ordering::Relaxed) == 1 {
            return Err("incomplete MCP cleanup: injected wait error".into());
        }
        let wait_error = |e| format!("incomplete MCP cleanup: wait failed: {e}");
        if let Some(status) = g.child.try_wait().map_err(wait_error)? {
            self.reaped(generation, status);
            return Ok(());
        }
        if let Ok(result) = timeout_at(budget.eof, g.child.wait()).await {
            self.reaped(generation, result.map_err(wait_error)?);
            return Ok(());
        }
        #[cfg(unix)]
        {
            #[cfg(test)]
            if self.failure.load(Ordering::Relaxed) == 2 {
                return Err("incomplete MCP cleanup: injected TERM error".into());
            }
            if let Some(pid) = g.child.id() {
                // The unreaped Child retains this PID; signal only that child,
                // not a guessed process group or any wrapper descendants.
                // SAFETY: kill accepts an integer PID and has no memory preconditions.
                if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
                    return Err(format!(
                        "incomplete MCP cleanup: TERM failed: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            if let Ok(result) = timeout_at(budget.term, g.child.wait()).await {
                self.reaped(generation, result.map_err(wait_error)?);
                return Ok(());
            }
        }
        g.child
            .start_kill()
            .map_err(|e| format!("incomplete MCP cleanup: forced kill failed: {e}"))?;
        let status = timeout_at(budget.end, g.child.wait())
            .await
            .map_err(|_| "incomplete MCP cleanup: forced reap timed out".to_string())?
            .map_err(wait_error)?;
        self.reaped(generation, status);
        Ok(())
    }

    async fn restart(&self, name: &str) -> Result<(), String> {
        let observed = self.generation_id.load(Ordering::Acquire);
        let mut interrupted = self.interrupt.subscribe();
        self.ensure_open()?;
        let _restart = tokio::select! {
            biased;
            _ = interrupted.changed() => return Err(format!("MCP server '{name}' recovery interrupted")),
            guard = self.restarting.lock() => guard,
        };
        self.ensure_open()?;
        if observed != self.generation_id.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut guard = RestartGuard {
            transport: self,
            complete: false,
        };
        let attempt = self.restart_attempts.fetch_add(1, Ordering::Relaxed);
        if attempt >= MAX_RESTART_ATTEMPTS {
            return Err(format!(
                "MCP server '{name}' exceeded max restart attempts ({MAX_RESTART_ATTEMPTS})"
            ));
        }
        self.interrupt.send_modify(|epoch| *epoch += 1);
        interrupted.borrow_and_update();
        let budget = CloseBudget::new();
        let mut generation = timeout_at(budget.end, self.generation.lock())
            .await
            .map_err(|_| "incomplete MCP cleanup: restart I/O lock timed out".to_string())?;
        self.reap(&mut generation, budget).await?;

        let backoff_ms = BASE_BACKOFF_MS * (1u64 << attempt.min(5));
        warn!(
            "MCP server '{name}' restarting (attempt {}/{MAX_RESTART_ATTEMPTS}, backoff {backoff_ms}ms)",
            attempt + 1
        );
        tokio::select! {
            biased;
            _ = interrupted.changed() => return Err(format!("MCP server '{name}' recovery interrupted")),
            _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
        }
        // No await between admission, spawn, and publication of ALL the pipes.
        let closed = self.closed.lock().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(format!("MCP server '{name}' is closed"));
        }
        *generation = Some(Self::spawn_process(&self.config)?);
        self.generation_id.fetch_add(1, Ordering::Release);
        guard.complete = true;
        Ok(())
    }

    async fn write_frame(
        g: &mut StdioGeneration,
        server_name: &str,
        payload: &Value,
    ) -> Result<(), String> {
        let stdin = g
            .stdin
            .as_mut()
            .ok_or_else(|| format!("MCP server '{server_name}' closed stdout"))?;
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
            .map_err(|e| format!("MCP '{server_name}' flush error: {e}"))
    }

    async fn send_request(
        &self,
        server_name: &str,
        request: &Value,
        id: u64,
        tools_changed: &AtomicBool,
    ) -> Result<Value, String> {
        let mut interrupted = self.interrupt.subscribe();
        self.ensure_open()?;
        let response = async {
            let mut generation = self.generation.lock().await;
            self.ensure_open()?;
            let g = generation
                .as_mut()
                .ok_or_else(|| format!("MCP server '{server_name}' closed stdout"))?;
            Self::write_frame(g, server_name, request).await?;
            let mut line = String::new();
            loop {
                line.clear();
                let bytes_read = g
                    .stdout
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
        };
        tokio::select! {
            biased;
            _ = interrupted.changed() => Err(format!("MCP server '{server_name}' I/O interrupted")),
            result = response => result,
        }
    }

    async fn send_notification(
        &self,
        server_name: &str,
        notification: &Value,
    ) -> Result<(), String> {
        let mut interrupted = self.interrupt.subscribe();
        self.ensure_open()?;
        let write = async {
            let mut generation = self.generation.lock().await;
            self.ensure_open()?;
            let g = generation
                .as_mut()
                .ok_or_else(|| format!("MCP server '{server_name}' closed stdout"))?;
            Self::write_frame(g, server_name, notification).await
        };
        tokio::select! {
            biased;
            _ = interrupted.changed() => Err(format!("MCP server '{server_name}' I/O interrupted")),
            result = write => result,
        }
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

#[cfg(test)]
#[path = "transport_tests.rs"]
mod lifecycle_tests;
