//! System-tool bundle — `get_time`, `calculate`, `describe_tool`.
//!
//! Small, dependency-free helpers grouped together so main.rs doesn't need
//! to know about them individually.

use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::tools::{Calculate, DescribeTool, GetTime};
use std::sync::Arc;

pub struct SystemExtension;

impl Extension for SystemExtension {
    fn name(&self) -> &'static str {
        "system"
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
            Ok(Arc::new(SystemInstance { manifest }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct SystemInstance {
    manifest: ExtensionManifest,
}

impl ExtensionInstance for SystemInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn tools(&self) -> Vec<Arc<dyn crate::tool::Tool>> {
        vec![
            Arc::new(GetTime),
            Arc::new(Calculate),
            Arc::new(DescribeTool),
        ]
    }
}
