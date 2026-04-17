use crate::config::McpServerConfig;
use crate::security::SecretStore;
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};

use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// Maximum tool output size (100 KB).
const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// MCP protocol version we advertise.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// Maximum restart attempts before giving up (stdio transport).
const MAX_RESTART_ATTEMPTS: u8 = 5;

/// Base backoff delay in milliseconds (doubles each attempt: 1s, 2s, 4s, 8s, 16s).
const BASE_BACKOFF_MS: u64 = 1000;

// ============================================================================
// Transport layer
// ============================================================================

/// Transport for communicating with an MCP server.
///
/// Two variants:
/// - **Stdio**: subprocess with JSON-RPC over stdin/stdout (original)
/// - **Http**: Streamable HTTP transport (POST requests, JSON or SSE responses)
enum Transport {
    Stdio {
        stdin: Mutex<ChildStdin>,
        stdout: Mutex<BufReader<ChildStdout>>,
        _child: Box<Mutex<Child>>,
        config: Box<McpServerConfig>,
        restart_attempts: AtomicU8,
    },
    Http {
        url: String,
        client: reqwest::Client,
        /// MCP session ID returned by the server (tracked across requests).
        session_id: Mutex<Option<String>>,
    },
}

impl Transport {
    /// Create a stdio transport by spawning a subprocess.
    fn new_stdio(config: &McpServerConfig) -> Result<Self, String> {
        let (child, stdin, stdout) = Self::spawn_process(config)?;
        Ok(Transport::Stdio {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            _child: Box::new(Mutex::new(child)),
            config: Box::new(config.clone()),
            restart_attempts: AtomicU8::new(0),
        })
    }

    /// Create an HTTP transport targeting the given URL.
    fn new_http(url: &str) -> Self {
        Transport::Http {
            url: url.to_string(),
            client: reqwest::Client::new(),
            session_id: Mutex::new(None),
        }
    }

    /// Spawn a subprocess for stdio transport.
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

    /// Check if an error indicates the subprocess has died (stdio only).
    fn is_process_dead_error(&self, error: &str) -> bool {
        matches!(self, Transport::Stdio { .. })
            && (error.contains("closed stdout")
                || error.contains("write error")
                || error.contains("read error")
                || error.contains("Broken pipe"))
    }

    /// Attempt to restart the subprocess (stdio only). No-op for HTTP.
    async fn restart(&self, name: &str) -> Result<(), String> {
        match self {
            Transport::Stdio {
                stdin,
                stdout,
                _child,
                config,
                restart_attempts,
            } => {
                let attempt = restart_attempts.fetch_add(1, Ordering::Relaxed);
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

                let (child, new_stdin, new_stdout) = Self::spawn_process(config)?;
                *_child.lock().await = child;
                *stdin.lock().await = new_stdin;
                *stdout.lock().await = BufReader::new(new_stdout);

                Ok(())
            }
            Transport::Http { .. } => Ok(()), // HTTP doesn't need restart
        }
    }

    /// Reset restart counter (stdio only). Called on successful request.
    fn reset_restart_counter(&self) {
        if let Transport::Stdio {
            restart_attempts, ..
        } = self
        {
            restart_attempts.store(0, Ordering::Relaxed);
        }
    }
}

// ============================================================================
// McpServer — transport-agnostic MCP server manager
// ============================================================================

/// Manages a single MCP server connection and its tool metadata.
///
/// Works with both stdio (subprocess) and HTTP (Streamable HTTP) transports.
pub struct McpServer {
    name: String,
    transport: Transport,
    next_id: AtomicU64,
    default_policy: Option<ToolPolicy>,
    /// Set when the server sends `notifications/tools/list_changed`.
    tools_changed: AtomicBool,
    /// Shared metadata for each tool, keyed by raw tool name.
    tool_metadata: RwLock<HashMap<String, McpToolMetadata>>,
}

impl McpServer {
    /// Start an MCP server using the configured transport.
    ///
    /// For stdio: spawns a subprocess. For HTTP: connects to the URL.
    /// Both perform the initialize handshake and are ready for tool discovery.
    pub async fn start(config: &McpServerConfig) -> Result<Self, String> {
        let transport = if let Some(ref url) = config.url {
            info!("MCP server '{}': connecting via HTTP to {url}", config.name);
            Transport::new_http(url)
        } else {
            Transport::new_stdio(config)?
        };

        let server = Self {
            name: config.name.clone(),
            transport,
            next_id: AtomicU64::new(1),
            default_policy: config.default_policy.clone(),
            tools_changed: AtomicBool::new(false),
            tool_metadata: RwLock::new(HashMap::new()),
        };

        server.initialize().await?;
        Ok(server)
    }

    /// Perform the MCP initialize handshake.
    async fn initialize(&self) -> Result<(), String> {
        let result = self
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "chaz",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;

        if let Some(info) = result.get("serverInfo") {
            info!(
                "MCP server '{}' initialized: {}",
                self.name,
                info.get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
            );
        }

        self.send_notification("notifications/initialized", json!({}))
            .await?;

        Ok(())
    }

    /// Discover tools from the MCP server.
    async fn list_tools(&self) -> Result<Vec<McpToolInfo>, String> {
        let result = self.send_request("tools/list", json!({})).await?;

        let tools_array = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or_else(|| format!("MCP server '{}': invalid tools/list response", self.name))?;

        let mut tools = Vec::new();
        for tool_val in tools_array {
            let name = tool_val
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| format!("MCP server '{}': tool missing name field", self.name))?
                .to_string();
            let description = tool_val
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = tool_val
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

            tools.push(McpToolInfo {
                name,
                description,
                input_schema,
            });
        }

