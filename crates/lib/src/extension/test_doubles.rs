//! Extension builders and endpoints shared by the framework tests.
//!
//! `TestExt` publishes the configured handlers at each requested scope.
//! Scenario-specific behavior stays beside its tests.

use super::context_strategy::ContextStrategy;
use super::durable_context::DurableContextContributor;
use super::*;
use crate::runtime::RuntimeMessage;
use crate::test_support::{fresh_eidetica, fresh_session_registry};
use crate::tool::ToolRegistry;
use crate::types::ConversationId;
use eidetica::Instance;
use eidetica::crdt::Doc;

/// Minimal extension: a name and nothing else. The default
/// `instantiate` yields a no-op instance, so it registers nothing
/// through the drain — useful for registration/event-log tests that
/// only care that an extension exists.
pub(super) struct NamedExt(pub(super) &'static str);
impl Extension for NamedExt {
    fn name(&self) -> &'static str {
        self.0
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
}

#[derive(Default, Clone)]
pub(super) struct TestParts {
    pub(super) tool_call: Option<Arc<dyn handler::HookHandlerToolCall>>,
    pub(super) before_agent_start: Option<Arc<dyn handler::HookHandlerBeforeAgentStart>>,
    pub(super) tool_result: Option<Arc<dyn handler::HookHandlerToolResult>>,
    pub(super) routine_handler: Option<Arc<dyn handler::RoutineHandler>>,
    pub(super) prompt_augmentation: Option<Arc<dyn caps::PromptAugmentation>>,
    pub(super) context_tail: Option<Arc<dyn caps::ContextTail>>,
    pub(super) context_strategy: Option<Arc<dyn ContextStrategy>>,
    pub(super) durable_context: Option<Arc<dyn DurableContextContributor>>,
    pub(super) messenger: Option<Arc<dyn caps::Messenger>>,
    pub(super) status_segment: Option<Arc<dyn caps::StatusSegment>>,
    pub(super) tools: Vec<Arc<dyn Tool>>,
    pub(super) commands: Vec<(String, Arc<dyn ExtensionCommand>)>,
}

pub(super) struct TestExt {
    name: &'static str,
    supported: Vec<HookKind>,
    scopes: Vec<instance::Scope>,
    parts: TestParts,
    /// Context capabilities are declared by default; override for undeclared-endpoint tests.
    provides: Option<Vec<caps::CapabilityKind>>,
    instantiations: Option<Arc<std::sync::atomic::AtomicUsize>>,
    fail_instantiate: bool,
    panic_instantiate: bool,
}

impl TestExt {
    pub(super) fn new(name: &'static str) -> Self {
        Self {
            name,
            supported: Vec::new(),
            scopes: vec![instance::Scope::Global],
            parts: TestParts::default(),
            provides: None,
            instantiations: None,
            fail_instantiate: false,
            panic_instantiate: false,
        }
    }
    /// Bump `counter` on every `instantiate` call, so tests can assert
    /// how many instances the hub actually built.
    pub(super) fn instantiation_counter(
        mut self,
        counter: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        self.instantiations = Some(counter);
        self
    }
    /// Fail `instantiate` *after* the parts are configured, so tests
    /// can assert that the parts a successful instantiation would have
    /// published never reach the hub's registries.
    pub(super) fn fails_instantiate(mut self) -> Self {
        self.fail_instantiate = true;
        self
    }
    /// Panic inside `instantiate` (not just an Err return) so the
    /// `catch_unwind` path in `install_all` is exercised.
    pub(super) fn panics_instantiate(mut self) -> Self {
        self.panic_instantiate = true;
        self
    }
    pub(super) fn scopes(mut self, scopes: Vec<instance::Scope>) -> Self {
        self.scopes = scopes;
        self
    }
    /// Override `manifest().provides_capabilities` outright. Pass
    /// `vec![]` to model an extension that publishes an endpoint
    /// without declaring the capability.
    pub(super) fn provides_capabilities(mut self, kinds: Vec<caps::CapabilityKind>) -> Self {
        self.provides = Some(kinds);
        self
    }
    pub(super) fn prompt_augmentation(mut self, p: Arc<dyn caps::PromptAugmentation>) -> Self {
        self.parts.prompt_augmentation = Some(p);
        self
    }
    pub(super) fn context_tail(mut self, tail: Arc<dyn caps::ContextTail>) -> Self {
        self.parts.context_tail = Some(tail);
        self
    }
    pub(super) fn context_strategy(mut self, strategy: Arc<dyn ContextStrategy>) -> Self {
        self.parts.context_strategy = Some(strategy);
        self
    }
    pub(super) fn durable_context(
        mut self,
        contributor: Arc<dyn DurableContextContributor>,
    ) -> Self {
        self.parts.durable_context = Some(contributor);
        self
    }
    pub(super) fn messenger(mut self, messenger: Arc<dyn caps::Messenger>) -> Self {
        self.parts.messenger = Some(messenger);
        self
    }
    pub(super) fn status_segment(mut self, s: Arc<dyn caps::StatusSegment>) -> Self {
        self.parts.status_segment = Some(s);
        self
    }
    pub(super) fn tool_call(mut self, h: Arc<dyn handler::HookHandlerToolCall>) -> Self {
        self.supported.push(HookKind::ToolCall);
        self.parts.tool_call = Some(h);
        self
    }
    /// Publish a tool through the instance drain. Tools count as the
    /// `Tool` hook kind in the declaration.
    pub(super) fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.supported.push(HookKind::Tool);
        self.parts.tools.push(tool);
        self
    }
    pub(super) fn tool_result(mut self, h: Arc<dyn handler::HookHandlerToolResult>) -> Self {
        self.supported.push(HookKind::ToolResult);
        self.parts.tool_result = Some(h);
        self
    }
    pub(super) fn before_agent_start(
        mut self,
        h: Arc<dyn handler::HookHandlerBeforeAgentStart>,
    ) -> Self {
        self.supported.push(HookKind::BeforeAgentStart);
        self.parts.before_agent_start = Some(h);
        self
    }
    pub(super) fn routine_handler(mut self, h: Arc<dyn handler::RoutineHandler>) -> Self {
        self.parts.routine_handler = Some(h);
        self
    }
    pub(super) fn command(mut self, name: &str, h: Arc<dyn ExtensionCommand>) -> Self {
        self.supported.push(HookKind::Command);
        self.parts.commands.push((name.to_string(), h));
        self
    }
}

