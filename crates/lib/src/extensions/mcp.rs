//! MCP-server-as-extension — one `McpExtension` per configured MCP server.
//!
//! Instantiation owns the transport immediately, starts its handshake off the
//! boot path, and returns an instance with no tools, so a peer becomes
//! interactive without waiting on any MCP handshake or discovery. Startup
//! then registers the server's tools directly into the shared
//! [`ToolRegistry`], where the next turn's tool list picks them up.
//!
//! Tools still carry attribution (`owner: "mcp-<server_name>"`) so they
//! participate in per-session extension filtering the same as any built-in
//! extension's — arriving late changes when they appear, not what they are.
//!
//! Failure is absorbed, never propagated: a server that cannot start, or
//! that does not settle within its startup timeout, is recorded in the
//! [`McpRegistry`] as failed and contributes zero tools. That preserves the
//! resilience contract that one broken server cannot stop a peer booting.

use crate::config::McpServerConfig;
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::mcp::McpRegistry;
use crate::mcp::server::{McpServer, build_capability_tools};
use crate::tool::{Tool, ToolRegistry};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

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

    fn instantiate<'a>(&'a self, scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        let config = self.config.clone();
        let name = self.name;
        let mcp_registry = scope_ctx.peer().mcp_registry.clone();
        let tool_registry = scope_ctx.peer().tool_registry.clone();
        Box::pin(async move {
            // Admit, spawn and retain ownership synchronously. `install_all`
            // announces every Global server before a caller can await readiness;
            // the startup task cannot publish through the held admission lock.
            mcp_registry.launch(config.name.clone(), || {
                let server = Arc::new(McpServer::start(&config)?);
                tool_registry.announce_pending_source(&config.name);
                let handle = tokio::spawn(start_server(
                    config.clone(),
                    name,
                    server.clone(),
                    mcp_registry.clone(),
                    PendingSource {
                        name: config.name.clone(),
                        tools: tool_registry.clone(),
                    },
                ));
                Ok((server, handle))
            });

            // No tools: they go straight into the shared `ToolRegistry`
            // when they arrive, not through the hub's Global drain, which
            // has already run by then and takes `&mut self` besides.
            Ok(Arc::new(McpInstance {
                manifest,
                _name: name,
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

/// Created before spawn so even aborting an unpolled startup retires its notice.
struct PendingSource {
    name: String,
    tools: Arc<ToolRegistry>,
}

impl Drop for PendingSource {
    fn drop(&mut self) {
        self.tools.finish_pending_source(&self.name);
    }
}

/// Startup retains no exclusive process owner: the registry already owns the
/// transport and this task. Timeout stops protocol work, then explicitly closes.
async fn start_server(
    config: McpServerConfig,
    owner: &'static str,
    server: Arc<McpServer>,
    mcp_registry: Arc<McpRegistry>,
    pending: PendingSource,
) {
    use futures::FutureExt;
    let tool_registry = &pending.tools;
    let timeout = Duration::from_secs(config.startup_timeout_secs);
    let started = std::time::Instant::now();
    let body = std::panic::AssertUnwindSafe(connect_and_discover(&server, &config.name))
        .catch_unwind()
        .map(|caught| caught.unwrap_or_else(|_| Err("panicked during startup".to_string())));
    let outcome = match tokio::time::timeout(timeout, body).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "did not start within {}s (mcp_servers.startup_timeout_secs)",
            timeout.as_secs()
        )),
    };
    match outcome {
        Ok((tools, discovery_error)) => {
            if mcp_registry.publish(
                &config.name,
                server.clone(),
                discovery_error.clone(),
                tools,
                tool_registry,
                owner,
            ) {
                info!(server = %config.name, elapsed_ms = started.elapsed().as_millis() as u64,
                    tools = server.tool_count(), discovery_failed = discovery_error.is_some(), "MCP server ready");
            }
        }
        Err(mut error) => {
            if let Err(cleanup) = server.shutdown().await {
                error = format!("{error}; {cleanup}");
            }
            warn!(server = %config.name, %error, "MCP server failed to start — skipping its tools");
            mcp_registry.insert_failed(config.name.clone(), error);
        }
    }
}

/// Build tools without publishing any of them inside the cancellable future.
async fn connect_and_discover(
    server: &Arc<McpServer>,
    name: &str,
) -> Result<(Vec<Arc<dyn Tool>>, Option<String>), String> {
    server.initialize().await?;
    let capability_tools = build_capability_tools(server.clone(), name);
    let (mut tools, discovery_error): (Vec<Arc<dyn Tool>>, Option<String>) = match server
        .discover_and_wrap_tools(name)
        .await
    {
        Ok(t) => (
            t.into_iter()
                .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
                .collect(),
            None,
        ),
        Err(e) => {
            warn!(server = %name, error = %e, "MCP server tool discovery failed — registering capability wrappers only");
            (Vec::new(), Some(e))
        }
    };
    tools.extend(capability_tools);
    Ok((tools, discovery_error))
}

struct McpInstance {
    manifest: ExtensionManifest,
    _name: &'static str,
}

impl ExtensionInstance for McpInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
