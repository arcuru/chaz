//! Built-in chaz extensions.
//!
//! Each submodule defines one [`crate::extension::Extension`] implementation.
//! [`register_builtins`] wires them all up — call it from `main.rs` after
//! the [`ExtensionHub`] is constructed. Tools registered through the hub
//! are surfaced to the runtime by reading `hub.tools_for_registry()` and
//! pushing them into the legacy [`crate::tool::ToolRegistry`].

pub mod core;
pub mod fs;
pub mod heartbeat;
pub mod memory;
pub mod path_normalizer;
pub mod security_warnings;
pub mod system;
pub mod web;

use crate::backends::BackendManager;
use crate::embedding::Embedder;
use crate::extension::ExtensionHub;
use crate::hosted_index::HostedIndex;
use crate::security::SecurityContext;
use crate::server::Server;
use crate::session::SessionRegistry;
use crate::tools::SearchBackend;
use std::sync::{Arc, OnceLock};

/// Shared deps that several built-in extensions need at construction time.
/// Bundled into a struct so the `register_builtins` signature stays stable
/// as more extensions are added.
pub struct BuiltinDeps {
    pub agent_index: HostedIndex,
    pub session_registry: Arc<SessionRegistry>,
    pub embedder: Option<Arc<dyn Embedder>>,
    pub web_search_backends: Vec<SearchBackend>,
    pub spawn_server_cell: Arc<OnceLock<Arc<Server>>>,
    pub backend_manager: BackendManager,
    pub security: SecurityContext,
}

/// Register every built-in extension on the hub. Tools and commands land
/// inside the hub via `register_tool` / `register_command` during each
/// extension's `register()`. After this returns, `hub.tools_for_registry()`
/// gives the caller everything to populate a [`crate::tool::ToolRegistry`].
///
/// Order is registration order — hooks fire and commands collide in this
/// sequence.
pub fn register_builtins(hub: &mut ExtensionHub, deps: BuiltinDeps) {
    let extensions: Vec<Arc<dyn crate::extension::Extension>> = vec![
        Arc::new(core::CoreExtension::new(
            deps.spawn_server_cell,
            deps.backend_manager,
            deps.security,
        )),
        Arc::new(path_normalizer::PathNormalizer),
        Arc::new(security_warnings::SecurityWarnings),
        Arc::new(fs::FsExtension),
        Arc::new(system::SystemExtension),
        Arc::new(web::WebExtension::new(deps.web_search_backends)),
        Arc::new(memory::MemoryExtension::new(
            deps.session_registry,
            deps.agent_index.clone(),
            deps.embedder,
        )),
        Arc::new(heartbeat::HeartbeatExtension::new(deps.agent_index)),
    ];

    for ext in extensions {
        hub.register_extension(ext);
    }
}
