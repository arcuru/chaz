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
    async fn refresh_tools(&self) -> Result<(), String> {
        self.tools_changed.store(false, Ordering::Relaxed);
        let tools = self.list_tools().await?;

        let mut metadata = self.tool_metadata.write().unwrap();
        let mut added = 0;
        let mut updated = 0;
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

        if updated > 0 || added > 0 {
            info!(
                server = %self.name,
                updated,
                added,
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
            } => self.send_request_http(url, client, session_id, &request).await,
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
                let notif_str =
                    serde_json::to_string(&notification).map_err(|e| e.to_string())?;
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
                let notif_str =
                    serde_json::to_string(&notification).map_err(|e| e.to_string())?;
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
                    let method = parsed
                        .get("method")
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
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

            let parsed: Value = serde_json::from_str(&body)
                .map_err(|e| format!("MCP '{}' invalid JSON response: {e}", self.name))?;

            if let Some(err) = parsed.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                return Err(format!("MCP '{}' error: {msg}", self.name));
            }

            parsed
                .get("result")
                .cloned()
                .ok_or_else(|| format!("MCP '{}': response missing result", self.name))
        }
    }

    /// Parse an SSE response stream for the JSON-RPC result.
    ///
    /// SSE events are `data: <json>\n\n`. We look for the first event
    /// containing a JSON-RPC response (has "result" or "error") and
    /// process any notifications along the way.
    async fn parse_sse_response(&self, resp: reqwest::Response) -> Result<Value, String> {
        let body = resp
            .text()
            .await
            .map_err(|e| format!("MCP '{}' SSE read error: {e}", self.name))?;

        for line in body.lines() {
            let data = match line.strip_prefix("data: ") {
                Some(d) => d.trim(),
                None => continue,
            };

            if data.is_empty() {
                continue;
            }

            let parsed: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            debug!("MCP '{}' ← (SSE) {data}", self.name);

            // Check if this is a notification
            if parsed.get("id").is_none() {
                let method = parsed
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                if method == "notifications/tools/list_changed" {
                    info!("MCP '{}' signaled tools/list_changed", self.name);
                    self.tools_changed.store(true, Ordering::Relaxed);
                }
                continue;
            }

            // This is a response
            if let Some(err) = parsed.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                return Err(format!("MCP '{}' error: {msg}", self.name));
            }

            if let Some(result) = parsed.get("result").cloned() {
                return Ok(result);
            }
        }

        Err(format!(
            "MCP '{}': no JSON-RPC response in SSE stream",
            self.name
        ))
    }
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
}
