//! Memory-tool bundle — `remember`, `recall`, `list_memory_banks`.
//!
//! All three share the same dependency set (session registry, hosted agent
//! index, optional embedder), so grouping them avoids passing those deps
//! three separate times in main.rs.

use crate::embedding::Embedder;
use crate::extension::{Extension, ExtensionHub};
use crate::hosted_index::HostedIndex;
use crate::session::SessionRegistry;
use crate::tool::ToolRegistry;
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

    fn register(self: Arc<Self>, _hub: &mut ExtensionHub) {}

    fn contribute_tools(&self, registry: &mut ToolRegistry) {
        registry.register(Remember::new(
            self.registry.clone(),
            self.agent_index.clone(),
            self.embedder.clone(),
        ));
        registry.register(Recall::new(
            self.registry.clone(),
            self.agent_index.clone(),
            self.embedder.clone(),
        ));
        registry.register(ListMemoryBanks::new(
            self.registry.clone(),
            self.agent_index.clone(),
        ));
    }
}
