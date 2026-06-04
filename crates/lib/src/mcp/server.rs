//! `McpServer` — manages one MCP server connection, its tool metadata,
//! and the `McpTool` wrapper that exposes each discovered tool as a
//! `Tool` trait impl.

use crate::config::McpServerConfig;
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};

use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tracing::{info, warn};

use super::transport::Transport;

/// Maximum tool output size (100 KB).
const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// MCP protocol version we advertise.
const PROTOCOL_VERSION: &str = "2025-03-26";

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
    pub(super) tool_metadata: RwLock<HashMap<String, McpToolMetadata>>,
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
    pub(super) async fn list_tools(&self) -> Result<Vec<McpToolInfo>, String> {
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
        if self.tools_changed.load(Ordering::Relaxed)
            && let Err(e) = self.refresh_tools().await
        {
            warn!(server = %self.name, error = %e, "Failed to refresh tools after list_changed");
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
        self.transport
            .send_request(&self.name, &request, id, &self.tools_changed)
            .await
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        self.transport
            .send_notification(&self.name, &notification)
            .await
    }
    /// Discover tools from the server and return `McpTool` wrappers.
    ///
    /// Populates the internal metadata map and creates one `McpTool` per
    /// discovered tool. Each wrapper shares this server's connection via
    /// `Arc`. The caller is responsible for holding the `Arc<McpServer>`
    /// alive — the tools' `execute()` calls route back through it.
    ///
    /// Namespaced names are `{server_name}__{tool_name}` (e.g.
    /// `filesystem__read_file`). Double-underscore matches the convention
    /// used by Anthropic's Agent SDK / Claude Code (`mcp__server__tool`)
    /// and Docker's MCP Gateway, and stays within the
    /// `^[a-zA-Z0-9_-]{1,64}$` shape that OpenAI / OpenRouter / DeepSeek /
    /// Groq / Together require for function names. Tools whose namespaced
    /// name would exceed `MAX_TOOL_NAME_LEN` (64) are dropped with a
    /// warning — every common provider 400s on longer names.
    pub async fn discover_and_wrap_tools(
        self: &Arc<Self>,
        server_name: &str,
    ) -> Result<Vec<McpTool>, String> {
        let tool_infos = self.list_tools().await?;
        {
            let mut metadata = self.tool_metadata.write().unwrap();
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
        Ok(tool_infos
            .into_iter()
            .filter_map(|info| {
                let raw = info.name;
                let namespaced = format!("{server_name}__{raw}");
                if namespaced.len() > MAX_TOOL_NAME_LEN {
                    tracing::warn!(
                        server = %server_name,
                        tool = %raw,
                        len = namespaced.len(),
                        max = MAX_TOOL_NAME_LEN,
                        "MCP tool namespaced name exceeds provider limit; skipping",
                    );
                    return None;
                }
                Some(McpTool {
                    server: self.clone(),
                    raw_name: raw,
                    namespaced_name: namespaced,
                })
            })
            .collect())
    }
}

/// Maximum length for a tool name accepted by the major LLM providers.
/// OpenAI, Anthropic, and the MCP spec all converge on 64. Names longer
/// than this 400 on the wire (see `claude-code#23149` for a real case).
pub(super) const MAX_TOOL_NAME_LEN: usize = 64;

/// Metadata for a single tool discovered from an MCP server.
pub(super) struct McpToolInfo {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) input_schema: Value,
}

/// Shared, updatable metadata for a tool. Read by McpTool::descriptor(),
/// written by McpServer::refresh_tools().
#[derive(Clone, Debug)]
pub(super) struct McpToolMetadata {
    pub(super) description: String,
    pub(super) input_schema: Value,
}

/// Wraps a single MCP tool as a `Tool` trait implementation.
///
/// Description and schema are read from the server's shared metadata map,
/// so they update automatically when the server re-discovers tools.
pub struct McpTool {
    pub(super) server: Arc<McpServer>,
    pub(super) raw_name: String,
    pub(super) namespaced_name: String,
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
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            self.server
                .call_tool(&self.raw_name, arguments)
                .await
                .map_err(classify_mcp_error)
        })
    }

    fn default_policy(&self) -> ToolPolicy {
        self.server.default_policy.clone().unwrap_or(ToolPolicy {
            risk: RiskLevel::Medium,
            approval: ApprovalRequirement::UnlessAutoApproved,
            timeout: 60,
            sensitive_params: Vec::new(),
            rate_limit: None,
            grants: Default::default(),
        })
    }
}

