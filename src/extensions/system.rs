//! System-tool bundle — `get_time`, `calculate`, `describe_tool`.
//!
//! Small, dependency-free helpers grouped together so main.rs doesn't need
//! to know about them individually.

use crate::extension::{Extension, ExtensionHub, HookKind};
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

    fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
        hub.register_tool(Arc::new(GetTime));
        hub.register_tool(Arc::new(Calculate));
        hub.register_tool(Arc::new(DescribeTool));
    }
}
