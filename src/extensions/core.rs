//! Core-tool bundle — `shell`, `compact`, `spawn_agent`, `spawn_task`.
//!
//! These are too tightly coupled to the server to live in main.rs as
//! direct registrations now that everything else flows through extensions
//! — `SpawnAgent`/`SpawnTask` need a late-bound `Arc<Server>` (filled in
//! after `Server::new` returns), and `Compact` / `ShellExec` are the
//! always-available baseline that no session should ever lose.
//!
//! Keeping them in a `core` extension preserves the "everything is an
//! extension" surface while letting the server's spawn cell flow through
//! the same construction path as the other built-ins.

use crate::backends::BackendManager;
use crate::extension::caps::{CapabilityRequest, ExtensionCaps};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionHub, ExtensionRef, HookKind};
use crate::security::SecurityContext;
use crate::server::Server;
use crate::tools::{Compact, ShellExec, SpawnAgent, SpawnTask};
use std::future::Future;
use std::pin::Pin;
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

    fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
        hub.register_tool(Arc::new(ShellExec));
        hub.register_tool(Arc::new(Compact));
        hub.register_tool(Arc::new(SpawnAgent {
            server: self.spawn_server_cell.clone(),
            backend: self.backend.clone(),
            security: self.security.clone(),
        }));
        hub.register_tool(Arc::new(SpawnTask {
            server: self.spawn_server_cell.clone(),
            backend: self.backend.clone(),
            security: self.security.clone(),
        }));
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
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
                .ok_or_else(|| anyhow::anyhow!("core install requires ToolRegistration cap"))?;
            let tools: Vec<Arc<dyn crate::tool::Tool>> = vec![
                Arc::new(ShellExec),
                Arc::new(Compact),
                Arc::new(SpawnAgent {
                    server: self.spawn_server_cell.clone(),
                    backend: self.backend.clone(),
                    security: self.security.clone(),
                }),
                Arc::new(SpawnTask {
                    server: self.spawn_server_cell.clone(),
                    backend: self.backend.clone(),
                    security: self.security.clone(),
                }),
            ];
            for t in tools {
                let d = t.descriptor();
                tool_reg.register(d, t).await?;
            }
            Ok(InstalledExtension::empty())
        })
    }
}
