//! Web-tool bundle — `web_fetch`, `web_search`.
//!
//! `WebSearch` carries a `Vec<SearchBackend>` selected by config; the
//! extension takes the resolved list at construction time rather than
//! re-resolving it from config (which lives outside the extension surface).

use crate::extension::caps::{CapabilityRequest, ExtensionCaps};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::tools::{SearchBackend, WebFetch, WebSearch};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct WebExtension {
    search_backends: Vec<SearchBackend>,
}

impl WebExtension {
    pub fn new(search_backends: Vec<SearchBackend>) -> Self {
        Self { search_backends }
    }
}

impl Extension for WebExtension {
    fn name(&self) -> &'static str {
        "web"
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
                .ok_or_else(|| anyhow::anyhow!("web install requires ToolRegistration cap"))?;
            let tools: Vec<Arc<dyn crate::tool::Tool>> = vec![
                Arc::new(WebFetch),
                Arc::new(WebSearch::new(self.search_backends.clone())),
            ];
            for t in tools {
                let d = t.descriptor();
                tool_reg.register(d, t).await?;
            }
            Ok(InstalledExtension::empty())
        })
    }
}