/// Classify a stringly-typed MCP error into a typed `ToolError`.
///
/// The transport layer still returns `Result<_, String>` (MCP-internal
/// protocol errors are heterogeneous enough that keeping them as strings
/// is pragmatic). This shim maps transport-origin substrings to the
/// retryable `Network` variant so the runtime can back off on transient
/// HTTP/DNS/socket failures, and keeps everything else as `Execution`.
fn classify_mcp_error(msg: String) -> crate::tool::ToolError {
    use crate::tool::ToolError;
    // Substrings produced by HttpTransport::{send_request, send_notification}
    // and StdioTransport's read/write paths.
    let network_markers = [
        "HTTP error:",     // reqwest::send failure (DNS, connect, TLS)
        "HTTP body error", // reqwest::text failure mid-stream
        "SSE read error",  // SSE stream truncated
        "closed stdout",   // subprocess died mid-conversation
        "write error",     // subprocess pipe broke
        "read error",      // subprocess pipe broke
        "Broken pipe",     // OS-level pipe error
    ];
    if network_markers.iter().any(|m| msg.contains(m)) {
        ToolError::Network(msg)
    } else {
        ToolError::Execution(msg)
    }
}

/// Extract concatenated text from MCP content array.
pub(super) fn extract_text_content(result: &Value) -> String {
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
    use crate::tool::ToolError;

    // ================================================================
    // classify_mcp_error
    // ================================================================

    #[test]
    fn test_classify_http_error_is_network() {
        let err = classify_mcp_error("MCP 'srv' HTTP error: connection refused".to_string());
        assert!(matches!(err, ToolError::Network(_)));
    }

    #[test]
    fn test_classify_http_body_error_is_network() {
        let err = classify_mcp_error("MCP 'srv' HTTP body error: premature eof".to_string());
        assert!(matches!(err, ToolError::Network(_)));
    }

    #[test]
    fn test_classify_closed_stdout_is_network() {
        // Subprocess died — conceptually a network/transport failure for our purposes.
        let err = classify_mcp_error("MCP server 'srv' closed stdout".to_string());
        assert!(matches!(err, ToolError::Network(_)));
    }

    #[test]
    fn test_classify_write_error_is_network() {
        let err = classify_mcp_error("MCP 'srv' write error: Broken pipe".to_string());
        assert!(matches!(err, ToolError::Network(_)));
    }

    #[test]
    fn test_classify_tool_returned_error_is_execution() {
        // Application-level tool failures stay as Execution.
        let err = classify_mcp_error("file not found".to_string());
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[test]
    fn test_classify_protocol_error_is_execution() {
        // JSON-RPC protocol errors aren't transport-level.
        let err = classify_mcp_error("MCP 'srv' error: Method not found".to_string());
        assert!(matches!(err, ToolError::Execution(_)));
    }

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
            namespaced_name: "srv__my_tool".to_string(),
        };
        let desc = tool.descriptor();
        assert_eq!(desc.name, "srv__my_tool");
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
            namespaced_name: "srv__gone_tool".to_string(),
        };

        let desc = tool.descriptor();
        assert_eq!(desc.name, "srv__gone_tool");
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
            namespaced_name: "srv__evolving".to_string(),
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
            namespaced_name: "srv__t".to_string(),
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
            grants: Default::default(),
        });
        let server = Arc::new(server);
        let tool = McpTool {
            server,
            raw_name: "t".to_string(),
            namespaced_name: "srv__t".to_string(),
        };
        let policy = tool.default_policy();
        assert_eq!(policy.risk, RiskLevel::High);
        assert_eq!(policy.timeout, 10);
        assert_eq!(policy.sensitive_params, vec!["secret"]);
        assert_eq!(policy.rate_limit, Some(5));
    }

    #[test]
    fn test_tools_changed_flag_default_false() {
        let server = make_test_server("srv");
        assert!(!server.tools_changed.load(Ordering::Relaxed));
    }

    // ================================================================
    // Output truncation
    // ================================================================

    #[test]
    fn test_max_output_bytes_constant() {
        // Sanity check — should be 100 KB
        assert_eq!(MAX_OUTPUT_BYTES, 100 * 1024);
    }

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
            namespaced_name: "srv__removed".to_string(),
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
    // Subprocess integration tests
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
