//! System-tool bundle — `get_time`, `calculate`, `describe_tool`.
//!
//! Small, dependency-free helpers grouped together so main.rs doesn't need
//! to know about them individually.

use crate::extension::caps::{CapabilityRequest, ExtensionCaps};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionHub, ExtensionRef, HookKind};
use crate::tools::{Calculate, DescribeTool, GetTime};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct SystemExtension;

impl Extension for SystemExtension {
    fn name(&self) -> &'static str {
        "system"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool]
    }

    fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
        hub.register_tool(Arc::new(GetTime));
        hub.register_tool(Arc::new(Calculate));
        hub.register_tool(Arc::new(DescribeTool));
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
                .ok_or_else(|| anyhow::anyhow!("system install requires ToolRegistration cap"))?;
            let tools: Vec<Arc<dyn crate::tool::Tool>> = vec![
                Arc::new(GetTime),
                Arc::new(Calculate),
                Arc::new(DescribeTool),
            ];
            for t in tools {
                let d = t.descriptor();
                tool_reg.register(d, t).await?;
            }
            Ok(InstalledExtension::empty())
        })
    }
}