impl Extension for TestExt {
    fn name(&self) -> &'static str {
        self.name
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &self.supported
    }
    fn scopes(&self) -> &[instance::Scope] {
        &self.scopes
    }
    fn manifest(&self) -> manifest::ExtensionManifest {
        manifest::ExtensionManifest {
            name: self.name().into(),
            extension_ref: self.extension_ref(),
            supported_hooks: self.supported.clone(),
            required_capabilities: vec![],
            requested_capabilities: vec![],
            provides_capabilities: self.provides.clone().unwrap_or_else(|| {
                let mut provides = Vec::new();
                if self.parts.context_strategy.is_some() {
                    provides.push(caps::CapabilityKind::ContextStrategy);
                }
                if self.parts.context_tail.is_some() {
                    provides.push(caps::CapabilityKind::ContextTail);
                }
                if self.parts.durable_context.is_some() {
                    provides.push(caps::CapabilityKind::DurableContext);
                }
                provides
            }),
        }
    }
    fn instantiate<'a>(
        &'a self,
        _scope_ctx: instance::ScopeCtx<'a>,
    ) -> instance::InstantiateFuture<'a> {
        let manifest = self.manifest();
        let parts = self.parts.clone();
        let instantiations = self.instantiations.clone();
        let fail = self.fail_instantiate;
        let do_panic = self.panic_instantiate;
        Box::pin(async move {
            if let Some(counter) = instantiations {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            if do_panic {
                panic!("intentional panic for catch_unwind test");
            }
            if fail {
                return Err(anyhow::anyhow!("simulated instantiation failure"));
            }
            Ok(Arc::new(TestInstance { manifest, parts }) as Arc<dyn instance::ExtensionInstance>)
        })
    }
}

