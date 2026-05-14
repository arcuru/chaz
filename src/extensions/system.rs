//! System-tool bundle — `get_time`, `calculate`, `describe_tool`.
//!
//! Small, dependency-free helpers grouped together so main.rs doesn't need
//! to know about them individually.

use crate::extension::{Extension, ExtensionHub};
use crate::tool::ToolRegistry;
use crate::tools::{Calculate, DescribeTool, GetTime};
use std::sync::Arc;

pub struct SystemExtension;

impl Extension for SystemExtension {
    fn name(&self) -> &'static str {
        "system"
    }

    fn register(self: Arc<Self>, _hub: &mut ExtensionHub) {}

    fn contribute_tools(&self, registry: &mut ToolRegistry) {
        registry.register(GetTime);
        registry.register(Calculate);
        registry.register(DescribeTool);
    }
}
