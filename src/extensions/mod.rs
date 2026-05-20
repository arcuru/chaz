//! Built-in chaz extensions.
//!
//! Each submodule defines one [`crate::extension::Extension`] implementation.
//! [`register_builtins`] wires them all up — call it from `main.rs` after
//! the [`ExtensionHub`] is constructed. Tools registered through the hub
//! are surfaced to the runtime by reading `hub.tools_for_registry()` and
//! pushing them into the legacy [`crate::tool::ToolRegistry`].

pub mod agent_schedule;
pub mod core;
pub mod fs;
pub mod mcp;
pub mod memory;
pub mod path_normalizer;
pub mod schedule;
pub mod security_warnings;
pub mod skills;
pub mod system;
pub mod web;

use crate::backends::BackendManager;
use crate::embedding::Embedder;
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
    pub memory_bank_index: HostedIndex,
    pub session_registry: Arc<SessionRegistry>,
    pub embedder: Option<Arc<dyn Embedder>>,
    pub web_search_backends: Vec<SearchBackend>,
    pub spawn_server_cell: Arc<OnceLock<Arc<Server>>>,
    pub backend_manager: BackendManager,
    pub security: SecurityContext,
}

/// Build the full built-in extension set as a vector. Consumed by
/// `ExtensionHub::install_all` (cap-based install path).
pub fn all_builtins(deps: BuiltinDeps) -> Vec<Arc<dyn crate::extension::Extension>> {
    let spawn_cell = deps.spawn_server_cell;
    let session_registry = deps.session_registry;
    vec![
        Arc::new(core::CoreExtension::new(
            spawn_cell.clone(),
            deps.backend_manager,
            deps.security,
        )),
        Arc::new(path_normalizer::PathNormalizer),
        Arc::new(security_warnings::SecurityWarnings),
        Arc::new(fs::FsExtension),
        Arc::new(system::SystemExtension),
        Arc::new(web::WebExtension::new(deps.web_search_backends)),
        Arc::new(memory::MemoryExtension::new(
            session_registry.clone(),
            deps.agent_index.clone(),
            deps.memory_bank_index.clone(),
            deps.embedder,
        )),
        Arc::new(schedule::ScheduleExtension::new()),
        Arc::new(skills::SkillsExtension::new()),
        Arc::new(agent_schedule::AgentScheduleExtension::new(spawn_cell)),
    ]
}
