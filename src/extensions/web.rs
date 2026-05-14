//! Web-tool bundle — `web_fetch`, `web_search`.
//!
//! `WebSearch` carries a `Vec<SearchBackend>` selected by config; the
//! extension takes the resolved list at construction time rather than
//! re-resolving it from config (which lives outside the extension surface).

use crate::extension::{Extension, ExtensionHub};
use crate::tool::ToolRegistry;
use crate::tools::{SearchBackend, WebFetch, WebSearch};
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

    fn register(self: Arc<Self>, _hub: &mut ExtensionHub) {}

    fn contribute_tools(&self, registry: &mut ToolRegistry) {
        registry.register(WebFetch);
        registry.register(WebSearch::new(self.search_backends.clone()));
    }
}
