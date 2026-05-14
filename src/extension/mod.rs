//! Extension/hook framework for chaz.
//!
//! Extensions are compile-time Rust types registered with an [`ExtensionHub`]
//! during startup. They can:
//! - Contribute tools to the [`ToolRegistry`] before it is shared.
//! - Install hook handlers that fire at well-known points in the agent loop
//!   (`before_agent_start`, `tool_call`, `tool_result`, `agent_end`,
//!   `session_start`, `session_shutdown`).
//! - Register slash commands.
//!
//! Modeled after pi's `ExtensionAPI` (TypeScript). Hot reload / WASM-loaded
//! extensions are deliberately out of scope for v1 — see `extension_api_version`.
//!
//! Panic safety: hook impls must not panic. A panic in a hook will propagate
//! through the agent turn. (TODO: add `catch_unwind` isolation once the
//! `futures` crate is in the tree.)

pub mod hooks;

use crate::runtime::RuntimeMessage;
use crate::session::Session;
use crate::tool::ToolRegistry;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

pub use hooks::{
    HookAgentEnd, HookBeforeAgentStart, HookSessionShutdown, HookSessionStart, HookToolCall,
    HookToolResult,
};

/// Decision returned from a `tool_call` hook.
#[derive(Debug)]
pub enum ToolCallDecision {
    /// Continue execution. Args may have been mutated in place.
    Continue,
    /// Skip the tool call; synthesize a result with this reason.
    Block { reason: String },
}

/// Lightweight context handed to hook implementations.
///
/// Deliberately narrower than `ToolContext` — extensions should not be
/// mutating tool scopes, grants, or hosts. Session access is exposed so
/// extensions can read history or append entries via the existing
/// `Session` API.
pub struct HookContext {
    pub agent_name: String,
    pub model: Option<String>,
    pub call_depth: usize,
    pub session: Arc<Mutex<Session>>,
}

/// Outcome of an extension-registered slash command.
///
/// Mirrors `commands::CommandOutcome::Text`/`Error` — extensions can't
/// produce session switches or session lists, which are gateway-coupled.
pub enum ExtensionCommandOutcome {
    Text(String),
    Error(String),
}

/// Handler for a slash command registered by an extension.
pub trait ExtensionCommand: Send + Sync {
    fn description(&self) -> &'static str;

    /// Invoke the command. `args` is everything after the command name,
    /// trimmed. `ctx` carries the same session/agent info as a hook.
    fn invoke<'a>(
        &'a self,
        args: &'a str,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>>;
}

/// An extension is a compile-time Rust type that wires hooks, tools, and
/// commands into the agent runtime. Implementations are registered in
/// `src/extensions/mod.rs::register_builtins`.
pub trait Extension: Send + Sync {
    fn name(&self) -> &'static str;

    /// Wire hooks and commands. Called once at startup.
    fn register(self: Arc<Self>, hub: &mut ExtensionHub);

    /// Contribute tools to the registry. Called before the registry is
    /// wrapped in `Arc`, so tools land in `ScopedTools`/`ToolProfile`
    /// filtering automatically.
    fn contribute_tools(&self, _registry: &mut ToolRegistry) {}

    /// Reserved for a future hot-reload / WASM-loaded surface.
    fn extension_api_version(&self) -> u32 {
        1
    }
}

/// Central registry for hook handlers, extension commands, and the
/// extensions themselves. Held on `Server` as `Arc<ExtensionHub>`.
pub struct ExtensionHub {
    extensions: Vec<Arc<dyn Extension>>,
    before_agent_start: Vec<Box<dyn HookBeforeAgentStart>>,
    tool_call: Vec<Box<dyn HookToolCall>>,
    tool_result: Vec<Box<dyn HookToolResult>>,
    agent_end: Vec<Box<dyn HookAgentEnd>>,
    session_start: Vec<Box<dyn HookSessionStart>>,
    session_shutdown: Vec<Box<dyn HookSessionShutdown>>,
    commands: HashMap<String, Box<dyn ExtensionCommand>>,
    /// Names reserved by built-in slash commands; extensions cannot register
    /// these. Populated by [`ExtensionHub::reserve_builtin_commands`] during
    /// hub construction.
    reserved_command_names: std::collections::HashSet<String>,
}

impl Default for ExtensionHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionHub {
    pub fn new() -> Self {
        Self {
            extensions: Vec::new(),
            before_agent_start: Vec::new(),
            tool_call: Vec::new(),
            tool_result: Vec::new(),
            agent_end: Vec::new(),
            session_start: Vec::new(),
            session_shutdown: Vec::new(),
            commands: HashMap::new(),
            reserved_command_names: std::collections::HashSet::new(),
        }
    }