pub(super) struct TestInstance {
    pub(super) manifest: manifest::ExtensionManifest,
    pub(super) parts: TestParts,
}
impl instance::ExtensionInstance for TestInstance {
    fn manifest(&self) -> &manifest::ExtensionManifest {
        &self.manifest
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.parts.tools.clone()
    }
    fn commands(&self) -> Vec<(String, Arc<dyn ExtensionCommand>)> {
        self.parts.commands.clone()
    }
    fn tool_call_hook(&self) -> Option<Arc<dyn handler::HookHandlerToolCall>> {
        self.parts.tool_call.clone()
    }
    fn tool_result_hook(&self) -> Option<Arc<dyn handler::HookHandlerToolResult>> {
        self.parts.tool_result.clone()
    }
    fn before_agent_start_hook(&self) -> Option<Arc<dyn handler::HookHandlerBeforeAgentStart>> {
        self.parts.before_agent_start.clone()
    }
    fn routine_handler(&self) -> Option<Arc<dyn handler::RoutineHandler>> {
        self.parts.routine_handler.clone()
    }
    fn prompt_augmentation(&self) -> Option<Arc<dyn caps::PromptAugmentation>> {
        self.parts.prompt_augmentation.clone()
    }
    fn context_tail(&self) -> Option<Arc<dyn caps::ContextTail>> {
        self.parts.context_tail.clone()
    }
    fn context_strategy(&self) -> Option<Arc<dyn ContextStrategy>> {
        self.parts.context_strategy.clone()
    }
    fn durable_context_contributor(&self) -> Option<Arc<dyn DurableContextContributor>> {
        self.parts.durable_context.clone()
    }
    fn messenger(&self) -> Option<Arc<dyn caps::Messenger>> {
        self.parts.messenger.clone()
    }
    fn status_segment(&self) -> Option<Arc<dyn caps::StatusSegment>> {
        self.parts.status_segment.clone()
    }
}

pub(super) fn test_peer_handles(registry: Arc<SessionRegistry>) -> Arc<instance::PeerHandles> {
    Arc::new(instance::PeerHandles {
        registry,
        agent_index: HostedIndex::empty("agent"),
        memory_bank_index: HostedIndex::empty("bank"),
        skill_bank_index: HostedIndex::empty("skill_bank"),
        embedder: None,
        secrets: None,
        server_slot: crate::instance::ServerSlot::default(),
        mcp_registry: Arc::new(crate::mcp::McpRegistry::new()),
        agent_state_allowlist: Default::default(),
        tool_registry: Arc::new(ToolRegistry::new()),
    })
}

/// Hub with `session_registry` + `peer_handles` wired, so
/// `install_all` runs the Global-instance drain (tools, commands,
/// hooks, routine handlers all surface).
pub(super) async fn test_hub() -> ExtensionHub {
    let (_instance, registry) = fresh_session_registry().await;
    let mut hub = ExtensionHub::new();
    hub.set_session_registry(registry.clone());
    hub.set_peer_handles(test_peer_handles(registry));
    hub
}

pub(super) async fn make_session_db() -> (Instance, Database) {
    let (instance, mut user) = fresh_eidetica().await;
    let key = user.get_default_key().unwrap();
    let mut s = Doc::new();
    s.set("name", "session");
    let db = user.create_database(s, &key).await.unwrap();
    (instance, db)
}

pub(super) struct FixedAug(pub(super) &'static str);
impl caps::PromptAugmentation for FixedAug {
    fn augment_system_prompt<'a>(
        &'a self,
        _agent_name: &'a str,
        _recent: &'a [String],
        _available_tool_names: &'a [String],
    ) -> caps::CapFuture<'a, Option<String>> {
        let text = self.0.to_string();
        Box::pin(async move { Ok(Some(text)) })
    }
}

impl caps::ContextTail for FixedAug {
    fn context_tail<'a>(
        &'a self,
        _call: &'a caps::ContextTailCall<'a>,
    ) -> caps::CapFuture<'a, Option<String>> {
        let text = self.0.to_string();
        Box::pin(async move { Ok(Some(text)) })
    }
}

