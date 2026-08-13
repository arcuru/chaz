//! MCP-server-as-extension — one `McpExtension` per configured MCP server.
//!
//! Instantiation does not start the server. It announces the server, spawns
//! its startup off the boot path, and returns an instance with no tools, so
//! a peer becomes interactive without waiting on any MCP server. Startup
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
            // Announce before spawning, on this thread. `install_all`
            // instantiates every Global extension before anything can wait
            // on readiness, so announcing here guarantees a waiter sees
            // this server as pending rather than as not-yet-configured.
            mcp_registry.insert_starting(config.name.clone());
            tool_registry.announce_pending_source(&config.name);

            let handle = tokio::spawn(start_server(
                config,
                name,
                mcp_registry.clone(),
                tool_registry,
            ));
            mcp_registry.track_task(handle);

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

/// Start one MCP server, register its tools, and record how it went.
///
/// Runs as a background task, so it must not return an error anyone is
/// expected to read: every exit path writes a terminal status to
/// `mcp_registry` and retires the pending-source announcement. That
/// includes the timeout path and the panic path — a task that skipped
/// either would leave the server stuck as `Starting` and hold
/// [`McpRegistry::wait_ready`] open forever, hanging a one-shot run that
/// was waiting on it.
async fn start_server(
    config: McpServerConfig,
    owner: &'static str,
    mcp_registry: Arc<McpRegistry>,
    tool_registry: Arc<ToolRegistry>,
) {
    use futures::FutureExt;

    let timeout = Duration::from_secs(config.startup_timeout_secs);
    let started = std::time::Instant::now();

    // `AssertUnwindSafe` is sound here because a panic mid-startup leaves
    // nothing observable behind: the server handle is local to the inner
    // future, and any tools it managed to register are individually valid.
    let body = std::panic::AssertUnwindSafe(connect_and_register(&config, owner, &tool_registry))
        .catch_unwind()
        .map(|caught| match caught {
            Ok(result) => result,
            Err(_) => Err("panicked during startup".to_string()),
        });

    let outcome = tokio::time::timeout(timeout, body).await;

    match outcome {
        Ok(Ok((server, discovery_error))) => {
            info!(
                server = %config.name,
                elapsed_ms = started.elapsed().as_millis() as u64,
                tools = server.tool_count(),
                discovery_failed = discovery_error.is_some(),
                "MCP server ready"
            );
            mcp_registry.insert_running(config.name.clone(), server, discovery_error);
        }
        Ok(Err(e)) => {
            warn!(
                server = %config.name,
                error = %e,
                "MCP server failed to start — skipping its tools"
            );
            mcp_registry.insert_failed(config.name.clone(), e);
        }
        Err(_) => {
            let error = format!(
                "did not start within {}s (mcp_servers.startup_timeout_secs)",
                timeout.as_secs()
            );
            warn!(
                server = %config.name,
                timeout_secs = timeout.as_secs(),
                "MCP server startup timed out — skipping its tools"
            );
            mcp_registry.insert_failed(config.name.clone(), error);
        }
    }

    // Retire the announcement last, and on every path. Until this runs, a
    // call to one of this server's tools is answered with "still starting"
    // rather than "unknown tool".
    tool_registry.finish_pending_source(&config.name);
}

/// The startup body proper: handshake, discover, register.
///
/// `Err` means the server never came up. `Ok` with a `Some` second element
/// means it came up but `tools/list` failed — its resource and prompt
/// wrappers are registered and its tools are not.
async fn connect_and_register(
    config: &McpServerConfig,
    owner: &'static str,
    tool_registry: &ToolRegistry,
) -> Result<(Arc<McpServer>, Option<String>), String> {
    let server = Arc::new(McpServer::start(config).await?);
    let capability_tools = build_capability_tools(server.clone(), &config.name);

    let (tools, discovery_error): (Vec<Arc<dyn Tool>>, Option<String>) =
        match server.discover_and_wrap_tools(&config.name).await {
            Ok(t) => (
                t.into_iter()
                    .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
                    .collect(),
                None,
            ),
            Err(e) => {
                warn!(
                    server = %config.name,
                    error = %e,
                    "MCP server tool discovery failed — registering capability wrappers only"
                );
                // Resources/prompts wrappers can still work even if
                // tools/list failed.
                (Vec::new(), Some(e))
            }
        };

    for tool in tools.into_iter().chain(capability_tools) {
        tool_registry.register_arc_owned(tool, Some(owner));
    }

    Ok((server, discovery_error))
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
