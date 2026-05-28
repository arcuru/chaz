//! Web-tool bundle — `web_fetch`, `web_search`.
//!
//! `WebSearch` carries a `Vec<SearchBackend>` selected by config; the
//! extension takes the resolved list at construction time rather than
//! re-resolving it from config (which lives outside the extension surface).

use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
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

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![HookKind::Tool],
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn instantiate<'a>(&'a self, _scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        let search_backends = self.search_backends.clone();
        Box::pin(async move {
            Ok(Arc::new(WebInstance {
                manifest,
                search_backends,
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct WebInstance {
    manifest: ExtensionManifest,
    search_backends: Vec<SearchBackend>,
}

impl ExtensionInstance for WebInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn tools(&self) -> Vec<Arc<dyn crate::tool::Tool>> {
        vec![
            Arc::new(WebFetch),
            Arc::new(WebSearch::new(self.search_backends.clone())),
        ]
    }
}