    /// Reserve built-in slash command names so extensions can't shadow them.
    pub fn reserve_builtin_commands<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.reserved_command_names
            .extend(names.into_iter().map(Into::into));
    }

    pub fn register_extension(&mut self, ext: Arc<dyn Extension>) {
        ext.clone().register(self);
        self.extensions.push(ext);
    }

    pub fn extension_names(&self) -> Vec<&'static str> {
        self.extensions.iter().map(|e| e.name()).collect()
    }

    // --- registration API used inside Extension::register ---

    pub fn on_before_agent_start(&mut self, hook: Box<dyn HookBeforeAgentStart>) {
        self.before_agent_start.push(hook);
    }

    pub fn on_tool_call(&mut self, hook: Box<dyn HookToolCall>) {
        self.tool_call.push(hook);
    }

    pub fn on_tool_result(&mut self, hook: Box<dyn HookToolResult>) {
        self.tool_result.push(hook);
    }

    pub fn on_agent_end(&mut self, hook: Box<dyn HookAgentEnd>) {
        self.agent_end.push(hook);
    }

    pub fn on_session_start(&mut self, hook: Box<dyn HookSessionStart>) {
        self.session_start.push(hook);
    }

    pub fn on_session_shutdown(&mut self, hook: Box<dyn HookSessionShutdown>) {
        self.session_shutdown.push(hook);
    }

    /// Register an extension slash command.
    ///
    /// Names colliding with a built-in or an already-registered extension
    /// command are rejected with a warning. First registration wins on
    /// cross-extension collision.
    pub fn register_command<S: Into<String>>(
        &mut self,
        name: S,
        handler: Box<dyn ExtensionCommand>,
    ) {
        let name = name.into();
        if self.reserved_command_names.contains(&name) {
            warn!(
                command = %name,
                "Extension command shadows a built-in; ignoring registration"
            );
            return;
        }
        if self.commands.contains_key(&name) {
            warn!(
                command = %name,
                "Duplicate extension command registration; keeping first registration"
            );
            return;
        }
        self.commands.insert(name, handler);
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    pub fn list_commands(&self) -> Vec<(&str, &'static str)> {
        self.commands
            .iter()
            .map(|(name, handler)| (name.as_str(), handler.description()))
            .collect()
    }

    // --- hook dispatch ---

    /// Fire `before_agent_start` for every registered handler. Each
    /// handler may append messages, which are flattened into a single
    /// vector preserving registration order.
    pub async fn fire_before_agent_start(&self, ctx: &HookContext) -> Vec<RuntimeMessage> {
        let mut out = Vec::new();
        for hook in &self.before_agent_start {
            out.extend(hook.on_before_agent_start(ctx).await);
        }
        out
    }

    /// Fire `tool_call` for every registered handler. Args are mutated in
    /// place. First `Block` short-circuits the rest.
    pub async fn fire_tool_call(
        &self,
        ctx: &HookContext,
        tool_name: &str,
        args: &mut serde_json::Value,
    ) -> ToolCallDecision {
        for hook in &self.tool_call {
            match hook.on_tool_call(ctx, tool_name, args).await {
                ToolCallDecision::Continue => {}
                ToolCallDecision::Block { reason } => return ToolCallDecision::Block { reason },
            }
        }
        ToolCallDecision::Continue
    }

    /// Fire `tool_result`. Handlers are run in registration order; each
    /// receives the (possibly transformed) result from the previous.
    pub async fn fire_tool_result(
        &self,
        ctx: &HookContext,
        tool_name: &str,
        result: String,
    ) -> String {
        let mut acc = result;
        for hook in &self.tool_result {
            acc = hook.on_tool_result(ctx, tool_name, acc).await;
        }
        acc
    }

    pub async fn fire_agent_end(&self, ctx: &HookContext) {
        for hook in &self.agent_end {
            hook.on_agent_end(ctx).await;
        }
    }

    pub async fn fire_session_start(&self, ctx: &HookContext) {
        for hook in &self.session_start {
            hook.on_session_start(ctx).await;
        }
    }

    pub async fn fire_session_shutdown(&self, ctx: &HookContext) {
        for hook in &self.session_shutdown {
            hook.on_session_shutdown(ctx).await;
        }
    }

    /// Look up and invoke an extension command by name. Returns `None`
    /// if no extension registered this name.
    pub async fn try_dispatch_command(
        &self,
        name: &str,
        args: &str,
        ctx: &HookContext,
    ) -> Option<ExtensionCommandOutcome> {
        let handler = self.commands.get(name)?;
        Some(handler.invoke(args, ctx).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeMessage;
    use crate::types::ConversationId;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;

    async fn fixture_ctx() -> HookContext {
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
            agent_name: "test_agent".into(),
            model: None,
            call_depth: 0,
            session: Arc::new(Mutex::new(session)),
        }
    }

    struct CountingHook {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl HookBeforeAgentStart for CountingHook {
        fn on_before_agent_start<'a>(
            &'a self,
            _ctx: &'a HookContext,
        ) -> Pin<Box<dyn Future<Output = Vec<RuntimeMessage>> + Send + 'a>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![RuntimeMessage::System("injected".into())]
            })
        }
    }

    #[tokio::test]
    async fn before_agent_start_runs_in_registration_order() {
        let mut hub = ExtensionHub::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        hub.on_before_agent_start(Box::new(CountingHook {
            calls: calls.clone(),
        }));
        hub.on_before_agent_start(Box::new(CountingHook {
            calls: calls.clone(),
        }));
        let ctx = fixture_ctx().await;
        let injected = hub.fire_before_agent_start(&ctx).await;
        assert_eq!(injected.len(), 2);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    struct BlockingHook;
    impl HookToolCall for BlockingHook {
        fn on_tool_call<'a>(
            &'a self,
            _ctx: &'a HookContext,
            name: &'a str,
            _args: &'a mut serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>> {
            Box::pin(async move {
                if name == "shell" {
                    ToolCallDecision::Block {
                        reason: "blocked by test".into(),
                    }
                } else {
                    ToolCallDecision::Continue
                }
            })
        }
    }

    struct MutatingHook;
    impl HookToolCall for MutatingHook {
        fn on_tool_call<'a>(
            &'a self,
            _ctx: &'a HookContext,
            _name: &'a str,
            args: &'a mut serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>> {
            Box::pin(async move {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("touched".into(), serde_json::Value::Bool(true));
                }
                ToolCallDecision::Continue
            })
        }
    }

    #[tokio::test]
    async fn tool_call_block_short_circuits_and_mutation_propagates() {
        let mut hub = ExtensionHub::new();
        hub.on_tool_call(Box::new(MutatingHook));
        hub.on_tool_call(Box::new(BlockingHook));
        let ctx = fixture_ctx().await;

        let mut args = serde_json::json!({});
        let decision = hub.fire_tool_call(&ctx, "read_file", &mut args).await;
        assert!(matches!(decision, ToolCallDecision::Continue));
        assert_eq!(args.get("touched").and_then(|v| v.as_bool()), Some(true));

        let mut args2 = serde_json::json!({});
        let decision2 = hub.fire_tool_call(&ctx, "shell", &mut args2).await;
        assert!(matches!(
            decision2,
            ToolCallDecision::Block { ref reason } if reason == "blocked by test"
        ));
    }

    struct DummyCmd;
    impl ExtensionCommand for DummyCmd {
        fn description(&self) -> &'static str {
            "test command"
        }
        fn invoke<'a>(
            &'a self,
            args: &'a str,
            _ctx: &'a HookContext,
        ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
            Box::pin(async move { ExtensionCommandOutcome::Text(format!("got: {args}")) })
        }
    }

    #[tokio::test]
    async fn command_collision_with_builtin_is_rejected() {
        let mut hub = ExtensionHub::new();
        hub.reserve_builtin_commands(["info"]);
        hub.register_command("info", Box::new(DummyCmd));
        assert!(!hub.has_command("info"));
    }

    #[tokio::test]
    async fn duplicate_extension_command_keeps_first() {
        let mut hub = ExtensionHub::new();
        hub.register_command("greet", Box::new(DummyCmd));
        struct OtherCmd;
        impl ExtensionCommand for OtherCmd {
            fn description(&self) -> &'static str {
                "other"
            }
            fn invoke<'a>(
                &'a self,
                _args: &'a str,
                _ctx: &'a HookContext,
            ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
                Box::pin(async move { ExtensionCommandOutcome::Text("other".into()) })
            }
        }
        hub.register_command("greet", Box::new(OtherCmd));
        let ctx = fixture_ctx().await;
        let out = hub
            .try_dispatch_command("greet", "x", &ctx)
            .await
            .expect("command registered");
        match out {
            ExtensionCommandOutcome::Text(s) => assert_eq!(s, "got: x"),
            _ => panic!("expected text outcome"),
        }
    }
}