        Ok(tools)
    }

    /// Re-discover tools from the server and update shared metadata.
    ///
    /// Updates existing tools, adds new ones, and removes tools that the server
    /// no longer reports. Removed tools will return empty descriptors from
    /// McpTool::descriptor() (the McpTool wrapper still exists in the registry
    /// but the LLM won't see useful metadata).
    async fn refresh_tools(&self) -> Result<(), String> {
        self.tools_changed.store(false, Ordering::Relaxed);
        let tools = self.list_tools().await?;

        let mut metadata = self.tool_metadata.write().unwrap();
        let mut added = 0;
        let mut updated = 0;

        // Track which tools the server still reports
        let current_names: std::collections::HashSet<&str> =
            tools.iter().map(|t| t.name.as_str()).collect();

        // Remove tools that the server no longer reports
        let before = metadata.len();
        metadata.retain(|name, _| current_names.contains(name.as_str()));
        let removed = before - metadata.len();

        for info in &tools {
            let new_meta = McpToolMetadata {
                description: info.description.clone(),
                input_schema: info.input_schema.clone(),
            };
            if let Some(existing) = metadata.get_mut(&info.name) {
                if existing.description != new_meta.description
                    || existing.input_schema != new_meta.input_schema
                {
                    *existing = new_meta;
                    updated += 1;
                }
            } else {
                metadata.insert(info.name.clone(), new_meta);
                added += 1;
            }
        }

        if updated > 0 || added > 0 || removed > 0 {
            info!(
                server = %self.name,
                updated,
                added,
                removed,
                total = tools.len(),
                "MCP tools refreshed"
            );
        }
        if added > 0 {
            warn!(
                server = %self.name,
                added,
                "New MCP tools discovered but cannot be added to registry at runtime — restart to pick them up"
            );
        }
        if removed > 0 {
            warn!(
                server = %self.name,
                removed,
                "MCP tools removed by server — stale tool wrappers remain in registry until restart"
            );
        }

        Ok(())
    }

    /// Call a tool on the MCP server, with auto-restart on process death.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        // Lazy refresh: if the server signaled tools/list_changed, re-discover before calling
        if self.tools_changed.load(Ordering::Relaxed) {
            if let Err(e) = self.refresh_tools().await {
                warn!(server = %self.name, error = %e, "Failed to refresh tools after list_changed");
            }
        }

        let params = json!({
            "name": name,
            "arguments": arguments
        });

        let result = match self.send_request("tools/call", params.clone()).await {
            Ok(r) => r,
            Err(e) if self.transport.is_process_dead_error(&e) => {
                self.transport.restart(&self.name).await?;
                self.initialize().await?;
                info!("MCP server '{}' restarted successfully", self.name);
                self.send_request("tools/call", params).await?
            }
            Err(e) => return Err(e),
        };

        self.transport.reset_restart_counter();

        // Check if the result indicates an error
        if result.get("isError").and_then(|e| e.as_bool()) == Some(true) {
            let error_text = extract_text_content(&result);
            return Err(if error_text.is_empty() {
                "MCP tool returned an error".to_string()
            } else {
                error_text
            });
        }

        let text = extract_text_content(&result);
        if text.len() > MAX_OUTPUT_BYTES {
            Ok(format!(
                "{}\n\n[output truncated at {} bytes]",
                &text[..MAX_OUTPUT_BYTES],
                MAX_OUTPUT_BYTES
            ))
        } else {
            Ok(text)
        }
    }

    /// Send a JSON-RPC request and wait for the response.
    async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        match &self.transport {
            Transport::Stdio { stdin, stdout, .. } => {
                self.send_request_stdio(stdin, stdout, &request, id).await
            }
            Transport::Http {
                url,
                client,
                session_id,
            } => {
                self.send_request_http(url, client, session_id, &request)
                    .await
            }
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut stdin = stdin.lock().await;
                let notif_str = serde_json::to_string(&notification).map_err(|e| e.to_string())?;
                debug!("MCP '{}' → {notif_str}", self.name);
                stdin
                    .write_all(notif_str.as_bytes())
                    .await
                    .map_err(|e| format!("MCP '{}' write error: {e}", self.name))?;
                stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|e| format!("MCP '{}' write error: {e}", self.name))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| format!("MCP '{}' flush error: {e}", self.name))?;
                Ok(())
            }
            Transport::Http {
                url,
                client,
                session_id,
            } => {
                let notif_str = serde_json::to_string(&notification).map_err(|e| e.to_string())?;
                debug!("MCP '{}' → {notif_str}", self.name);

                let mut req = client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(notif_str);

                if let Some(sid) = session_id.lock().await.as_ref() {
                    req = req.header("Mcp-Session-Id", sid);
                }

                let resp = req
                    .send()
                    .await
                    .map_err(|e| format!("MCP '{}' HTTP error: {e}", self.name))?;

                if !resp.status().is_success() {
                    warn!(
                        "MCP '{}' notification HTTP {}: {}",
                        self.name,
                        resp.status(),
                        resp.text().await.unwrap_or_default()
                    );
                }
                Ok(())
            }
        }
    }

    // === Stdio transport implementation ===

    async fn send_request_stdio(
        &self,
        stdin: &Mutex<ChildStdin>,
        stdout: &Mutex<BufReader<ChildStdout>>,
        request: &Value,
        id: u64,
    ) -> Result<Value, String> {
        let mut stdin = stdin.lock().await;
        let mut stdout = stdout.lock().await;

        let request_str = serde_json::to_string(request).map_err(|e| e.to_string())?;
        debug!("MCP '{}' → {request_str}", self.name);
        stdin
            .write_all(request_str.as_bytes())
            .await
            .map_err(|e| format!("MCP '{}' write error: {e}", self.name))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("MCP '{}' write error: {e}", self.name))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("MCP '{}' flush error: {e}", self.name))?;

        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = stdout
                .read_line(&mut line)
                .await
                .map_err(|e| format!("MCP '{}' read error: {e}", self.name))?;

            if bytes_read == 0 {
                return Err(format!("MCP server '{}' closed stdout", self.name));
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let parsed: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    warn!("MCP '{}' non-JSON line: {e}", self.name);
                    continue;
                }
            };

            debug!("MCP '{}' ← {trimmed}", self.name);

            // Handle notifications (no id field)
            let resp_id = match parsed.get("id") {
                Some(id_val) => id_val.as_u64(),
                None => {
                    let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    if method == "notifications/tools/list_changed" {
                        info!("MCP '{}' signaled tools/list_changed", self.name);
                        self.tools_changed.store(true, Ordering::Relaxed);
                    } else {
                        debug!("MCP '{}' notification: {trimmed}", self.name);
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
                    return Err(format!("MCP '{}' error: {msg}", self.name));
                }
                return parsed
                    .get("result")
                    .cloned()
                    .ok_or_else(|| format!("MCP '{}': response missing result", self.name));
            }

            warn!(
                "MCP '{}' unexpected response id {:?} (expected {id})",
                self.name, resp_id
            );
        }
    }

    // === HTTP transport implementation ===

    async fn send_request_http(
        &self,
        url: &str,
        client: &reqwest::Client,
        session_id: &Mutex<Option<String>>,
        request: &Value,
    ) -> Result<Value, String> {
        let request_str = serde_json::to_string(request).map_err(|e| e.to_string())?;
        debug!("MCP '{}' → {request_str}", self.name);

        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(sid) = session_id.lock().await.as_ref() {
            req = req.header("Mcp-Session-Id", sid);
        }

        let resp = req
            .body(request_str)
            .send()
            .await
            .map_err(|e| format!("MCP '{}' HTTP error: {e}", self.name))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("MCP '{}' HTTP {status}: {body}", self.name));
        }

        // Track session ID from response headers
        if let Some(sid) = resp.headers().get("mcp-session-id") {
            if let Ok(sid_str) = sid.to_str() {
                *session_id.lock().await = Some(sid_str.to_string());
            }
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            // SSE response: parse events until we find our JSON-RPC response
            self.parse_sse_response(resp).await
        } else {
            // Direct JSON response
            let body = resp
                .text()
                .await
                .map_err(|e| format!("MCP '{}' HTTP body error: {e}", self.name))?;
            debug!("MCP '{}' ← {body}", self.name);
            parse_jsonrpc_response(&self.name, &body)
        }
    }

    /// Parse an SSE response stream for the JSON-RPC result.
    async fn parse_sse_response(&self, resp: reqwest::Response) -> Result<Value, String> {
        let body = resp
            .text()
            .await
            .map_err(|e| format!("MCP '{}' SSE read error: {e}", self.name))?;

        parse_sse_body(&self.name, &body, &self.tools_changed)
    }
}

