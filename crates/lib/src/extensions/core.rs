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
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::security::SecurityContext;
use crate::server::Server;
use crate::tools::{Compact, ShellExec, SpawnAgent, SpawnWorker};
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
}
