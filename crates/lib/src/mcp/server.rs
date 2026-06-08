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

/// MCP protocol version we advertise on the `InitializeRequest`. Servers
/// negotiate down via the spec's lifecycle — they respond with whatever
/// version they actually support, and chaz uses that on the wire from
/// then on (see `HttpTransport::set_protocol_version`).
///
/// `2025-11-25` is the current published spec — see
/// `modelcontextprotocol/specification/spec/2025-11-25/schema.ts`. We
/// previously advertised `2025-03-26`, which predated the `annotations`
/// field (now consumed in `McpToolAnnotations`); bumping was correct
/// once that support landed.
const PROTOCOL_VERSION: &str = "2025-11-25";

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
    /// Server-advertised capabilities, captured during `initialize`. Tells
    /// chaz which primitives (tools, resources, prompts) the server
    /// supports — used by [`McpExtension`] to gate which wrapper tools
    /// it adds to the registry.
    capabilities: RwLock<McpServerCapabilities>,
}

/// Which MCP primitives a server advertised support for in its
/// `initialize` response. Servers omit absent capabilities entirely;
/// chaz reads each as "supported" iff the object is present.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McpServerCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
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
            capabilities: RwLock::new(McpServerCapabilities::default()),
        };

        server.initialize().await?;
        Ok(server)
    }

    /// Snapshot the server-advertised capabilities. Caller decides
    /// which wrapper tools to register.
    pub fn capabilities(&self) -> McpServerCapabilities {
        *self.capabilities.read().unwrap()
    }

    /// Configured server name (matches `McpServerConfig.name`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of tools currently in this server's metadata cache.
    /// Reflects whatever the last successful `tools/list` returned.
    pub fn tool_count(&self) -> usize {
        self.tool_metadata.read().unwrap().len()
    }

    /// Sorted list of cached tool names. Used by the TUI Peer→MCP
    /// settings page; cheap snapshot.
    pub fn tool_names(&self) -> Vec<String> {
        let metadata = self.tool_metadata.read().unwrap();
        let mut names: Vec<String> = metadata.keys().cloned().collect();
        names.sort();
        names
    }

    /// Perform the MCP initialize handshake.
    ///
    /// Capture the server's negotiated `protocolVersion` from the response
    /// and hand it to the transport so subsequent HTTP requests carry the
    /// `MCP-Protocol-Version` header the spec requires (Streamable HTTP
    /// §Protocol Version Header). Stdio transport ignores the value.
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

        // Capture which primitives the server advertises. Absent
        // sub-objects mean "not supported" — chaz won't register
        // wrapper tools for capabilities the server didn't claim.
        let caps_obj = result.get("capabilities");
        let caps = McpServerCapabilities {
            tools: caps_obj.is_some_and(|c| c.get("tools").is_some()),
            resources: caps_obj.is_some_and(|c| c.get("resources").is_some()),
            prompts: caps_obj.is_some_and(|c| c.get("prompts").is_some()),
        };
        *self.capabilities.write().unwrap() = caps;

        // Negotiated version: spec says the client SHOULD use what came
        // back, not what it sent. Fall back to what we advertised when
        // the server didn't echo a version (older or sloppy servers).
        let negotiated = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(PROTOCOL_VERSION)
            .to_string();
        self.transport.set_protocol_version(&negotiated).await;

        self.send_notification("notifications/initialized", json!({}))
            .await?;

        Ok(())
    }

    /// Discover resources from the MCP server (spec: `resources/list`).
    /// Returns the raw `resources` array — each entry is `{ uri, name?,
    /// description?, mimeType? }`. Caller formats for display or hands a
    /// URI to [`Self::read_resource`].
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, String> {
        let result = self.send_request("resources/list", json!({})).await?;
        let arr = result
            .get("resources")
            .and_then(|r| r.as_array())
            .ok_or_else(|| {
                format!(
                    "MCP server '{}': invalid resources/list response",
                    self.name
                )
            })?;
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            let uri = v
                .get("uri")
                .and_then(|u| u.as_str())
                .ok_or_else(|| format!("MCP server '{}': resource missing uri field", self.name))?
                .to_string();
            out.push(McpResource {
                uri,
                name: v.get("name").and_then(|n| n.as_str()).map(str::to_string),
                description: v
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
                mime_type: v
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .map(str::to_string),
            });
        }
        Ok(out)
    }

    /// Read a single resource by URI (spec: `resources/read`).
    /// Returns the concatenated text contents across every `contents`
    /// entry the server emits; binary entries are described inline as
    /// `[binary <mime>: N bytes]` rather than dumped raw. Subject to the
    /// same `MAX_OUTPUT_BYTES` truncation as tool calls.
    pub async fn read_resource(&self, uri: &str) -> Result<String, String> {
        let result = self
            .send_request("resources/read", json!({ "uri": uri }))
            .await?;
        let contents = result
            .get("contents")
            .and_then(|c| c.as_array())
            .ok_or_else(|| {
                format!(
                    "MCP server '{}': invalid resources/read response (no contents array)",
                    self.name
                )
            })?;
        let mut parts = Vec::new();
        for item in contents {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                parts.push(text.to_string());
            } else if let Some(blob) = item.get("blob").and_then(|b| b.as_str()) {
                let mime = item
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or("application/octet-stream");
                // base64-encoded bytes — count the encoded length, decoded
                // size is roughly 3/4 of that. Don't decode; we don't want
                // to spill binary into the LLM context.
                let approx_bytes = blob.len().saturating_mul(3) / 4;
                parts.push(format!("[binary {mime}: ~{approx_bytes} bytes]"));
            }
        }
        let joined = parts.join("\n");
        if joined.len() > MAX_OUTPUT_BYTES {
            Ok(format!(
                "{}\n\n[output truncated at {} bytes]",
                &joined[..MAX_OUTPUT_BYTES],
                MAX_OUTPUT_BYTES
            ))
        } else {
            Ok(joined)
        }
    }

    /// Discover prompts from the MCP server (spec: `prompts/list`).
    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>, String> {
        let result = self.send_request("prompts/list", json!({})).await?;
        let arr = result
            .get("prompts")
            .and_then(|p| p.as_array())
            .ok_or_else(|| format!("MCP server '{}': invalid prompts/list response", self.name))?;
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            let name = v
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| format!("MCP server '{}': prompt missing name field", self.name))?
                .to_string();
            let arguments = v
                .get("arguments")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|arg| {
                            Some(McpPromptArgument {
                                name: arg.get("name").and_then(|n| n.as_str())?.to_string(),
                                description: arg
                                    .get("description")
                                    .and_then(|d| d.as_str())
                                    .map(str::to_string),
                                required: arg
                                    .get("required")
                                    .and_then(|r| r.as_bool())
                                    .unwrap_or(false),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(McpPrompt {
                name,
                description: v
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
                arguments,
            });
        }
        Ok(out)
    }

    /// Render a prompt template with the given arguments (spec:
    /// `prompts/get`). Returns the concatenated message text — chaz
    /// surfaces this as a tool result the LLM can consume directly.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: serde_json::Map<String, Value>,
    ) -> Result<String, String> {
        let result = self
            .send_request(
                "prompts/get",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let messages = result
            .get("messages")
            .and_then(|m| m.as_array())
            .ok_or_else(|| {
                format!(
                    "MCP server '{}': invalid prompts/get response (no messages array)",
                    self.name
                )
            })?;
        let mut parts = Vec::new();
        for msg in messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content");
            if let Some(text) = content.and_then(|c| c.get("text")).and_then(|t| t.as_str()) {
                parts.push(format!("[{role}] {text}"));
            } else if let Some(content_arr) = content.and_then(|c| c.as_array()) {
                for item in content_arr {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        parts.push(format!("[{role}] {text}"));
                    }
                }
            }
        }
        let joined = parts.join("\n\n");
        if joined.len() > MAX_OUTPUT_BYTES {
            Ok(format!(
                "{}\n\n[output truncated at {} bytes]",
                &joined[..MAX_OUTPUT_BYTES],
                MAX_OUTPUT_BYTES
            ))
        } else {
            Ok(joined)
        }
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
            let annotations = tool_val
                .get("annotations")
                .and_then(McpToolAnnotations::from_json);

            tools.push(McpToolInfo {
                name,
                description,
                input_schema,
                annotations,
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
                annotations: info.annotations.clone(),
            };
            if let Some(existing) = metadata.get_mut(&info.name) {
                if *existing != new_meta {
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
            Err(e) if self.transport.is_session_expired_error(&e) => {
                // HTTP server told us our session is gone (spec
                // §Session Management — client MUST start a new
                // session on 404). Re-initialize and retry once.
                self.initialize().await?;
                info!(
                    "MCP server '{}' session re-initialized after expiry",
                    self.name
                );
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
                        annotations: info.annotations.clone(),
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
    pub(super) annotations: Option<McpToolAnnotations>,
}

/// Shared, updatable metadata for a tool. Read by McpTool::descriptor(),
/// written by McpServer::refresh_tools().
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct McpToolMetadata {
    pub(super) description: String,
    pub(super) input_schema: Value,
    pub(super) annotations: Option<McpToolAnnotations>,
}

/// Behavioral hints an MCP server may attach to a tool definition
/// (`tools/list` response, per the MCP 2025-06 spec §Tool). All fields
/// are optional and advisory — chaz uses them to seed `default_policy`
/// when the server's yaml block doesn't pin one explicitly.
///
/// Spec note: these are *hints*, not guarantees. A server claiming
/// `readOnlyHint: true` and then mutating state is misbehaving; we
/// trust the hint at policy-derivation time but the policy layer
/// (timeouts, leak detection, approval) still runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct McpToolAnnotations {
    /// Tool reads but does not modify its environment.
    pub(super) read_only_hint: Option<bool>,
    /// Tool may perform destructive (irreversible) updates.
    pub(super) destructive_hint: Option<bool>,
    /// Repeated calls with identical args have no additional effect.
    pub(super) idempotent_hint: Option<bool>,
    /// Tool interacts with entities outside its immediate environment.
    pub(super) open_world_hint: Option<bool>,
}

impl McpToolAnnotations {
    /// Parse the `annotations` object from a `tools/list` tool entry.
    /// Returns `None` when the field is absent or malformed; the caller
    /// then falls back to chaz's Medium default.
    fn from_json(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let read_bool = |k: &str| obj.get(k).and_then(|x| x.as_bool());
        Some(Self {
            read_only_hint: read_bool("readOnlyHint"),
            destructive_hint: read_bool("destructiveHint"),
            idempotent_hint: read_bool("idempotentHint"),
            open_world_hint: read_bool("openWorldHint"),
        })
    }

    /// Map hints to a default `ToolPolicy`. Conservative ordering:
    /// `destructiveHint` wins over `readOnlyHint` if both are somehow
    /// set, because dropping approval on a destructive tool is worse
    /// than requiring approval on a read-only one.
    fn to_policy(&self) -> ToolPolicy {
        if self.destructive_hint == Some(true) {
            return ToolPolicy {
                risk: RiskLevel::High,
                approval: ApprovalRequirement::Always,
                timeout: 60,
                sensitive_params: Vec::new(),
                rate_limit: None,
                grants: Default::default(),
            };
        }
        if self.read_only_hint == Some(true) {
            return ToolPolicy {
                risk: RiskLevel::Low,
                approval: ApprovalRequirement::Never,
                timeout: 60,
                sensitive_params: Vec::new(),
                rate_limit: None,
                grants: Default::default(),
            };
        }
        // No useful hints — fall through to chaz's historical default.
        mcp_default_policy()
    }
}

/// chaz's pre-annotations default for any MCP tool that didn't ship
/// behavioral hints and whose server has no explicit `default_policy`
/// block in yaml.
fn mcp_default_policy() -> ToolPolicy {
    ToolPolicy {
        risk: RiskLevel::Medium,
        approval: ApprovalRequirement::UnlessAutoApproved,
        timeout: 60,
        sensitive_params: Vec::new(),
        rate_limit: None,
        grants: Default::default(),
    }
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
        // Precedence:
        //  1. `default_policy` set explicitly on the server in yaml — wins
        //     unconditionally so users can always pin behavior they care about
        //  2. Annotations from `tools/list` — `destructiveHint` / `readOnlyHint`
        //     map to High+Always / Low+Never respectively
        //  3. Otherwise: Medium + UnlessAutoApproved, chaz's historical default
        if let Some(p) = self.server.default_policy.clone() {
            return p;
        }
        let metadata = self.server.tool_metadata.read().unwrap();
        if let Some(meta) = metadata.get(&self.raw_name)
            && let Some(ann) = &meta.annotations
        {
            return ann.to_policy();
        }
        mcp_default_policy()
    }
}

/// A single resource the MCP server exposes via `resources/list`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpResource {
    pub uri: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// A prompt template the MCP server exposes via `prompts/list`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpPrompt {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<McpPromptArgument>,
}

