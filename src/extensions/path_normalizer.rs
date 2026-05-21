//! Strip trailing slashes from `path` arguments on filesystem tool calls.
//!
//! Ported from pi's `path-normalizer.ts`. Some LLMs occasionally emit
//! paths with a trailing slash (`/etc/hosts/`) which the filesystem layer
//! treats as a directory and rejects. Canonicalizing the arg before
//! execution avoids the round-trip through an error message.

use crate::extension::caps::ExtensionCaps;
use crate::extension::handler::{HandlerFuture, HookHandlerToolCall};
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind, ToolCallDecision};
use std::sync::Arc;

pub struct PathNormalizer;

impl Extension for PathNormalizer {
    fn name(&self) -> &'static str {
        "path_normalizer"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::ToolCall]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![HookKind::ToolCall],
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn instantiate<'a>(&'a self, _scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        Box::pin(async move {
            Ok(Arc::new(PathNormalizerInstance { manifest }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct PathNormalizerInstance {
    manifest: ExtensionManifest,
}

impl ExtensionInstance for PathNormalizerInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn tool_call_hook(&self) -> Option<Arc<dyn HookHandlerToolCall>> {
        Some(Arc::new(PathNormalizerCapHook))
    }
}

struct PathNormalizerCapHook;

impl HookHandlerToolCall for PathNormalizerCapHook {
    fn on_tool_call<'a>(
        &'a self,
        _caps: &'a ExtensionCaps,
        tool_name: &'a str,
        args: &'a mut serde_json::Value,
    ) -> HandlerFuture<'a, ToolCallDecision> {
        Box::pin(async move { normalize_args(tool_name, args) })
    }
}

fn normalize_args(tool_name: &str, args: &mut serde_json::Value) -> ToolCallDecision {
    if !matches!(tool_name, "read_file" | "write_file" | "edit_file") {
        return ToolCallDecision::Continue;
    }
    if let Some(obj) = args.as_object_mut()
        && let Some(path_val) = obj.get_mut("path")
        && let Some(path_str) = path_val.as_str()
        && path_str != "/"
        && path_str.ends_with('/')
    {
        let trimmed = path_str.trim_end_matches('/').to_string();
        *path_val = serde_json::Value::String(trimmed);
    }
    ToolCallDecision::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn strips_trailing_slash_on_read_file() {
        let caps = ExtensionCaps::empty();
        let hook = PathNormalizerCapHook;
        let mut args = serde_json::json!({"path": "/etc/hosts/"});
        let decision = hook.on_tool_call(&caps, "read_file", &mut args).await;
        assert!(matches!(decision, ToolCallDecision::Continue));
        assert_eq!(
            args.get("path").and_then(|v| v.as_str()),
            Some("/etc/hosts")
        );
    }

    #[tokio::test]
    async fn leaves_root_path_alone() {
        let caps = ExtensionCaps::empty();
        let hook = PathNormalizerCapHook;
        let mut args = serde_json::json!({"path": "/"});
        let _ = hook.on_tool_call(&caps, "read_file", &mut args).await;
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("/"));
    }

    #[tokio::test]
    async fn ignores_non_matching_tool() {
        let caps = ExtensionCaps::empty();
        let hook = PathNormalizerCapHook;
        let mut args = serde_json::json!({"path": "/etc/hosts/"});
        let _ = hook.on_tool_call(&caps, "shell", &mut args).await;
        assert_eq!(
            args.get("path").and_then(|v| v.as_str()),
            Some("/etc/hosts/")
        );
    }

    #[tokio::test]
    async fn ignores_missing_path() {
        let caps = ExtensionCaps::empty();
        let hook = PathNormalizerCapHook;
        let mut args = serde_json::json!({});
        let _ = hook.on_tool_call(&caps, "read_file", &mut args).await;
        assert!(args.get("path").is_none());
    }
}