pub(super) struct FixedStatus(pub(super) &'static str, pub(super) &'static str);
impl caps::StatusSegment for FixedStatus {
    fn status_segments<'a>(
        &'a self,
        _agent_name: &'a str,
    ) -> caps::CapFuture<'a, BTreeMap<String, String>> {
        let (k, v) = (self.0.to_string(), self.1.to_string());
        Box::pin(async move { Ok(BTreeMap::from([(k, v)])) })
    }
}

/// Always-`Continue` tool-call hook used by registration-tracking
/// tests.
pub(super) struct PassToolCall;
impl handler::HookHandlerToolCall for PassToolCall {
    fn on_tool_call<'a>(
        &'a self,
        _: &'a str,
        _: &'a mut serde_json::Value,
    ) -> handler::HandlerFuture<'a, ToolCallDecision> {
        Box::pin(async { ToolCallDecision::Continue })
    }
}

pub(super) fn tool_call_ext(name: &'static str) -> Arc<dyn Extension> {
    Arc::new(TestExt::new(name).tool_call(Arc::new(PassToolCall)))
}

pub(super) struct CountingHook {
    pub(super) calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl handler::HookHandlerBeforeAgentStart for CountingHook {
    fn on_before_agent_start<'a>(&'a self) -> handler::HandlerFuture<'a, Vec<RuntimeMessage>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![RuntimeMessage::System("injected".into())]
        })
    }
}

pub(super) fn counting_ext(
    name: &'static str,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<dyn Extension> {
    Arc::new(TestExt::new(name).before_agent_start(Arc::new(CountingHook { calls })))
}

pub(super) struct BlockingHook;
impl handler::HookHandlerToolCall for BlockingHook {
    fn on_tool_call<'a>(
        &'a self,
        name: &'a str,
        _args: &'a mut serde_json::Value,
    ) -> handler::HandlerFuture<'a, ToolCallDecision> {
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

pub(super) struct MutatingHook;
impl handler::HookHandlerToolCall for MutatingHook {
    fn on_tool_call<'a>(
        &'a self,
        _name: &'a str,
        args: &'a mut serde_json::Value,
    ) -> handler::HandlerFuture<'a, ToolCallDecision> {
        Box::pin(async move {
            if let Some(obj) = args.as_object_mut() {
                obj.insert("touched".into(), serde_json::Value::Bool(true));
            }
            ToolCallDecision::Continue
        })
    }
}

pub(super) struct PanickingBeforeAgentStartHook;
impl handler::HookHandlerBeforeAgentStart for PanickingBeforeAgentStartHook {
    fn on_before_agent_start<'a>(&'a self) -> handler::HandlerFuture<'a, Vec<RuntimeMessage>> {
        Box::pin(async move { panic!("intentional panic for catch_unwind test") })
    }
}

pub(super) struct DummyCmd;
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

pub(super) fn cmd_ext(name: &'static str, cmd: &'static str) -> Arc<dyn Extension> {
    Arc::new(TestExt::new(name).command(cmd, Arc::new(DummyCmd)))
}

/// Routine handler that records every payload it receives.
pub(super) struct Recorder {
    pub(super) seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}
impl handler::RoutineHandler for Recorder {
    fn on_fire<'a>(
        &'a self,
        payload: serde_json::Value,
    ) -> handler::HandlerFuture<'a, anyhow::Result<()>> {
        let seen = self.seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(payload);
            Ok(())
        })
    }
}

/// Active owners for dispatch tests that do not exercise the activation log.
pub(super) fn all_active(names: &[&'static str]) -> HashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

pub(super) async fn fixture_ctx_with_active(active: HashSet<String>) -> HookContext {
    let mut ctx = fixture_ctx().await;
    ctx.active_extensions = active;
    ctx
}

pub(super) async fn fixture_ctx() -> HookContext {
    let (_instance, db) = make_session_db().await;
    let session = Session::new(ConversationId("conv".into()), db).await;
    HookContext {
        agent_name: "test_agent".into(),
        model: None,
        call_depth: 0,
        session: Arc::new(Mutex::new(session)),
        active_extensions: HashSet::new(),
        routine_engine: None,
    }
}