/// One declared argument on a prompt template.
#[derive(Clone, Debug, PartialEq)]
pub struct McpPromptArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

/// Built-in tool: list every resource exposed by one MCP server.
/// Registered by `McpExtension` only when the server advertises the
/// `resources` capability during initialize. Namespaced as
/// `{server}__list_resources`.
pub struct McpListResourcesTool {
    pub(super) server: Arc<McpServer>,
    pub(super) namespaced_name: String,
}

impl Tool for McpListResourcesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.namespaced_name.clone(),
            description: format!(
                "List every resource exposed by the '{}' MCP server. \
                 Returns URIs the model can hand to {}__read_resource.",
                self.server.name, self.server.name
            ),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        _arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let resources = self
                .server
                .list_resources()
                .await
                .map_err(classify_mcp_error)?;
            if resources.is_empty() {
                return Ok(format!("(no resources on '{}')", self.server.name));
            }
            let mut lines = Vec::with_capacity(resources.len());
            for r in &resources {
                let label = r.name.as_deref().unwrap_or(&r.uri);
                let mime = r
                    .mime_type
                    .as_deref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default();
                let desc = r
                    .description
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default();
                lines.push(format!("- {label} <{}>{}{}", r.uri, mime, desc));
            }
            Ok(lines.join("\n"))
        })
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Low,
            approval: ApprovalRequirement::Never,
            ..ToolPolicy::default()
        }
    }
}

