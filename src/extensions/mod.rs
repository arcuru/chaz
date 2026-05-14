//! Built-in chaz extensions.
//!
//! Each submodule defines one [`crate::extension::Extension`] implementation.
//! [`register_builtins`] wires them all up — call it from `main.rs` after
//! the [`ExtensionHub`] is constructed but before the [`ToolRegistry`] is
//! wrapped in `Arc`.

pub mod fs;
pub mod heartbeat;
pub mod memory;
pub mod path_normalizer;
pub mod security_warnings;
pub mod system;
pub mod web;

use crate::embedding::Embedder;
use crate::extension::ExtensionHub;
use crate::hosted_index::HostedIndex;
use crate::session::SessionRegistry;
use crate::tool::ToolRegistry;
use crate::tools::SearchBackend;
use std::sync::Arc;

/// Shared deps that several built-in extensions need at construction time.
/// Bundled into a struct so the `register_builtins` signature stays stable
/// as more extensions are added.
pub struct BuiltinDeps {
    pub agent_index: HostedIndex,
    pub session_registry: Arc<SessionRegistry>,
    pub embedder: Option<Arc<dyn Embedder>>,
    pub web_search_backends: Vec<SearchBackend>,
}

/// Register every built-in extension. Order is registration order — hooks
/// fire in this sequence.
pub fn register_builtins(
    hub: &mut ExtensionHub,
    tool_registry: &mut ToolRegistry,
    deps: BuiltinDeps,
) {
    let extensions: Vec<Arc<dyn crate::extension::Extension>> = vec![
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
        ext.contribute_tools(tool_registry);
        hub.register_extension(ext);
    }
}