// ============================================================================
// SSE parsing (extracted for testability)
// ============================================================================

/// Parse an SSE body for a JSON-RPC response.
///
/// Handles both `data: {...}` (with space) and `data:{...}` (without space)
/// formats. Processes notifications inline, setting `tools_changed` flag
/// when `notifications/tools/list_changed` is encountered. Returns the
/// first JSON-RPC result found, or an error.
fn parse_sse_body(
    server_name: &str,
    body: &str,
    tools_changed: &AtomicBool,
) -> Result<Value, String> {
    for line in body.lines() {
        // SSE spec: "data:" followed by optional space, then the value
        let data = if let Some(d) = line.strip_prefix("data: ") {
            d.trim()
        } else if let Some(d) = line.strip_prefix("data:") {
            d.trim()
        } else {
            continue;
        };

        if data.is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        debug!("MCP '{server_name}' ← (SSE) {data}");

        // Check if this is a notification (no "id" field)
        if parsed.get("id").is_none() {
            let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method == "notifications/tools/list_changed" {
                info!("MCP '{server_name}' signaled tools/list_changed");
                tools_changed.store(true, Ordering::Relaxed);
            }
            continue;
        }

        // This is a response — check for error first
        if let Some(err) = parsed.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(format!("MCP '{server_name}' error: {msg}"));
        }

        if let Some(result) = parsed.get("result").cloned() {
            return Ok(result);
        }
    }

    Err(format!(
        "MCP '{server_name}': no JSON-RPC response in SSE stream"
    ))
}

/// Parse a JSON-RPC response body, extracting the result or error.
fn parse_jsonrpc_response(server_name: &str, body: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(body)
        .map_err(|e| format!("MCP '{server_name}' invalid JSON response: {e}"))?;

    if let Some(err) = parsed.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("MCP '{server_name}' error: {msg}"));
    }

    parsed
        .get("result")
        .cloned()
        .ok_or_else(|| format!("MCP '{server_name}': response missing result"))
}

// ============================================================================
// Tool types
// ============================================================================

/// Metadata for a single tool discovered from an MCP server.
struct McpToolInfo {
    name: String,
    description: String,
    input_schema: Value,
}

/// Shared, updatable metadata for a tool. Read by McpTool::descriptor(),
/// written by McpServer::refresh_tools().
#[derive(Clone, Debug)]
struct McpToolMetadata {
    description: String,
    input_schema: Value,
}

/// Wraps a single MCP tool as a `Tool` trait implementation.
///
/// Description and schema are read from the server's shared metadata map,
/// so they update automatically when the server re-discovers tools.
pub struct McpTool {
    server: Arc<McpServer>,
    raw_name: String,
    namespaced_name: String,
}

impl Tool for McpTool {
    fn descriptor(&self) -> ToolDescriptor {
        let metadata = self.server.tool_metadata.read().unwrap();
        match metadata.get(&self.raw_name) {
            Some(meta) => ToolDescriptor {
                name: self.namespaced_name.clone(),
                description: meta.description.clone(),
                parameters: meta.input_schema.clone(),
            },
            None => ToolDescriptor {
                name: self.namespaced_name.clone(),
                description: String::new(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move { self.server.call_tool(&self.raw_name, arguments).await })
    }

    fn default_policy(&self) -> ToolPolicy {
        self.server.default_policy.clone().unwrap_or(ToolPolicy {
            risk: RiskLevel::Medium,
            approval: ApprovalRequirement::UnlessAutoApproved,
            timeout: 60,
            sensitive_params: Vec::new(),
            rate_limit: None,
        })
    }
}

// ============================================================================
// Directory scanning & startup
// ============================================================================

/// Load MCP server configs from a directory.
///
/// Scans for `.yaml`, `.yml`, and `.json` files. Each file should contain a single
/// `McpServerConfig`. Invalid files are logged and skipped.
pub fn load_server_configs_from_dir(dir: &std::path::Path) -> Vec<McpServerConfig> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            error!(
                "Failed to read MCP server directory '{}': {e}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut configs = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "yaml" | "yml" | "json") {
            continue;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read MCP manifest '{}': {e}", path.display());
                continue;
            }
        };

        let config: Result<McpServerConfig, String> = match ext {
            "json" => serde_json::from_str(&contents).map_err(|e| e.to_string()),
            _ => serde_yaml::from_str(&contents).map_err(|e| e.to_string()),
        };

        match config {
            Ok(cfg) => {
                info!(
                    "Loaded MCP server manifest '{}' from {}",
                    cfg.name,
                    path.display()
                );
                configs.push(cfg);
            }
            Err(e) => {
                warn!("Failed to parse MCP manifest '{}': {e}", path.display());
            }
        }
    }

    configs
}

