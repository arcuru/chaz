//! Built-in chaz extensions.
//!
//! Each submodule defines one [`crate::extension::Extension`] implementation.
//! [`register_builtins`] wires them all up — call it from `main.rs` after
//! the [`ExtensionHub`] is constructed but before the [`ToolRegistry`] is
//! wrapped in `Arc`.

pub mod path_normalizer;
pub mod security_warnings;

use crate::extension::ExtensionHub;
use crate::tool::ToolRegistry;
use std::sync::Arc;

/// Register every built-in extension. Order is registration order — hooks
/// fire in this sequence.
pub fn register_builtins(hub: &mut ExtensionHub, registry: &mut ToolRegistry) {
    let extensions: Vec<Arc<dyn crate::extension::Extension>> = vec![
        Arc::new(path_normalizer::PathNormalizer),
        Arc::new(security_warnings::SecurityWarnings),
    ];

    for ext in extensions {
        ext.contribute_tools(registry);
        hub.register_extension(ext);
    }
}
