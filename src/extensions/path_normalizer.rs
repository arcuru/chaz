//! Strip trailing slashes from `path` arguments on filesystem tool calls.
//!
//! Ported from pi's `path-normalizer.ts`. Some LLMs occasionally emit
//! paths with a trailing slash (`/etc/hosts/`) which the filesystem layer
//! treats as a directory and rejects. Canonicalizing the arg before
//! execution avoids the round-trip through an error message.

use crate::extension::{
    Extension, ExtensionHub, HookContext, HookKind, HookToolCall, ToolCallDecision,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct PathNormalizer;

impl Extension for PathNormalizer {
    fn name(&self) -> &'static str {
        "path_normalizer"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::ToolCall]
    }

    fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
        hub.on_tool_call(Box::new(PathNormalizerHook));
    }
}

struct PathNormalizerHook;

impl HookToolCall for PathNormalizerHook {
    fn on_tool_call<'a>(
        &'a self,
        _ctx: &'a HookContext,
        tool_name: &'a str,
        args: &'a mut serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>> {
        Box::pin(async move {
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::HookContext;
    use crate::session::Session;
    use crate::types::ConversationId;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn make_ctx() -> HookContext {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = Doc::new();
        s.set("name", "session");
        let db = user.create_database(s, &key).await.unwrap();
        let session = Session::new(ConversationId("conv".into()), db).await;
        HookContext {
            agent_name: "test".into(),
            model: None,
            call_depth: 0,
            session: Arc::new(Mutex::new(session)),
        }
    }

    #[tokio::test]
    async fn strips_trailing_slash_on_read_file() {
        let hook = PathNormalizerHook;
        let ctx = make_ctx().await;
        let mut args = serde_json::json!({"path": "/etc/hosts/"});
        let decision = hook.on_tool_call(&ctx, "read_file", &mut args).await;
        assert!(matches!(decision, ToolCallDecision::Continue));
        assert_eq!(
            args.get("path").and_then(|v| v.as_str()),
            Some("/etc/hosts")
        );
    }

    #[tokio::test]
    async fn leaves_root_path_alone() {
        let hook = PathNormalizerHook;
        let ctx = make_ctx().await;
        let mut args = serde_json::json!({"path": "/"});
        let _ = hook.on_tool_call(&ctx, "read_file", &mut args).await;
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("/"));
    }

    #[tokio::test]
    async fn ignores_non_matching_tool() {
        let hook = PathNormalizerHook;
        let ctx = make_ctx().await;
        let mut args = serde_json::json!({"path": "/etc/hosts/"});
        let _ = hook.on_tool_call(&ctx, "shell", &mut args).await;
        assert_eq!(
            args.get("path").and_then(|v| v.as_str()),
            Some("/etc/hosts/")
        );
    }

    #[tokio::test]
    async fn ignores_missing_path() {
        let hook = PathNormalizerHook;
        let ctx = make_ctx().await;
        let mut args = serde_json::json!({});
        let _ = hook.on_tool_call(&ctx, "read_file", &mut args).await;
        assert!(args.get("path").is_none());
    }
}