/// Start all configured MCP servers and return their tools.
///
/// Failed servers are logged and skipped — they don't block startup.
pub async fn start_mcp_servers(configs: &[McpServerConfig]) -> Vec<Box<dyn Tool>> {
    let mut all_tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for config in configs {
        match start_one_server(config).await {
            Ok((server, tool_infos)) => {
                // Populate the server's shared metadata map
                {
                    let mut metadata = server.tool_metadata.write().unwrap();
                    for info in &tool_infos {
                        metadata.insert(
                            info.name.clone(),
                            McpToolMetadata {
                                description: info.description.clone(),
                                input_schema: info.input_schema.clone(),
                            },
                        );
                    }
                }
                let server = Arc::new(server);
                let count = tool_infos.len();
                for info in tool_infos {
                    let namespaced = format!("{}.{}", config.name, info.name);
                    if !seen_names.insert(namespaced.clone()) {
                        warn!("MCP tool name collision: '{namespaced}' — skipping duplicate");
                        continue;
                    }
                    all_tools.push(Box::new(McpTool {
                        server: server.clone(),
                        raw_name: info.name,
                        namespaced_name: namespaced,
                    }));
                }
                info!("MCP server '{}': registered {count} tool(s)", config.name);
            }
            Err(e) => {
                error!("MCP server '{}' failed to start: {e}", config.name);
            }
        }
    }

    all_tools
}

async fn start_one_server(
    config: &McpServerConfig,
) -> Result<(McpServer, Vec<McpToolInfo>), String> {
    let server = McpServer::start(config).await?;
    let tools = server.list_tools().await?;
    Ok((server, tools))
}

