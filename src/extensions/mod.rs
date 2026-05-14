//! Built-in chaz extensions.
//!
//! Each submodule defines one [`crate::extension::Extension`] implementation.
//! [`register_builtins`] wires them all up — call it from `main.rs` after
//! the [`ExtensionHub`] is constructed but before the [`ToolRegistry`] is
//! wrapped in `Arc`.

pub mod heartbeat;
pub mod path_normalizer;
pub mod security_warnings;

use crate::extension::ExtensionHub;
use crate::hosted_index::HostedIndex;
use crate::tool::ToolRegistry;
use std::sync::Arc;

/// Register every built-in extension. Order is registration order — hooks
/// fire in this sequence.
///
/// `agent_index` is shared with the heartbeat extension's tools and slash
/// command for resolving target-agent references.
pub fn register_builtins(
    hub: &mut ExtensionHub,
    registry: &mut ToolRegistry,
    agent_index: HostedIndex,
) {
    let extensions: Vec<Arc<dyn crate::extension::Extension>> = vec![
        Arc::new(path_normalizer::PathNormalizer),
        Arc::new(security_warnings::SecurityWarnings),
        Arc::new(heartbeat::HeartbeatExtension::new(agent_index)),
    ];

    for ext in extensions {
        ext.contribute_tools(registry);
        hub.register_extension(ext);
    }
}
