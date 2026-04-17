use crate::config::McpServerConfig;
use crate::security::SecretStore;
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};

use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// Maximum tool output size (100 KB).
const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// MCP protocol version we advertise.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// Maximum restart attempts before giving up.
const MAX_RESTART_ATTEMPTS: u8 = 5;

/// Base backoff delay in milliseconds (doubles each attempt: 1s, 2s, 4s, 8s, 16s).
const BASE_BACKOFF_MS: u64 = 1000;

/// Manages a single MCP subprocess server and its JSON-RPC transport.
///
/// Supports automatic restart with exponential backoff when the subprocess crashes.
pub struct McpServer {
    name: String,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    next_id: AtomicU64,
    default_policy: Option<ToolPolicy>,
    /// Kept alive so the child process isn't killed on drop of Child fields.
    _child: Mutex<Child>,
    /// Config for restarting the server.
    config: McpServerConfig,
    /// Consecutive restart attempts (reset to 0 on successful request).
    restart_attempts: AtomicU8,
}

impl McpServer {
    /// Spawn the MCP server subprocess and perform the initialize handshake.
    pub async fn start(config: &McpServerConfig) -> Result<Self, String> {
        let (child, stdin, stdout) = Self::spawn_process(config)?;

        let server = Self {
            name: config.name.clone(),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            next_id: AtomicU64::new(1),
            default_policy: config.default_policy.clone(),
            _child: Mutex::new(child),
            config: config.clone(),
            restart_attempts: AtomicU8::new(0),
        };

        server.initialize().await?;
        Ok(server)
    }

    /// Spawn the subprocess, returning the child and its stdio handles.
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

    /// Attempt to restart the subprocess with exponential backoff.
    ///
    /// Returns Ok if restart succeeded, Err if max attempts exhausted.
    async fn restart(&self) -> Result<(), String> {
        let attempt = self.restart_attempts.fetch_add(1, Ordering::Relaxed);
        if attempt >= MAX_RESTART_ATTEMPTS {
            return Err(format!(
                "MCP server '{}' exceeded max restart attempts ({MAX_RESTART_ATTEMPTS})",
                self.name
            ));
        }

        let backoff_ms = BASE_BACKOFF_MS * (1u64 << attempt.min(5));
        warn!(
            "MCP server '{}' restarting (attempt {}/{MAX_RESTART_ATTEMPTS}, backoff {backoff_ms}ms)",
            self.name,
            attempt + 1
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;

        let (child, stdin, stdout) = Self::spawn_process(&self.config)?;

        // Replace the process handles
        *self._child.lock().await = child;
        *self.stdin.lock().await = stdin;
        *self.stdout.lock().await = BufReader::new(stdout);

        // Re-initialize
        self.initialize().await?;

        info!("MCP server '{}' restarted successfully", self.name);
        Ok(())
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

        // Send initialized notification (no id, no response expected)
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

    /// Call a tool on the MCP server, with auto-restart on process death.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        let params = json!({
            "name": name,
            "arguments": arguments
        });

        let result = match self.send_request("tools/call", params.clone()).await {
            Ok(r) => r,
            Err(e) if self.is_process_dead_error(&e) => {
                // Process died — attempt restart and retry
                self.restart().await?;
                self.send_request("tools/call", params).await?
            }
            Err(e) => return Err(e),
        };

        // Reset restart counter on success
        self.restart_attempts.store(0, Ordering::Relaxed);

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
        // Truncate large outputs
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

    /// Check if an error indicates the subprocess has died.
    fn is_process_dead_error(&self, error: &str) -> bool {
        error.contains("closed stdout")
            || error.contains("write error")
            || error.contains("read error")
            || error.contains("Broken pipe")
    }

    /// Send a JSON-RPC request and wait for the matching response.
    async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let mut stdin = self.stdin.lock().await;
        let mut stdout = self.stdout.lock().await;

        // Write request as a single line
        let request_str = serde_json::to_string(&request).map_err(|e| e.to_string())?;
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

        // Read lines until we find a response with matching id
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

            // Skip notifications (no id field)
            let resp_id = match parsed.get("id") {
                Some(id_val) => id_val.as_u64(),
                None => {
                    debug!("MCP '{}' notification: {trimmed}", self.name);
                    continue;
                }
            };

            if resp_id == Some(id) {
                // Check for JSON-RPC error
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

            // Mismatched id — log and keep reading
            warn!(
                "MCP '{}' unexpected response id {:?} (expected {id})",
                self.name, resp_id
            );
        }
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let mut stdin = self.stdin.lock().await;
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
}

/// Metadata for a single tool discovered from an MCP server.
struct McpToolInfo {
    name: String,
    description: String,
    input_schema: Value,
}

/// Wraps a single MCP tool as a `Tool` trait implementation.
///
/// Each discovered tool from an MCP server becomes one `McpTool` instance,
/// registered in the `ToolRegistry` with a namespaced name (`server.tool`).
pub struct McpTool {
    server: Arc<McpServer>,
    /// Raw tool name as the MCP server knows it
    raw_name: String,
    /// Namespaced name: `server_name.tool_name`
    namespaced_name: String,
    description: String,
    input_schema: Value,
}

impl Tool for McpTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.namespaced_name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.clone(),
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

/// Load MCP server configs from a directory.
///
/// Scans for `.yaml`, `.yml`, and `.json` files. Each file should contain a single
/// `McpServerConfig`. Invalid files are logged and skipped.
pub fn load_server_configs_from_dir(dir: &std::path::Path) -> Vec<McpServerConfig> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read MCP server directory '{}': {e}", dir.display());
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
            "json" => serde_json::from_str(&contents)
                .map_err(|e| e.to_string()),
            _ => serde_yaml::from_str(&contents)
                .map_err(|e| e.to_string()),
        };

        match config {
            Ok(cfg) => {
                info!("Loaded MCP server manifest '{}' from {}", cfg.name, path.display());
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
                        description: info.description,
                        input_schema: info.input_schema,
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
