//! MCP-server-as-extension — one `McpExtension` per configured MCP server.
//!
//! Each instance wraps an [`McpServer`] and contributes its discovered
//! tools through `ExtensionInstance::tools`. Tools carry attribution
//! (`owner: "mcp-<server_name>"`) so they participate in per-session
//! extension filtering, the same as any built-in extension.
//!
//! Failed servers are logged and produce zero tools (matching the legacy
//! `start_mcp_servers` resilience contract).

use crate::config::McpServerConfig;
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::mcp::server::{McpServer, build_capability_tools};
use crate::tool::Tool;
use std::sync::Arc;
use tracing::warn;

/// An MCP server wrapped as an extension.
pub struct McpExtension {
    /// Leaked extension name — the `Extension` trait requires `&'static str`,
    /// and MCP extensions live for the process lifetime anyway.
    name: &'static str,
    /// Frozen copy of the server config.
    config: McpServerConfig,
}

impl McpExtension {
    pub fn new(config: McpServerConfig) -> Self {
        let name: &'static str = Box::leak(format!("mcp-{}", config.name).into_boxed_str());
        Self { name, config }
    }
}

impl Extension for McpExtension {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name.to_string(),
            extension_ref: ExtensionRef::builtin(self.name),
            supported_hooks: vec![HookKind::Tool],
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn instantiate<'a>(&'a self, _scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        let config = self.config.clone();
        let name = self.name;
        Box::pin(async move {
            // Start the MCP server. If it fails, log and produce an
            // empty instance — matching the legacy resilience contract.
            let tools: Vec<Arc<dyn Tool>> = match McpServer::start(&config).await {
                Ok(server) => {
                    let server = Arc::new(server);
                    let capability_tools = build_capability_tools(server.clone(), &config.name);
                    match server.discover_and_wrap_tools(&config.name).await {
                        Ok(t) => {
                            let count = t.len();
                            let cap_count = capability_tools.len();
                            tracing::info!(
                                server = %config.name,
                                tools = count,
                                capability_tools = cap_count,
                                "MCP server registered as extension"
                            );
                            let mut all: Vec<Arc<dyn Tool>> = t
                                .into_iter()
                                .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
                                .collect();
                            all.extend(capability_tools);
                            all
                        }
                        Err(e) => {
                            warn!(
                                server = %config.name,
                                error = %e,
                                "MCP server tool discovery failed — skipping"
                            );
                            // Resources/prompts wrappers can still work
                            // even if tools/list failed.
                            capability_tools
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        server = %config.name,
                        error = %e,
                        "MCP server failed to start — skipping its tools"
                    );
                    Vec::new()
                }
            };

            Ok(Arc::new(McpInstance {
                manifest,
                _name: name,
                tools,
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct McpInstance {
    manifest: ExtensionManifest,
    _name: &'static str,
    tools: Vec<Arc<dyn Tool>>,
}

impl ExtensionInstance for McpInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}