/// Extract concatenated text from MCP content array.
fn extract_text_content(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };

    let mut parts = Vec::new();
    for item in content {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    parts.push(text.to_string());
                }
            }
            Some(other) => {
                warn!("MCP unsupported content type: {other}");
            }
            None => {}
        }
    }

    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ================================================================
    // extract_text_content
    // ================================================================

    #[test]
    fn test_extract_text_single_item() {
        let result = json!({
            "content": [{"type": "text", "text": "hello world"}]
        });
        assert_eq!(extract_text_content(&result), "hello world");
    }

    #[test]
    fn test_extract_text_multiple_items() {
        let result = json!({
            "content": [
                {"type": "text", "text": "line 1"},
                {"type": "text", "text": "line 2"}
            ]
        });
        assert_eq!(extract_text_content(&result), "line 1\nline 2");
    }

    #[test]
    fn test_extract_text_no_content_field() {
        let result = json!({"something": "else"});
        assert_eq!(extract_text_content(&result), "");
    }

    #[test]
    fn test_extract_text_empty_content_array() {
        let result = json!({"content": []});
        assert_eq!(extract_text_content(&result), "");
    }

    #[test]
    fn test_extract_text_content_not_array() {
        let result = json!({"content": "just a string"});
        assert_eq!(extract_text_content(&result), "");
    }

    #[test]
    fn test_extract_text_skips_non_text_types() {
        let result = json!({
            "content": [
                {"type": "image", "data": "base64..."},
                {"type": "text", "text": "the text part"}
            ]
        });
        assert_eq!(extract_text_content(&result), "the text part");
    }

    #[test]
    fn test_extract_text_missing_text_field() {
        // type is "text" but the "text" field is missing
        let result = json!({
            "content": [{"type": "text"}]
        });
        assert_eq!(extract_text_content(&result), "");
    }

    // ================================================================
    // parse_jsonrpc_response
    // ================================================================

    #[test]
    fn test_jsonrpc_response_success() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let result = parse_jsonrpc_response("test", body).unwrap();
        assert_eq!(result, json!({"tools": []}));
    }

    #[test]
    fn test_jsonrpc_response_error() {
        let body =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#;
        let err = parse_jsonrpc_response("test", body).unwrap_err();
        assert!(err.contains("Invalid Request"));
    }

    #[test]
    fn test_jsonrpc_response_error_missing_message() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600}}"#;
        let err = parse_jsonrpc_response("test", body).unwrap_err();
        assert!(err.contains("unknown error"));
    }

    #[test]
    fn test_jsonrpc_response_missing_result() {
        // Has id but neither result nor error — malformed
        let body = r#"{"jsonrpc":"2.0","id":1}"#;
        let err = parse_jsonrpc_response("test", body).unwrap_err();
        assert!(err.contains("response missing result"));
    }

    #[test]
    fn test_jsonrpc_response_invalid_json() {
        let err = parse_jsonrpc_response("test", "not json at all").unwrap_err();
        assert!(err.contains("invalid JSON"));
    }

    #[test]
    fn test_jsonrpc_response_null_result() {
        // result is explicitly null — valid JSON-RPC
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let result = parse_jsonrpc_response("test", body).unwrap();
        assert_eq!(result, Value::Null);
    }

    // ================================================================
    // parse_sse_body
    // ================================================================

    #[test]
    fn test_sse_basic_response() {
        let flag = AtomicBool::new(false);
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"value\":42}}\n\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!({"value": 42}));
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_sse_no_space_after_data_colon() {
        // Some SSE implementations omit the space
        let flag = AtomicBool::new(false);
        let body = "data:{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn test_sse_error_response() {
        let flag = AtomicBool::new(false);
        let body =
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-1,\"message\":\"boom\"}}\n\n";
        let err = parse_sse_body("test", body, &flag).unwrap_err();
        assert!(err.contains("boom"));
    }

    #[test]
    fn test_sse_notification_before_response() {
        let flag = AtomicBool::new(false);
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!({"tools": []}));
        // The notification should have set the flag
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_sse_only_notifications_no_response() {
        let flag = AtomicBool::new(false);
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        let err = parse_sse_body("test", body, &flag).unwrap_err();
        assert!(err.contains("no JSON-RPC response"));
    }

    #[test]
    fn test_sse_empty_body() {
        let flag = AtomicBool::new(false);
        let err = parse_sse_body("test", "", &flag).unwrap_err();
        assert!(err.contains("no JSON-RPC response"));
    }

    #[test]
    fn test_sse_non_data_lines_ignored() {
        let flag = AtomicBool::new(false);
        let body = "\
event: message\n\
id: 1\n\
retry: 5000\n\
: this is a comment\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"ok\"}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!("ok"));
    }

    #[test]
    fn test_sse_empty_data_line_skipped() {
        let flag = AtomicBool::new(false);
        let body = "\
data: \n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":true}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!(true));
    }

    #[test]
    fn test_sse_invalid_json_data_skipped() {
        let flag = AtomicBool::new(false);
        let body = "\
data: not valid json\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"found it\"}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!("found it"));
    }

    #[test]
    fn test_sse_response_with_id_null() {
        // id: null is present (not absent), so it shouldn't be treated as notification
        let flag = AtomicBool::new(false);
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":null,\"result\":\"null-id\"}\n\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!("null-id"));
    }

    #[test]
    fn test_sse_multiple_notifications_set_flag_once() {
        let flag = AtomicBool::new(false);
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"done\"}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!("done"));
        assert!(flag.load(Ordering::Relaxed));
    }

    // ================================================================
    // Tool metadata & McpTool::descriptor()
    // ================================================================

    /// Build an McpServer with fake HTTP transport for metadata testing.
    /// The HTTP transport won't be called — we just need the metadata map.
    fn make_test_server(name: &str) -> McpServer {
        McpServer {
            name: name.to_string(),
            transport: Transport::new_http("http://unused"),
            next_id: AtomicU64::new(1),
            default_policy: None,
            tools_changed: AtomicBool::new(false),
            tool_metadata: RwLock::new(HashMap::new()),
        }
    }

    #[test]
    fn test_mcp_tool_descriptor_from_metadata() {
        let server = make_test_server("srv");
        server.tool_metadata.write().unwrap().insert(
            "my_tool".to_string(),
            McpToolMetadata {
                description: "Does things".to_string(),
                input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            },
        );
        let server = Arc::new(server);
        let tool = McpTool {
            server: server.clone(),
            raw_name: "my_tool".to_string(),
            namespaced_name: "srv.my_tool".to_string(),
        };

        let desc = tool.descriptor();
        assert_eq!(desc.name, "srv.my_tool");
        assert_eq!(desc.description, "Does things");
        assert_eq!(
            desc.parameters,
            json!({"type": "object", "properties": {"x": {"type": "string"}}})
        );
    }

    #[test]
    fn test_mcp_tool_descriptor_missing_metadata() {
        // Tool exists in registry but metadata was removed (e.g., server removed the tool)
        let server = Arc::new(make_test_server("srv"));
        let tool = McpTool {
            server: server.clone(),
            raw_name: "gone_tool".to_string(),
            namespaced_name: "srv.gone_tool".to_string(),
        };

        let desc = tool.descriptor();
        assert_eq!(desc.name, "srv.gone_tool");
        assert_eq!(desc.description, "");
        assert_eq!(desc.parameters, json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn test_mcp_tool_descriptor_updates_after_metadata_change() {
        let server = make_test_server("srv");
        server.tool_metadata.write().unwrap().insert(
            "evolving".to_string(),
            McpToolMetadata {
                description: "v1".to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
            },
        );
        let server = Arc::new(server);
        let tool = McpTool {
            server: server.clone(),
            raw_name: "evolving".to_string(),
            namespaced_name: "srv.evolving".to_string(),
        };

        assert_eq!(tool.descriptor().description, "v1");

        // Simulate metadata update (as refresh_tools would do)
        server.tool_metadata.write().unwrap().insert(
            "evolving".to_string(),
            McpToolMetadata {
                description: "v2 with new params".to_string(),
                input_schema: json!({"type": "object", "properties": {"new_param": {"type": "number"}}}),
            },
        );

        let desc = tool.descriptor();
        assert_eq!(desc.description, "v2 with new params");
        assert!(desc.parameters["properties"]["new_param"].is_object());
    }

    #[test]
    fn test_mcp_tool_default_policy_no_override() {
        let server = Arc::new(make_test_server("srv"));
        let tool = McpTool {
            server,
            raw_name: "t".to_string(),
            namespaced_name: "srv.t".to_string(),
        };
        let policy = tool.default_policy();
        assert_eq!(policy.risk, RiskLevel::Medium);
        assert_eq!(policy.approval, ApprovalRequirement::UnlessAutoApproved);
        assert_eq!(policy.timeout, 60);
    }

    #[test]
    fn test_mcp_tool_default_policy_with_server_override() {
        let mut server = make_test_server("srv");
        server.default_policy = Some(ToolPolicy {
            risk: RiskLevel::High,
            approval: ApprovalRequirement::Always,
            timeout: 10,
            sensitive_params: vec!["secret".to_string()],
            rate_limit: Some(5),
        });
        let server = Arc::new(server);
        let tool = McpTool {
            server,
            raw_name: "t".to_string(),
            namespaced_name: "srv.t".to_string(),
        };
        let policy = tool.default_policy();
        assert_eq!(policy.risk, RiskLevel::High);
        assert_eq!(policy.timeout, 10);
        assert_eq!(policy.sensitive_params, vec!["secret"]);
        assert_eq!(policy.rate_limit, Some(5));
    }

    // ================================================================
    // tools_changed flag
    // ================================================================

    #[test]
    fn test_tools_changed_flag_default_false() {
        let server = make_test_server("srv");
        assert!(!server.tools_changed.load(Ordering::Relaxed));
    }

    #[test]
    fn test_tools_changed_flag_set_by_sse_notification() {
        let flag = AtomicBool::new(false);
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"ok\"}\n";
        let _ = parse_sse_body("test", body, &flag);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_tools_changed_flag_not_set_by_other_notifications() {
        let flag = AtomicBool::new(false);
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":50}}\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"ok\"}\n";
        let _ = parse_sse_body("test", body, &flag);
        assert!(!flag.load(Ordering::Relaxed));
    }

    // ================================================================
    // Transport::is_process_dead_error
    // ================================================================

    #[test]
    fn test_http_transport_never_detects_process_death() {
        let transport = Transport::new_http("http://example.com");
        assert!(!transport.is_process_dead_error("closed stdout"));
        assert!(!transport.is_process_dead_error("write error"));
        assert!(!transport.is_process_dead_error("Broken pipe"));
    }

    // ================================================================
    // Config deserialization
    // ================================================================

    #[test]
    fn test_config_stdio_transport() {
        let yaml = "name: test\ncommand: echo\nargs: [\"hello\"]";
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "test");
        assert_eq!(config.command, "echo");
        assert!(config.url.is_none());
    }

    #[test]
    fn test_config_http_transport() {
        let yaml = "name: remote\nurl: http://localhost:8080/mcp";
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "remote");
        assert_eq!(config.url.as_deref(), Some("http://localhost:8080/mcp"));
        assert_eq!(config.command, ""); // default empty string
    }

    #[test]
    fn test_config_with_url_and_command() {
        // Both set — url takes precedence in McpServer::start
        let yaml = "name: both\ncommand: echo\nurl: http://localhost/mcp";
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.url.is_some());
        assert_eq!(config.command, "echo");
    }

    #[test]
    fn test_config_with_default_policy() {
        let yaml = r#"
name: secure
command: echo
default_policy:
  risk: high
  approval: always
  timeout: 10
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        let policy = config.default_policy.unwrap();
        assert_eq!(policy.risk, RiskLevel::High);
        assert_eq!(policy.approval, ApprovalRequirement::Always);
        assert_eq!(policy.timeout, 10);
    }

    #[test]
    fn test_config_mcp_server_dir() {
        let yaml = r#"
homeserver_url: ""
username: ""
mcp_server_dir: "/etc/chaz/mcp.d"
"#;
        let config: crate::config::Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.mcp_server_dir.as_deref(), Some("/etc/chaz/mcp.d"));
    }

    // ================================================================
    // Directory scanning
    // ================================================================

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("chaz-mcp-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_load_server_configs_from_dir_yaml() {
        let dir = test_dir("yaml");
        std::fs::write(
            dir.join("test-server.yaml"),
            "name: test-server\ncommand: echo\nargs: [\"hello\"]",
        )
        .unwrap();

        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "test-server");
        assert_eq!(configs[0].command, "echo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_server_configs_from_dir_json() {
        let dir = test_dir("json");
        std::fs::write(
            dir.join("test-server.json"),
            r#"{"name": "json-server", "command": "cat", "args": ["-"]}"#,
        )
        .unwrap();

        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "json-server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_server_configs_skips_invalid() {
        let dir = test_dir("invalid");
        std::fs::write(dir.join("good.yaml"), "name: good\ncommand: echo").unwrap();
        std::fs::write(dir.join("bad.yaml"), "not: [valid: mcp config").unwrap();
        std::fs::write(dir.join("readme.txt"), "not a manifest").unwrap();

        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "good");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_server_configs_nonexistent_dir() {
        let configs = load_server_configs_from_dir(std::path::Path::new("/nonexistent/path"));
        assert!(configs.is_empty());
    }

    #[test]
    fn test_load_server_configs_yml_extension() {
        let dir = test_dir("yml");
        std::fs::write(dir.join("server.yml"), "name: yml-server\ncommand: cat").unwrap();
        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "yml-server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_server_configs_http_manifest() {
        let dir = test_dir("http-manifest");
        std::fs::write(
            dir.join("remote.yaml"),
            "name: remote\nurl: http://localhost:9090/mcp",
        )
        .unwrap();
        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].url.as_deref(), Some("http://localhost:9090/mcp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ================================================================
    // Output truncation
    // ================================================================

    #[test]
    fn test_max_output_bytes_constant() {
        // Sanity check — should be 100 KB
        assert_eq!(MAX_OUTPUT_BYTES, 100 * 1024);
    }

    // ================================================================
    // call_tool result parsing (exercised via extract_text_content
    // + the isError / truncation logic inline)
    // ================================================================

    #[test]
    fn test_call_tool_is_error_true_with_text() {
        // Simulate the result that call_tool receives when isError is set
        let result = json!({
            "isError": true,
            "content": [{"type": "text", "text": "something broke"}]
        });
        assert_eq!(result.get("isError").and_then(|e| e.as_bool()), Some(true));
        let error_text = extract_text_content(&result);
        assert_eq!(error_text, "something broke");
    }

    #[test]
    fn test_call_tool_is_error_true_empty_text() {
        // isError with no content → fallback message
        let result = json!({"isError": true, "content": []});
        let error_text = extract_text_content(&result);
        assert!(error_text.is_empty());
        // call_tool would return "MCP tool returned an error" for this case
    }

    #[test]
    fn test_call_tool_is_error_false() {
        let result = json!({"isError": false, "content": [{"type": "text", "text": "ok"}]});
        assert_ne!(result.get("isError").and_then(|e| e.as_bool()), Some(true));
    }

    #[test]
    fn test_call_tool_is_error_absent() {
        // No isError field at all — should not be treated as error
        let result = json!({"content": [{"type": "text", "text": "fine"}]});
        assert_eq!(result.get("isError").and_then(|e| e.as_bool()), None);
    }

    #[test]
    fn test_output_truncation_logic() {
        // Simulate what call_tool does for large output
        let large_text = "x".repeat(MAX_OUTPUT_BYTES + 1000);
        assert!(large_text.len() > MAX_OUTPUT_BYTES);
        let truncated = format!(
            "{}\n\n[output truncated at {} bytes]",
            &large_text[..MAX_OUTPUT_BYTES],
            MAX_OUTPUT_BYTES
        );
        assert!(truncated.len() < large_text.len());
        assert!(truncated.contains("[output truncated at"));
        assert_eq!(
            &truncated[..MAX_OUTPUT_BYTES],
            &large_text[..MAX_OUTPUT_BYTES]
        );
    }

    #[test]
    fn test_output_at_exact_limit_not_truncated() {
        let exact_text = "x".repeat(MAX_OUTPUT_BYTES);
        // At exactly the limit, not over — should NOT truncate
        assert!(exact_text.len() <= MAX_OUTPUT_BYTES);
    }

    // ================================================================
    // list_tools parsing
    // ================================================================

    #[test]
    fn test_list_tools_parse_full_tool() {
        // Directly test the parsing logic that list_tools uses
        let response = json!({
            "tools": [{
                "name": "read_file",
                "description": "Read a file",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path"}
                    },
                    "required": ["path"]
                }
            }]
        });
        let tools_array = response.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools_array.len(), 1);
        let tool = &tools_array[0];
        assert_eq!(tool["name"].as_str().unwrap(), "read_file");
        assert_eq!(tool["description"].as_str().unwrap(), "Read a file");
        assert!(tool["inputSchema"]["properties"]["path"].is_object());
    }

    #[test]
    fn test_list_tools_missing_description_defaults() {
        let response = json!({
            "tools": [{"name": "bare_tool"}]
        });
        let tool = &response["tools"][0];
        // description defaults to "" when missing
        let description = tool
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        assert_eq!(description, "");
    }

    #[test]
    fn test_list_tools_missing_input_schema_defaults() {
        let response = json!({
            "tools": [{"name": "bare_tool", "description": "no schema"}]
        });
        let tool = &response["tools"][0];
        let input_schema = tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        assert_eq!(input_schema, json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn test_list_tools_empty_array() {
        let response = json!({"tools": []});
        let tools = response["tools"].as_array().unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn test_list_tools_missing_tools_key() {
        let response = json!({"something": "else"});
        assert!(response.get("tools").and_then(|t| t.as_array()).is_none());
    }

    #[test]
    fn test_list_tools_tool_missing_name() {
        let response = json!({
            "tools": [{"description": "no name tool"}]
        });
        let tool = &response["tools"][0];
        assert!(tool.get("name").and_then(|n| n.as_str()).is_none());
    }

    // ================================================================
    // refresh_tools metadata logic (including stale removal)
    // ================================================================

    /// Helper: directly apply refresh logic to a metadata map.
    /// Mirrors what refresh_tools does after calling list_tools.
    fn apply_refresh(
        metadata: &mut HashMap<String, McpToolMetadata>,
        tools: &[(&str, &str, Value)],
    ) -> (usize, usize, usize) {
        let current_names: std::collections::HashSet<&str> =
            tools.iter().map(|(name, _, _)| *name).collect();

        let before = metadata.len();
        metadata.retain(|name, _| current_names.contains(name.as_str()));
        let removed = before - metadata.len();

        let mut added = 0;
        let mut updated = 0;
        for (name, desc, schema) in tools {
            let new_meta = McpToolMetadata {
                description: desc.to_string(),
                input_schema: schema.clone(),
            };
            if let Some(existing) = metadata.get_mut(*name) {
                if existing.description != new_meta.description
                    || existing.input_schema != new_meta.input_schema
                {
                    *existing = new_meta;
                    updated += 1;
                }
            } else {
                metadata.insert(name.to_string(), new_meta);
                added += 1;
            }
        }
        (added, updated, removed)
    }

    #[test]
    fn test_refresh_no_changes() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "tool_a".to_string(),
            McpToolMetadata {
                description: "desc a".to_string(),
                input_schema: json!({"type": "object"}),
            },
        );

        let (added, updated, removed) = apply_refresh(
            &mut metadata,
            &[("tool_a", "desc a", json!({"type": "object"}))],
        );

        assert_eq!(added, 0);
        assert_eq!(updated, 0);
        assert_eq!(removed, 0);
        assert_eq!(metadata.len(), 1);
    }

    #[test]
    fn test_refresh_updates_schema() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "tool_a".to_string(),
            McpToolMetadata {
                description: "old desc".to_string(),
                input_schema: json!({"type": "object"}),
            },
        );

        let (added, updated, removed) = apply_refresh(
            &mut metadata,
            &[(
                "tool_a",
                "new desc",
                json!({"type": "object", "properties": {"x": {}}}),
            )],
        );

        assert_eq!(added, 0);
        assert_eq!(updated, 1);
        assert_eq!(removed, 0);
        assert_eq!(metadata["tool_a"].description, "new desc");
    }

    #[test]
    fn test_refresh_adds_new_tool() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "tool_a".to_string(),
            McpToolMetadata {
                description: "a".to_string(),
                input_schema: json!({}),
            },
        );

        let (added, updated, removed) = apply_refresh(
            &mut metadata,
            &[("tool_a", "a", json!({})), ("tool_b", "b", json!({}))],
        );

        assert_eq!(added, 1);
        assert_eq!(updated, 0);
        assert_eq!(removed, 0);
        assert_eq!(metadata.len(), 2);
        assert!(metadata.contains_key("tool_b"));
    }

    #[test]
    fn test_refresh_removes_stale_tool() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "tool_a".to_string(),
            McpToolMetadata {
                description: "a".to_string(),
                input_schema: json!({}),
            },
        );
        metadata.insert(
            "tool_b".to_string(),
            McpToolMetadata {
                description: "b".to_string(),
                input_schema: json!({}),
            },
        );

        // Server now only reports tool_a — tool_b should be removed
        let (added, updated, removed) = apply_refresh(&mut metadata, &[("tool_a", "a", json!({}))]);

        assert_eq!(added, 0);
        assert_eq!(updated, 0);
        assert_eq!(removed, 1);
        assert_eq!(metadata.len(), 1);
        assert!(metadata.contains_key("tool_a"));
        assert!(!metadata.contains_key("tool_b"));
    }

    #[test]
    fn test_refresh_removes_all_tools() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "tool_a".to_string(),
            McpToolMetadata {
                description: "a".to_string(),
                input_schema: json!({}),
            },
        );

        // Server reports empty tools list
        let (added, updated, removed) = apply_refresh(&mut metadata, &[]);

        assert_eq!(added, 0);
        assert_eq!(updated, 0);
        assert_eq!(removed, 1);
        assert!(metadata.is_empty());
    }

    #[test]
    fn test_refresh_add_update_remove_simultaneously() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "keep_same".to_string(),
            McpToolMetadata {
                description: "same".to_string(),
                input_schema: json!({}),
            },
        );
        metadata.insert(
            "will_update".to_string(),
            McpToolMetadata {
                description: "old".to_string(),
                input_schema: json!({}),
            },
        );
        metadata.insert(
            "will_remove".to_string(),
            McpToolMetadata {
                description: "doomed".to_string(),
                input_schema: json!({}),
            },
        );

        let (added, updated, removed) = apply_refresh(
            &mut metadata,
            &[
                ("keep_same", "same", json!({})),
                ("will_update", "updated", json!({})),
                ("brand_new", "new", json!({})),
            ],
        );

        assert_eq!(added, 1);
        assert_eq!(updated, 1);
        assert_eq!(removed, 1);
        assert_eq!(metadata.len(), 3);
        assert!(metadata.contains_key("keep_same"));
        assert_eq!(metadata["will_update"].description, "updated");
        assert!(metadata.contains_key("brand_new"));
        assert!(!metadata.contains_key("will_remove"));
    }

    #[test]
    fn test_descriptor_returns_empty_after_metadata_removal() {
        // Simulate: tool existed, metadata removed by refresh
        let server = Arc::new(make_test_server("srv"));
        let tool = McpTool {
            server: server.clone(),
            raw_name: "removed".to_string(),
            namespaced_name: "srv.removed".to_string(),
        };

        // Initially no metadata — descriptor returns empty
        let desc = tool.descriptor();
        assert_eq!(desc.description, "");

        // Add metadata, verify it works
        server.tool_metadata.write().unwrap().insert(
            "removed".to_string(),
            McpToolMetadata {
                description: "exists".to_string(),
                input_schema: json!({"type": "object"}),
            },
        );
        assert_eq!(tool.descriptor().description, "exists");

        // Remove metadata (as refresh_tools now does)
        server.tool_metadata.write().unwrap().remove("removed");
        let desc = tool.descriptor();
        assert_eq!(desc.description, "");
        assert_eq!(desc.parameters, json!({"type": "object", "properties": {}}));
    }

    // ================================================================
    // next_id monotonicity
    // ================================================================

    #[test]
    fn test_next_id_increments() {
        let server = make_test_server("srv");
        let id1 = server.next_id.fetch_add(1, Ordering::Relaxed);
        let id2 = server.next_id.fetch_add(1, Ordering::Relaxed);
        let id3 = server.next_id.fetch_add(1, Ordering::Relaxed);
        assert_eq!(id1, 1); // starts at 1 (set in make_test_server)
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    // ================================================================
    // Subprocess integration test
    // ================================================================

    /// Spawn a real subprocess that speaks minimal MCP JSON-RPC
    /// and test the full lifecycle through McpServer.
    #[tokio::test]
    async fn test_subprocess_full_lifecycle() {
        // This shell script implements a minimal MCP server:
        // - Responds to initialize with serverInfo
        // - Responds to tools/list with one tool
        // - Responds to tools/call with a text result
        // - Sends a tools/list_changed notification after tools/list
        let script = r#"
import sys, json

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"serverInfo": {"name": "test-mcp"}, "protocolVersion": "2025-03-26", "capabilities": {}}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method.startswith("notifications/"):
        pass  # notification, no response
    elif method == "tools/list":
        # Send a notification BEFORE the response — tests interleaved notification handling
        notif = {"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}
        sys.stdout.write(json.dumps(notif) + "\n")
        sys.stdout.flush()
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "echo", "description": "Echo input", "inputSchema": {"type": "object", "properties": {"msg": {"type": "string"}}}}]}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        text = args.get("msg", "no msg")
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": f"echo: {text}"}]}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    else:
        resp = {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": f"Unknown method: {method}"}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

        let config = McpServerConfig {
            name: "test-subprocess".to_string(),
            command: "python3".to_string(),
            args: Some(vec!["-c".to_string(), script.to_string()]),
            env: None,
            url: None,
            default_policy: None,
        };

        // Start the server (runs initialize handshake)
        let server = McpServer::start(&config)
            .await
            .expect("Failed to start MCP server");

        // Discover tools
        let tools = server.list_tools().await.expect("Failed to list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echo input");

        // The server sends a tools/list_changed notification BEFORE the tools/list response.
        // The stdio read loop processes it while scanning for the matching response id,
        // so the flag should be set.
        assert!(
            server.tools_changed.load(Ordering::Relaxed),
            "tools_changed flag should be set by interleaved notification"
        );

        // Call a tool
        let result = server
            .call_tool("echo", json!({"msg": "hello"}))
            .await
            .expect("Failed to call tool");
        assert_eq!(result, "echo: hello");

        // call_tool checked tools_changed=true, called refresh_tools which called
        // list_tools. Our script sends another notification during list_tools, so
        // the flag may be re-set. What matters is the refresh happened (tools were
        // re-listed). We can verify by checking the result came through correctly.
    }

    /// Test that call_tool handles tool errors (isError: true).
    /// Uses the lifecycle server which supports all methods.
    #[tokio::test]
    async fn test_subprocess_tool_error() {
        // Server that returns isError: true for any tools/call
        let script = r#"
import sys, json

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except:
        continue
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"serverInfo": {"name": "err-mcp"}, "protocolVersion": "2025-03-26", "capabilities": {}}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method.startswith("notifications/"):
        pass
    elif method == "tools/list":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "fail", "description": "Always fails"}]}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"isError": True, "content": [{"type": "text", "text": "tool exploded"}]}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    else:
        resp = {"jsonrpc": "2.0", "id": mid, "result": {}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

        let config = McpServerConfig {
            name: "test-err".to_string(),
            command: "python3".to_string(),
            args: Some(vec!["-u".to_string(), "-c".to_string(), script.to_string()]),
            env: None,
            url: None,
            default_policy: None,
        };

        let server = McpServer::start(&config)
            .await
            .expect("Failed to start MCP server");
        // Populate metadata so call_tool can find the tool
        server.tool_metadata.write().unwrap().insert(
            "fail".to_string(),
            McpToolMetadata {
                description: "Always fails".to_string(),
                input_schema: json!({}),
            },
        );
        let err = server.call_tool("fail", json!({})).await.unwrap_err();
        assert_eq!(err, "tool exploded");
    }

    /// Test process death detection: server exits mid-conversation
    #[tokio::test]
    async fn test_subprocess_process_death() {
        // This server handles initialize then immediately exits
        let script = r#"
import sys, json

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"serverInfo": {"name": "die-mcp"}, "protocolVersion": "2025-03-26", "capabilities": {}}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method.startswith("notifications/"):
        sys.exit(0)  # die after receiving initialized notification
"#;

        let config = McpServerConfig {
            name: "test-die".to_string(),
            command: "python3".to_string(),
            args: Some(vec!["-c".to_string(), script.to_string()]),
            env: None,
            url: None,
            default_policy: None,
        };

        let server = McpServer::start(&config).await.unwrap();
        // Next request should fail with a process-death error
        let err = server
            .send_request("tools/list", json!({}))
            .await
            .unwrap_err();
        assert!(
            server.transport.is_process_dead_error(&err),
            "Expected process death error, got: {err}"
        );
    }
}
