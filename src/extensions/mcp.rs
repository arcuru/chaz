//! MCP-server-as-extension — one `McpExtension` per configured MCP server.
//!
//! Each instance wraps an [`McpServer`] and registers its discovered tools
//! through the extension hub's normal `ToolRegistration` cap. Tools carry
//! attribution (`owner: "mcp-<server_name>"`) so they participate in
//! per-session extension filtering, the same as any built-in extension.
//!
//! Failed servers are logged and produce zero tools (matching the legacy
//! `start_mcp_servers` resilience contract).

use crate::config::McpServerConfig;
use crate::extension::caps::{CapabilityRequest, ExtensionCaps};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::mcp::server::McpServer;
use crate::tool::Tool;
use std::future::Future;
use std::pin::Pin;
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
            required_capabilities: vec![CapabilityRequest::ToolRegistration],
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn install<'a>(
        &'a self,
        caps: ExtensionCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<InstalledExtension>> + Send + 'a>> {
        Box::pin(async move {
            let tool_reg = caps
                .tool_registration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("mcp install requires ToolRegistration cap"))?;

            // Start the MCP server. If it fails, log and return empty —
            // matching the legacy resilience contract.
            let server: McpServer = match McpServer::start(&self.config).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        server = %self.config.name,
                        error = %e,
                        "MCP server failed to start — skipping its tools"
                    );
                    return Ok(InstalledExtension::empty());
                }
            };

            let server = Arc::new(server);
            let tools = match server.discover_and_wrap_tools(&self.config.name).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        server = %self.config.name,
                        error = %e,
                        "MCP server tool discovery failed — skipping"
                    );
                    return Ok(InstalledExtension::empty());
                }
            };

            let count = tools.len();
            for t in tools {
                let d = t.descriptor();
                tool_reg.register(d, Arc::new(t)).await?;
            }

            tracing::info!(
                server = %self.config.name,
                count,
                "MCP server registered as extension"
            );

            Ok(InstalledExtension::empty())
        })
    }
}
