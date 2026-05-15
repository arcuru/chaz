//! Memory-tool bundle — `remember`, `recall`, `list_memory_banks`.
//!
//! All three share the same dependency set (session registry, hosted agent
//! index, optional embedder), so grouping them avoids passing those deps
//! three separate times in main.rs.

use crate::embedding::Embedder;
use crate::extension::caps::{CapabilityRequest, ExtensionCaps};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionHub, ExtensionRef, HookKind};
use crate::hosted_index::HostedIndex;
use crate::session::SessionRegistry;
use crate::tools::{ListMemoryBanks, Recall, Remember};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct MemoryExtension {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    embedder: Option<Arc<dyn Embedder>>,
}

impl MemoryExtension {
    pub fn new(
        registry: Arc<SessionRegistry>,
        agent_index: HostedIndex,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Self {
        Self {
            registry,
            agent_index,
            embedder,
        }
    }
}

impl Extension for MemoryExtension {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool]
    }

    fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
        hub.register_tool(Arc::new(Remember::new(
            self.registry.clone(),
            self.agent_index.clone(),
            self.embedder.clone(),
        )));
        hub.register_tool(Arc::new(Recall::new(
            self.registry.clone(),
            self.agent_index.clone(),
            self.embedder.clone(),
        )));
        hub.register_tool(Arc::new(ListMemoryBanks::new(
            self.registry.clone(),
            self.agent_index.clone(),
        )));
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
                .ok_or_else(|| anyhow::anyhow!("memory install requires ToolRegistration cap"))?;
            let tools: Vec<Arc<dyn crate::tool::Tool>> = vec![
                Arc::new(Remember::new(
                    self.registry.clone(),
                    self.agent_index.clone(),
                    self.embedder.clone(),
                )),
                Arc::new(Recall::new(
                    self.registry.clone(),
                    self.agent_index.clone(),
                    self.embedder.clone(),
                )),
                Arc::new(ListMemoryBanks::new(
                    self.registry.clone(),
                    self.agent_index.clone(),
                )),
            ];
            for t in tools {
                let d = t.descriptor();
                tool_reg.register(d, t).await?;
            }
            Ok(InstalledExtension::empty())
        })
    }
}
