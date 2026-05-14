//! Filesystem-tool bundle — `read_file`, `write_file`, `edit_file`.
//!
//! Sibling to the `path_normalizer` hook extension: bundling these three
//! tools keeps the filesystem surface in one place and makes path-mutating
//! hooks easy to discover next to the tools they target.

use crate::extension::{Extension, ExtensionHub, HookKind};
use crate::tools::{EditFile, ReadFile, WriteFile};
use std::sync::Arc;

pub struct FsExtension;

impl Extension for FsExtension {
    fn name(&self) -> &'static str {
        "fs"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool]
    }

    fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
        hub.register_tool(Arc::new(ReadFile));
        hub.register_tool(Arc::new(WriteFile));
        hub.register_tool(Arc::new(EditFile));
    }
}
