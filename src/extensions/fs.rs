//! Filesystem-tool bundle — `read_file`, `write_file`, `edit_file`.
//!
//! Sibling to the `path_normalizer` hook extension: bundling these three
//! tools keeps the filesystem surface in one place and makes path-mutating
//! hooks easy to discover next to the tools they target.

use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
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
        Box::pin(async move {
            Ok(Arc::new(FsInstance { manifest }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct FsInstance {
    manifest: ExtensionManifest,
}

impl ExtensionInstance for FsInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn tools(&self) -> Vec<Arc<dyn crate::tool::Tool>> {
        vec![Arc::new(ReadFile), Arc::new(WriteFile), Arc::new(EditFile)]
    }
}
