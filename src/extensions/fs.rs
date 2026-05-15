//! Filesystem-tool bundle — `read_file`, `write_file`, `edit_file`.
//!
//! Sibling to the `path_normalizer` hook extension: bundling these three
//! tools keeps the filesystem surface in one place and makes path-mutating
//! hooks easy to discover next to the tools they target.

use crate::extension::caps::{CapabilityRequest, ExtensionCaps};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::tools::{EditFile, ReadFile, WriteFile};
use std::future::Future;
use std::pin::Pin;
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
            required_capabilities: vec![CapabilityRequest::ToolRegistration],
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn install<'a>(
        &'a self,
        caps: ExtensionCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<InstalledExtension>> + Send + 'a>> {
        Box::pin(async move {
            let tool_reg = caps
                .tool_registration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("fs install requires ToolRegistration cap"))?;
            let tools: Vec<Arc<dyn crate::tool::Tool>> =
                vec![Arc::new(ReadFile), Arc::new(WriteFile), Arc::new(EditFile)];
            for t in tools {
                let d = t.descriptor();
                tool_reg.register(d, t).await?;
            }
            Ok(InstalledExtension::empty())
        })
    }
}