/// Built-in tool: read one resource from one MCP server by URI.
/// Namespaced as `{server}__read_resource`.
pub struct McpReadResourceTool {
    pub(super) server: Arc<McpServer>,
    pub(super) namespaced_name: String,
}

impl Tool for McpReadResourceTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.namespaced_name.clone(),
            description: format!(
                "Read the contents of one resource on the '{}' MCP server. \
                 Pass a URI returned by {}__list_resources.",
                self.server.name, self.server.name
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "uri": {
                        "type": "string",
                        "description": "Resource URI"
                    }
                },
                "required": ["uri"],
                "additionalProperties": false
            }),
        }
    }

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        use crate::tool::ToolError;
        Box::pin(async move {
            let uri = arguments
                .get("uri")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'uri' argument".into()))?;
            self.server
                .read_resource(uri)
                .await
                .map_err(classify_mcp_error)
        })
    }

    fn default_policy(&self) -> ToolPolicy {
        // Reading is read-only by definition; same shape as list_resources.
        ToolPolicy {
            risk: RiskLevel::Low,
            approval: ApprovalRequirement::Never,
            ..ToolPolicy::default()
        }
    }
}

/// Built-in tool: list every prompt template exposed by one MCP server.
/// Registered only when the server advertises the `prompts` capability.
/// Namespaced as `{server}__list_prompts`.
pub struct McpListPromptsTool {
    pub(super) server: Arc<McpServer>,
    pub(super) namespaced_name: String,
}

