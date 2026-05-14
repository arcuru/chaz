//! Memory-tool bundle — `remember`, `recall`, `list_memory_banks`.
//!
//! All three share the same dependency set (session registry, hosted agent
//! index, optional embedder), so grouping them avoids passing those deps
//! three separate times in main.rs.

use crate::embedding::Embedder;
use crate::extension::{Extension, ExtensionHub, HookKind};
use crate::hosted_index::HostedIndex;
use crate::session::SessionRegistry;
use crate::tools::{ListMemoryBanks, Recall, Remember};
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
}
