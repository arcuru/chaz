//! Core-tool bundle — `shell`, `compact`, `spawn_agent`, `spawn_worker`.
//!
//! These are too tightly coupled to the server to live in main.rs as
//! direct registrations now that everything else flows through extensions
//! — `SpawnAgent`/`SpawnWorker` need a late-bound `Arc<Server>` (filled in
//! after `Server::new` returns), and `Compact` / `ShellExec` are the
//! always-available baseline that no session should ever lose.
//!
//! Keeping them in a `core` extension preserves the "everything is an
//! extension" surface while letting the server's spawn cell flow through
//! the same construction path as the other built-ins.

use crate::backends::BackendManager;
use crate::extension::caps::{CapFuture, StatusSegment};
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::mcp::McpServerStatus;
use crate::security::SecurityContext;
use crate::server::Server;
use crate::tools::{Compact, ShellExec, SpawnAgent, SpawnWorker};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

pub struct CoreExtension {
    pub spawn_server_cell: Arc<OnceLock<Arc<Server>>>,
    pub backend: BackendManager,
    pub security: SecurityContext,
}

impl CoreExtension {
    pub fn new(
        spawn_server_cell: Arc<OnceLock<Arc<Server>>>,
        backend: BackendManager,
        security: SecurityContext,
    ) -> Self {
        Self {
            spawn_server_cell,
            backend,
            security,
        }
    }
}

impl Extension for CoreExtension {
    fn name(&self) -> &'static str {
        "core"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![HookKind::Tool],
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn instantiate<'a>(&'a self, _scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        let spawn_cell = self.spawn_server_cell.clone();
        let backend = self.backend.clone();
        let security = self.security.clone();
        Box::pin(async move {
            Ok(Arc::new(CoreInstance {
                manifest,
                spawn_server_cell: spawn_cell,
                backend,
                security,
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct CoreInstance {
    manifest: ExtensionManifest,
    spawn_server_cell: Arc<OnceLock<Arc<Server>>>,
    backend: BackendManager,
    security: SecurityContext,
}

impl ExtensionInstance for CoreInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn tools(&self) -> Vec<Arc<dyn crate::tool::Tool>> {
        vec![
            Arc::new(ShellExec),
            Arc::new(Compact),
            Arc::new(SpawnAgent {
                server: self.spawn_server_cell.clone(),
                backend: self.backend.clone(),
                security: self.security.clone(),
            }),
            Arc::new(SpawnWorker {
                server: self.spawn_server_cell.clone(),
                backend: self.backend.clone(),
                security: self.security.clone(),
            }),
        ]
    }

    fn status_segment(&self) -> Option<Arc<dyn StatusSegment>> {
        Some(Arc::new(McpStatus {
            server: self.spawn_server_cell.clone(),
        }))
    }
}

/// Publishes one `mcp` status segment: the count of running MCP servers
/// (and any that failed to start), read live from the server's MCP
/// registry. Global, not per-agent — the first producer for the
/// extension-output-store path. Empty when no MCP servers are configured.
struct McpStatus {
    server: Arc<OnceLock<Arc<Server>>>,
}

impl StatusSegment for McpStatus {
    fn status_segments<'a>(
        &'a self,
        _agent_name: &'a str,
    ) -> CapFuture<'a, BTreeMap<String, String>> {
        Box::pin(async move {
            let mut out = BTreeMap::new();
            let Some(server) = self.server.get() else {
                return Ok(out);
            };
            let (mut running, mut failed, mut starting) = (0u32, 0u32, 0u32);
            for entry in server.mcp_registry().snapshot() {
                match entry.status {
                    McpServerStatus::Starting => starting += 1,
                    McpServerStatus::Running { .. } => running += 1,
                    McpServerStatus::Failed { .. } => failed += 1,
                }
            }
            if running + failed + starting > 0 {
                // Servers still starting are called out separately: they
                // are neither working nor broken, and reporting them as
                // either would misread a boot in progress.
                let mut text = format!("{running} mcp");
                if starting > 0 {
                    text.push_str(&format!(", {starting} starting"));
                }
                if failed > 0 {
                    text.push_str(&format!(", {failed} down"));
                }
                out.insert("mcp".to_string(), text);
            }
            Ok(out)
        })
    }
}