impl Tool for McpListPromptsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.namespaced_name.clone(),
            description: format!(
                "List every prompt template exposed by the '{}' MCP server.",
                self.server.name
            ),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        _arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let prompts = self
                .server
                .list_prompts()
                .await
                .map_err(classify_mcp_error)?;
            if prompts.is_empty() {
                return Ok(format!("(no prompts on '{}')", self.server.name));
            }
            let mut lines = Vec::with_capacity(prompts.len());
            for p in &prompts {
                let desc = p
                    .description
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default();
                let args = if p.arguments.is_empty() {
                    String::new()
                } else {
                    let parts: Vec<String> = p
                        .arguments
                        .iter()
                        .map(|a| {
                            if a.required {
                                format!("{}*", a.name)
                            } else {
                                a.name.clone()
                            }
                        })
                        .collect();
                    format!(" [args: {}]", parts.join(", "))
                };
                lines.push(format!("- {}{}{}", p.name, args, desc));
            }
            Ok(lines.join("\n"))
        })
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Low,
            approval: ApprovalRequirement::Never,
            ..ToolPolicy::default()
        }
    }
}

/// Built-in tool: render one prompt template on one MCP server.
/// Namespaced as `{server}__get_prompt`.
pub struct McpGetPromptTool {
    pub(super) server: Arc<McpServer>,
    pub(super) namespaced_name: String,
}

impl Tool for McpGetPromptTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.namespaced_name.clone(),
            description: format!(
                "Render a prompt template on the '{}' MCP server. \
                 `arguments` is the free-form object the template expects \
                 (see {}__list_prompts for declared args).",
                self.server.name, self.server.name
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Prompt template name"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Template arguments"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        use crate::tool::ToolError;
        Box::pin(async move {
            let name = arguments
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'name' argument".into()))?;
            let args = arguments
                .get("arguments")
                .and_then(|a| a.as_object())
                .cloned()
                .unwrap_or_default();
            self.server
                .get_prompt(name, args)
                .await
                .map_err(classify_mcp_error)
        })
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Low,
            approval: ApprovalRequirement::Never,
            ..ToolPolicy::default()
        }
    }
}

/// Build the four wrapper tools for the primitives the server advertised.
/// Names are namespaced `{server}__{verb}` so they collide with neither
/// each other nor a server tool of the same name (the same `__`
/// convention `discover_and_wrap_tools` uses).
pub fn build_capability_tools(server: Arc<McpServer>, server_name: &str) -> Vec<Arc<dyn Tool>> {
    let caps = server.capabilities();
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    if caps.resources {
        out.push(Arc::new(McpListResourcesTool {
            server: server.clone(),
            namespaced_name: format!("{server_name}__list_resources"),
        }));
        out.push(Arc::new(McpReadResourceTool {
            server: server.clone(),
            namespaced_name: format!("{server_name}__read_resource"),
        }));
    }
    if caps.prompts {
        out.push(Arc::new(McpListPromptsTool {
            server: server.clone(),
            namespaced_name: format!("{server_name}__list_prompts"),
        }));
        out.push(Arc::new(McpGetPromptTool {
            server,
            namespaced_name: format!("{server_name}__get_prompt"),
        }));
    }
    out
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
#[path = "server_tests.rs"]
mod tests;
