//! Web-tool bundle — `web_fetch`, `web_search`.
//!
//! `WebSearch` carries a `Vec<SearchBackend>` selected by config; the
//! extension takes the resolved list at construction time rather than
//! re-resolving it from config (which lives outside the extension surface).

use crate::extension::{Extension, ExtensionHub, HookKind};
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

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool]
    }

    fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
        hub.register_tool(Arc::new(WebFetch));
        hub.register_tool(Arc::new(WebSearch::new(self.search_backends.clone())));
    }
}
