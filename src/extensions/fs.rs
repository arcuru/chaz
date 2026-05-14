//! Filesystem-tool bundle — `read_file`, `write_file`, `edit_file`.
//!
//! Sibling to the `path_normalizer` hook extension: bundling these three
//! tools keeps the filesystem surface in one place and makes path-mutating
//! hooks easy to discover next to the tools they target.

use crate::extension::{Extension, ExtensionHub, HookKind};
use crate::tool::ToolRegistry;
use crate::tools::{EditFile, ReadFile, WriteFile};
use std::sync::Arc;

pub struct FsExtension;

impl Extension for FsExtension {
    fn name(&self) -> &'static str {
        "fs"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }

    fn register(self: Arc<Self>, _hub: &mut ExtensionHub) {}

    fn contribute_tools(&self, registry: &mut ToolRegistry) {
        registry.register(ReadFile);
        registry.register(WriteFile);
        registry.register(EditFile);
    }
}
