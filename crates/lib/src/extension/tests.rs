//! Unit tests for the extension framework. Extracted from `mod.rs`.

use super::*;
use crate::runtime::RuntimeMessage;
use crate::types::ConversationId;
use eidetica::backend::database::InMemory;
use eidetica::crdt::Doc;
use eidetica::{Instance, NewUser};

#[test]
fn builtin_ref_carries_binary_version() {
    let r = ExtensionRef::builtin("heartbeat");
    match &r {
        ExtensionRef::Builtin { name, chaz_version } => {
            assert_eq!(name, "heartbeat");
            assert_eq!(chaz_version, env!("CARGO_PKG_VERSION"));
        }
        other => panic!("expected Builtin, got {other:?}"),
    }
    assert_eq!(r.name(), "heartbeat");
    assert_eq!(r.version(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn name_and_version_accessors_cover_every_variant() {
    let cases = [
        (
            ExtensionRef::Builtin {
                name: "a".into(),
                chaz_version: "0.1.0".into(),
            },
            "a",
            "0.1.0",
        ),
        (
            ExtensionRef::Eidetica {
                name: "b".into(),
                db_id: "db".into(),
                version: "v1".into(),
            },
            "b",
            "v1",
        ),
        (
            ExtensionRef::Ipld {
                name: "c".into(),
                cid: "bafy...".into(),
            },
            "c",
            "bafy...",
        ),
        (
            ExtensionRef::Git {
                name: "d".into(),
                repo: "https://example.com/r".into(),
                sha: "deadbeef".into(),
            },
            "d",
            "deadbeef",
        ),
    ];
    for (r, expected_name, expected_version) in &cases {
        assert_eq!(r.name(), *expected_name);
        assert_eq!(r.version(), *expected_version);
    }
}

#[test]
fn extension_ref_serde_round_trips_with_tag() {
    let original = ExtensionRef::Git {
        name: "loop_detector".into(),
        repo: "https://github.com/x/y".into(),
        sha: "abc123".into(),
    };
    let json = serde_json::to_string(&original).unwrap();
    // `#[serde(tag = "kind")]` produces a flat, discoverable shape.
    assert!(json.contains("\"kind\":\"git\""), "got: {json}");
    let parsed: ExtensionRef = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, original);
}

struct NamedExt(&'static str);
impl Extension for NamedExt {
    fn name(&self) -> &'static str {
        self.0
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
}

// ── Instance-model test helpers ─────────────────────────────────
//
// The hub no longer has a legacy `install()` path — every
// extension publishes its tools / commands / hook handlers through
// an `ExtensionInstance` drained at `install_all`. These helpers
// let tests build a Global extension from a bag of `Arc`-d
// handlers without spelling out a bespoke `Extension` +
// `ExtensionInstance` pair each time.

#[derive(Default, Clone)]
struct TestParts {
    tool_call: Option<Arc<dyn handler::HookHandlerToolCall>>,
    before_agent_start: Option<Arc<dyn handler::HookHandlerBeforeAgentStart>>,
    tool_result: Option<Arc<dyn handler::HookHandlerToolResult>>,
    agent_end: Option<Arc<dyn handler::HookHandlerAgentEnd>>,
    session_start: Option<Arc<dyn handler::HookHandlerSessionStart>>,
    session_shutdown: Option<Arc<dyn handler::HookHandlerSessionShutdown>>,
    routine_handler: Option<Arc<dyn handler::RoutineHandler>>,
    prompt_augmentation: Option<Arc<dyn caps::PromptAugmentation>>,
    tools: Vec<Arc<dyn Tool>>,
    commands: Vec<(String, Arc<dyn ExtensionCommand>)>,
}

struct TestExt {
    name: &'static str,
    supported: Vec<HookKind>,
    scopes: Vec<instance::Scope>,
    parts: TestParts,
}

impl TestExt {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            supported: Vec::new(),
            scopes: vec![instance::Scope::Global],
            parts: TestParts::default(),
        }
    }
    fn scopes(mut self, scopes: Vec<instance::Scope>) -> Self {
        self.scopes = scopes;
        self
    }
    fn prompt_augmentation(mut self, p: Arc<dyn caps::PromptAugmentation>) -> Self {
        self.parts.prompt_augmentation = Some(p);
        self
    }
    fn tool_call(mut self, h: Arc<dyn handler::HookHandlerToolCall>) -> Self {
        self.supported.push(HookKind::ToolCall);
        self.parts.tool_call = Some(h);
        self
    }
    fn before_agent_start(mut self, h: Arc<dyn handler::HookHandlerBeforeAgentStart>) -> Self {
        self.supported.push(HookKind::BeforeAgentStart);
        self.parts.before_agent_start = Some(h);
        self
    }
    fn routine_handler(mut self, h: Arc<dyn handler::RoutineHandler>) -> Self {
        self.parts.routine_handler = Some(h);
        self
    }
    fn command(mut self, name: &str, h: Arc<dyn ExtensionCommand>) -> Self {
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
    fn instantiate<'a>(
        &'a self,
        _scope_ctx: instance::ScopeCtx<'a>,
    ) -> instance::InstantiateFuture<'a> {
        let manifest = self.manifest();
        let parts = self.parts.clone();
        Box::pin(async move {
            Ok(Arc::new(TestInstance { manifest, parts }) as Arc<dyn instance::ExtensionInstance>)
        })
    }
}

struct TestInstance {
    manifest: manifest::ExtensionManifest,
    parts: TestParts,
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
    fn agent_end_hook(&self) -> Option<Arc<dyn handler::HookHandlerAgentEnd>> {
        self.parts.agent_end.clone()
    }
    fn session_start_hook(&self) -> Option<Arc<dyn handler::HookHandlerSessionStart>> {
        self.parts.session_start.clone()
    }
    fn session_shutdown_hook(&self) -> Option<Arc<dyn handler::HookHandlerSessionShutdown>> {
        self.parts.session_shutdown.clone()
    }
    fn routine_handler(&self) -> Option<Arc<dyn handler::RoutineHandler>> {
        self.parts.routine_handler.clone()
    }
    fn prompt_augmentation(&self) -> Option<Arc<dyn caps::PromptAugmentation>> {
        self.parts.prompt_augmentation.clone()
    }
}

fn test_peer_handles(registry: Arc<SessionRegistry>) -> Arc<instance::PeerHandles> {
    Arc::new(instance::PeerHandles {
        registry,
        agent_index: HostedIndex::empty("agent"),
        memory_bank_index: HostedIndex::empty("bank"),
        skill_bank_index: HostedIndex::empty("skill_bank"),
        embedder: None,
        secrets: None,
        server_cell: Arc::new(std::sync::OnceLock::new()),
        mcp_registry: Arc::new(crate::mcp::McpRegistry::new()),
        agent_state_allowlist: Default::default(),
    })
}

/// Hub with `session_registry` + `peer_handles` wired, so
/// `install_all` runs the Global-instance drain (tools, commands,
/// hooks, routine handlers all surface).
async fn test_hub() -> ExtensionHub {
    use crate::agent::AgentRegistry;
    let backend = InMemory::new();
    let (inst, user) = Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
        .await
        .unwrap();
    let agents = Arc::new(AgentRegistry::with_default_agent());
    let registry = Arc::new(SessionRegistry::new(inst, user, agents).await.unwrap());
    let mut hub = ExtensionHub::new();
    hub.set_session_registry(registry.clone());
    hub.set_peer_handles(test_peer_handles(registry));
    hub
}

async fn make_session_db() -> (Instance, Database) {
    let backend = InMemory::new();
    let (instance, mut user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();
    let key = user.get_default_key().unwrap();
    let mut s = Doc::new();
    s.set("name", "session");
    let db = user.create_database(s, &key).await.unwrap();
    (instance, db)
}

struct FixedAug(&'static str);
impl caps::PromptAugmentation for FixedAug {
    fn augment_system_prompt<'a>(
        &'a self,
        _agent_name: &'a str,
        _recent: &'a [String],
    ) -> caps::CapFuture<'a, Option<String>> {
        let text = self.0.to_string();
        Box::pin(async move { Ok(Some(text)) })
    }
}

#[tokio::test]
async fn ensure_agent_instances_instantiates_per_agent_extensions() {
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("per_agent_ext")
            .scopes(vec![instance::Scope::PerAgent])
            .prompt_augmentation(Arc::new(FixedAug("AGENT-AUG"))),
    )])
    .await
    .unwrap();

    // A PerAgent-only extension is not drained as a Global instance.
    assert!(hub.global_instances.is_empty());

    let (_inst, agent_db) = make_session_db().await;
    let got = hub.ensure_agent_instances("alpha", &agent_db).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].manifest().name, "per_agent_ext");
    assert!(got[0].prompt_augmentation().is_some());

    // Idempotent: a second call returns the same cached instance.
    let again = hub.ensure_agent_instances("alpha", &agent_db).await;
    assert_eq!(again.len(), 1);
    assert!(Arc::ptr_eq(&got[0], &again[0]));
}

#[tokio::test]
async fn ensure_agent_instances_keyed_per_agent_db() {
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("per_agent_ext").scopes(vec![instance::Scope::PerAgent]),
    )])
    .await
    .unwrap();

    let (_i1, db_a) = make_session_db().await;
    let (_i2, db_b) = make_session_db().await;
    let a = hub.ensure_agent_instances("alpha", &db_a).await;
    let b = hub.ensure_agent_instances("beta", &db_b).await;
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    // Distinct agent DBs get distinct instances.
    assert!(!Arc::ptr_eq(&a[0], &b[0]));
    assert_eq!(hub.agent_instances.read().await.len(), 2);
}

#[tokio::test]
async fn ensure_agent_instances_noop_without_per_agent_scope() {
    // A Global-only extension yields no per-agent instances.
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(TestExt::new("global_ext"))])
        .await
        .unwrap();
    let (_inst, agent_db) = make_session_db().await;
    assert!(
        hub.ensure_agent_instances("alpha", &agent_db)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn settings_missing_key_returns_empty_object() {
    let (_inst, db) = make_session_db().await;
    let got = read_settings(&db, "memory").await;
    assert_eq!(got, serde_json::json!({}));
}

#[tokio::test]
async fn settings_round_trip_via_helpers() {
    let (_inst, db) = make_session_db().await;
    let value = serde_json::json!({
        "max_results": 8,
        "embedder": "nomic"
    });
    write_settings(&db, "memory", value.clone()).await.unwrap();
    let got = read_settings(&db, "memory").await;
    assert_eq!(got, value);
}

#[tokio::test]
async fn settings_for_two_extensions_dont_collide() {
    let (_inst, db) = make_session_db().await;
    write_settings(&db, "memory", serde_json::json!({"k": 1}))
        .await
        .unwrap();
    write_settings(&db, "heartbeat", serde_json::json!({"k": 2}))
        .await
        .unwrap();
    assert_eq!(
        read_settings(&db, "memory").await,
        serde_json::json!({"k": 1})
    );
    assert_eq!(
        read_settings(&db, "heartbeat").await,
        serde_json::json!({"k": 2})
    );
}

#[tokio::test]
async fn settings_overwrite_replaces_prior_value() {
    let (_inst, db) = make_session_db().await;
    write_settings(&db, "x", serde_json::json!({"a": 1}))
        .await
        .unwrap();
    write_settings(&db, "x", serde_json::json!({"b": 2}))
        .await
        .unwrap();
    assert_eq!(read_settings(&db, "x").await, serde_json::json!({"b": 2}));
}

#[tokio::test]
async fn hook_context_settings_round_trip() {
    // Build the ctx inline so the `Instance` stays alive for the
    // duration of the read/write calls — `fixture_ctx` drops it
    // before returning, which is fine for fire_* tests that don't
    // touch the DB but not for settings ops.
    let (_inst, db) = make_session_db().await;
    let session = Session::new(ConversationId("conv".into()), db).await;
    let ctx = HookContext {
        agent_name: "test_agent".into(),
        model: None,
        call_depth: 0,
        session: Arc::new(Mutex::new(session)),
        active_extensions: HashSet::new(),
        routine_engine: None,
    };
    ctx.set_settings("heartbeat", serde_json::json!({"poll_secs": 60}))
        .await
        .unwrap();
    let got = ctx.get_settings("heartbeat").await;
    assert_eq!(got, serde_json::json!({"poll_secs": 60}));
}

#[tokio::test]
async fn read_active_on_empty_db_returns_empty() {
    let (_inst, db) = make_session_db().await;
    let active = read_active(&db).await.unwrap();
    assert!(active.is_empty());
}

#[tokio::test]
async fn record_active_writes_events_for_each_extension() {
    let (_inst, db) = make_session_db().await;
    let mut hub = ExtensionHub::new();
    hub.install_all(vec![
        Arc::new(NamedExt("alpha")),
        Arc::new(NamedExt("beta")),
    ])
    .await
    .unwrap();

    hub.record_active(&db).await.unwrap();

    let events = list_events(&db).await.unwrap();
    assert_eq!(events.len(), 2);
    let names: std::collections::HashSet<_> = events.iter().map(|e| e.name()).collect();
    assert!(names.contains("alpha"));
    assert!(names.contains("beta"));
    for e in &events {
        assert!(matches!(e, ExtensionEvent::Activated { .. }));
    }
}

#[tokio::test]
async fn record_active_is_idempotent_when_set_unchanged() {
    let (_inst, db) = make_session_db().await;
    let mut hub = ExtensionHub::new();
    hub.install_all(vec![Arc::new(NamedExt("alpha"))])
        .await
        .unwrap();

    hub.record_active(&db).await.unwrap();
    let after_first = list_events(&db).await.unwrap().len();
    assert_eq!(after_first, 1);

    // Second call with no changes must not append a duplicate.
    hub.record_active(&db).await.unwrap();
    let after_second = list_events(&db).await.unwrap().len();
    assert_eq!(after_second, 1);
}

#[tokio::test]
async fn record_active_respects_deactivation_across_restarts() {
    // `record_active` is the session_start reconciler. The old behavior
    // re-activated anything currently in the hub regardless of prior
    // Deactivated events — which would have undone every
    // `/extensions remove X` on the next session_start. The new
    // contract: respect explicit removal across restarts.
    let (_inst, db) = make_session_db().await;
    let mut hub = ExtensionHub::new();
    hub.install_all(vec![Arc::new(NamedExt("alpha"))])
        .await
        .unwrap();

    hub.record_active(&db).await.unwrap();
    append_event(
        &db,
        ExtensionEvent::Deactivated {
            name: "alpha".into(),
            timestamp: Utc::now() + chrono::Duration::seconds(1),
        },
    )
    .await
    .unwrap();
    assert!(read_active(&db).await.unwrap().is_empty());

    // A subsequent record_active does NOT reactivate — the Deactivated
    // event stands. Only an explicit `/extensions add X` (which writes
    // a fresh Activated) can bring it back.
    hub.record_active(&db).await.unwrap();
    assert!(
        read_active(&db).await.unwrap().is_empty(),
        "record_active should respect prior Deactivated"
    );
}

#[tokio::test]
async fn read_active_folds_to_latest_event_per_name() {
    let (_inst, db) = make_session_db().await;
    let t0 = Utc::now();
    // alpha: Activated at t0, then Deactivated at t1 — should not be active.
    append_event(
        &db,
        ExtensionEvent::Activated {
            name: "alpha".into(),
            extension_ref: ExtensionRef::builtin("alpha"),
            timestamp: t0,
        },
    )
    .await
    .unwrap();
    append_event(
        &db,
        ExtensionEvent::Deactivated {
            name: "alpha".into(),
            timestamp: t0 + chrono::Duration::seconds(10),
        },
    )
    .await
    .unwrap();
    // beta: Activated at t0 only — should be active.
    append_event(
        &db,
        ExtensionEvent::Activated {
            name: "beta".into(),
            extension_ref: ExtensionRef::builtin("beta"),
            timestamp: t0,
        },
    )
    .await
    .unwrap();

    let active = read_active(&db).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].name(), "beta");
}

#[tokio::test]
async fn read_disabled_returns_only_latest_deactivated_names() {
    let (_inst, db) = make_session_db().await;
    let t0 = Utc::now();
    // memory: removed (Deactivated) — should be in the disabled set.
    append_event(
        &db,
        ExtensionEvent::Deactivated {
            name: "memory".into(),
            timestamp: t0,
        },
    )
    .await
    .unwrap();
    // web: removed then re-added — latest is Activated, so NOT disabled.
    append_event(
        &db,
        ExtensionEvent::Deactivated {
            name: "web".into(),
            timestamp: t0,
        },
    )
    .await
    .unwrap();
    append_event(
        &db,
        ExtensionEvent::Activated {
            name: "web".into(),
            extension_ref: ExtensionRef::builtin("web"),
            timestamp: t0 + chrono::Duration::seconds(10),
        },
    )
    .await
    .unwrap();

    let disabled = read_disabled(&db).await.unwrap();
    assert_eq!(disabled.len(), 1);
    assert!(disabled.contains("memory"));
    assert!(!disabled.contains("web"));
}

struct VersionedExt(&'static str, &'static str);
impl Extension for VersionedExt {
    fn name(&self) -> &'static str {
        self.0
    }
    fn extension_ref(&self) -> ExtensionRef {
        ExtensionRef::Git {
            name: self.0.to_string(),
            repo: "repo".to_string(),
            sha: self.1.to_string(),
        }
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
}

#[tokio::test]
async fn record_active_writes_new_event_when_version_bumps() {
    let (_inst, db) = make_session_db().await;

    let mut hub_v1 = ExtensionHub::new();
    hub_v1
        .install_all(vec![Arc::new(VersionedExt("loop", "sha1"))])
        .await
        .unwrap();
    hub_v1.record_active(&db).await.unwrap();
    assert_eq!(list_events(&db).await.unwrap().len(), 1);

    // Fresh hub with the same name but different SHA: must write a new
    // event so the upgrade is captured in the log.
    let mut hub_v2 = ExtensionHub::new();
    hub_v2
        .install_all(vec![Arc::new(VersionedExt("loop", "sha2"))])
        .await
        .unwrap();
    hub_v2.record_active(&db).await.unwrap();
    let events = list_events(&db).await.unwrap();
    assert_eq!(events.len(), 2);
    let active = read_active(&db).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].version(), "sha2");
}

/// Always-`Continue` tool-call hook used by registration-tracking
/// tests.
struct PassToolCall;
impl handler::HookHandlerToolCall for PassToolCall {
    fn on_tool_call<'a>(
        &'a self,
        _: &'a str,
        _: &'a mut serde_json::Value,
    ) -> handler::HandlerFuture<'a, ToolCallDecision> {
        Box::pin(async { ToolCallDecision::Continue })
    }
}

fn tool_call_ext(name: &'static str) -> Arc<dyn Extension> {
    Arc::new(TestExt::new(name).tool_call(Arc::new(PassToolCall)))
}

#[tokio::test]
async fn hub_records_owner_for_each_hook_registration() {
    let mut hub = test_hub().await;
    hub.install_all(vec![tool_call_ext("alpha"), tool_call_ext("beta")])
        .await
        .unwrap();

    let alpha_kinds = hub.hooks_for("alpha");
    assert!(alpha_kinds.contains(&HookKind::ToolCall));
    let beta_kinds = hub.hooks_for("beta");
    assert!(beta_kinds.contains(&HookKind::ToolCall));
    // Other kinds untouched.
    assert!(!alpha_kinds.contains(&HookKind::ToolResult));
}

#[tokio::test]
async fn extensions_for_kind_returns_only_handlers_in_registration_order() {
    let mut hub = test_hub().await;
    hub.install_all(vec![
        Arc::new(NamedExt("noop")),
        tool_call_ext("alpha"),
        tool_call_ext("beta"),
    ])
    .await
    .unwrap();
    let owners = hub.extensions_for_kind(HookKind::ToolCall);
    assert_eq!(owners, vec!["alpha", "beta"]);
    let none = hub.extensions_for_kind(HookKind::AgentEnd);
    assert!(none.is_empty());
}

#[tokio::test]
async fn commands_track_owner_and_are_queryable() {
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("with_command").command("dance", Arc::new(DummyCmd)),
    )])
    .await
    .unwrap();
    assert!(hub.commands_for("with_command").contains("dance"));
    assert_eq!(hub.command_owner("dance"), Some("with_command"));
    assert_eq!(hub.command_owner("not_real"), None);
}

#[tokio::test]
async fn hub_extension_refs_returns_one_per_extension_in_order() {
    let mut hub = test_hub().await;
    hub.install_all(vec![
        Arc::new(NamedExt("alpha")),
        Arc::new(NamedExt("beta")),
    ])
    .await
    .unwrap();
    let refs = hub.extension_refs();
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].name(), "alpha");
    assert_eq!(refs[1].name(), "beta");
    for r in &refs {
        assert!(matches!(r, ExtensionRef::Builtin { .. }));
    }
}

/// Test-only ctx that pretends *every* extension name is active so
/// fire_* tests can exercise handler dispatch without manually
/// listing owners. Production code always builds a real per-session
/// set via `Server::active_extensions_for`.
fn all_active(names: &[&'static str]) -> HashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

async fn fixture_ctx_with_active(active: HashSet<String>) -> HookContext {
    let mut ctx = fixture_ctx().await;
    ctx.active_extensions = active;
    ctx
}

async fn fixture_ctx() -> HookContext {
    let backend = InMemory::new();
    let (_instance, mut user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();
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
        active_extensions: HashSet::new(),
        routine_engine: None,
    }
}

struct CountingHook {
    calls: Arc<std::sync::atomic::AtomicUsize>,
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

fn counting_ext(
    name: &'static str,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<dyn Extension> {
    Arc::new(TestExt::new(name).before_agent_start(Arc::new(CountingHook { calls })))
}

#[tokio::test]
async fn before_agent_start_runs_in_registration_order() {
    let mut hub = test_hub().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    hub.install_all(vec![
        counting_ext("a", calls.clone()),
        counting_ext("b", calls.clone()),
    ])
    .await
    .unwrap();
    let ctx = fixture_ctx_with_active(all_active(&["a", "b"])).await;
    let injected = hub.fire_before_agent_start(&ctx).await;
    assert_eq!(injected.len(), 2);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn inactive_extension_does_not_fire_hooks() {
    // Only "a" is active; "b" must be skipped despite being registered.
    let mut hub = test_hub().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    hub.install_all(vec![
        counting_ext("a", calls.clone()),
        counting_ext("b", calls.clone()),
    ])
    .await
    .unwrap();
    let ctx = fixture_ctx_with_active(all_active(&["a"])).await;
    hub.fire_before_agent_start(&ctx).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

struct BlockingHook;
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

struct MutatingHook;
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

struct PanickingBeforeAgentStartHook;
impl handler::HookHandlerBeforeAgentStart for PanickingBeforeAgentStartHook {
    fn on_before_agent_start<'a>(&'a self) -> handler::HandlerFuture<'a, Vec<RuntimeMessage>> {
        Box::pin(async move { panic!("intentional panic for catch_unwind test") })
    }
}

#[tokio::test]
async fn panicking_hook_is_isolated_and_subsequent_hooks_still_run() {
    let mut hub = test_hub().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    hub.install_all(vec![
        Arc::new(TestExt::new("boom").before_agent_start(Arc::new(PanickingBeforeAgentStartHook))),
        counting_ext("after", calls.clone()),
    ])
    .await
    .unwrap();
    let ctx = fixture_ctx_with_active(all_active(&["boom", "after"])).await;

    // Must not panic-propagate out of `fire_*` — that's the whole
    // point of the catch_unwind wrap.
    let injected = hub.fire_before_agent_start(&ctx).await;

    // The second handler still ran (one message injected, counter
    // incremented). The panicking handler contributed nothing.
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(injected.len(), 1);
}

#[tokio::test]
async fn tool_call_block_short_circuits_and_mutation_propagates() {
    let mut hub = test_hub().await;
    hub.install_all(vec![
        Arc::new(TestExt::new("mutating").tool_call(Arc::new(MutatingHook))),
        Arc::new(TestExt::new("blocking").tool_call(Arc::new(BlockingHook))),
    ])
    .await
    .unwrap();
    let ctx = fixture_ctx_with_active(all_active(&["mutating", "blocking"])).await;

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

fn cmd_ext(name: &'static str, cmd: &'static str) -> Arc<dyn Extension> {
    Arc::new(TestExt::new(name).command(cmd, Arc::new(DummyCmd)))
}

#[tokio::test]
async fn command_collision_with_builtin_is_rejected() {
    let mut hub = test_hub().await;
    hub.reserve_builtin_commands(["info"]);
    hub.install_all(vec![cmd_ext("ext", "info")]).await.unwrap();
    assert!(!hub.has_command("info"));
}

#[tokio::test]
async fn duplicate_extension_command_keeps_first() {
    let mut hub = test_hub().await;
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
    // Drain order = vec order: "first" registers "greet" before
    // "second" tries to — first-write-wins keeps "first".
    hub.install_all(vec![
        cmd_ext("first", "greet"),
        Arc::new(TestExt::new("second").command("greet", Arc::new(OtherCmd))),
    ])
    .await
    .unwrap();
    // "first" wins the collision and owns the command; needs to be
    // in the active set or dispatch will return None.
    let ctx = fixture_ctx_with_active(all_active(&["first"])).await;
    let out = hub
        .try_dispatch_command("greet", "x", &ctx)
        .await
        .expect("command registered");
    match out {
        ExtensionCommandOutcome::Text(s) => assert_eq!(s, "got: x"),
        _ => panic!("expected text outcome"),
    }
}

// -----------------------------------------------------------------
// install_all coverage
// -----------------------------------------------------------------

/// Extension with no scopes-relevant contribution: default
/// `instantiate` yields a no-op `LegacyInstance`, so it registers
/// nothing through the drain.
struct MinimalCapExt(&'static str);
impl Extension for MinimalCapExt {
    fn name(&self) -> &'static str {
        self.0
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
}

#[tokio::test]
async fn install_all_minimal_extension_is_a_noop() {
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(MinimalCapExt("solo"))])
        .await
        .unwrap();
    // Registered, but nothing drained: no routine handler, no
    // tools, no commands, no hooks.
    assert!(hub.extension_names().contains(&"solo"));
    assert!(hub.installed_for("solo").is_none());
    assert!(hub.hooks_for("solo").is_empty());
    assert!(hub.tools_for("solo").is_empty());
    assert!(hub.commands_for("solo").is_empty());
}

#[tokio::test]
async fn install_all_is_idempotent() {
    let mut hub = test_hub().await;
    let ext: Arc<dyn Extension> = Arc::new(MinimalCapExt("solo"));
    hub.install_all(vec![ext.clone()]).await.unwrap();
    hub.install_all(vec![ext]).await.unwrap();
    // Re-installing the same Global extension doesn't double its
    // instance.
    assert!(hub.global_instances.contains_key("solo"));
}

// -----------------------------------------------------------------
// dispatch_routine coverage
// -----------------------------------------------------------------

/// Routine handler that records every payload it receives.
struct Recorder {
    seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
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

#[tokio::test]
async fn dispatch_routine_invokes_registered_handler() {
    let mut hub = test_hub().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    hub.install_all(vec![Arc::new(
        TestExt::new("heartbeat").routine_handler(Arc::new(Recorder { seen: seen.clone() })),
    )])
    .await
    .unwrap();

    hub.dispatch_routine(
        "heartbeat",
        &RoutineScope::Global,
        serde_json::json!({"task": "ping"}),
    )
    .await
    .unwrap();

    let recorded = seen.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0], serde_json::json!({"task": "ping"}));
}

#[tokio::test]
async fn dispatch_routine_unknown_extension_errors() {
    let hub = ExtensionHub::new();
    let err = hub
        .dispatch_routine("ghost", &RoutineScope::Global, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ghost"), "got: {err}");
}

#[tokio::test]
async fn dispatch_routine_extension_without_handler_errors() {
    // A minimal extension publishes no routine handler.
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(MinimalCapExt("solo"))])
        .await
        .unwrap();
    let err = hub
        .dispatch_routine("solo", &RoutineScope::Global, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("routine"), "got: {err}");
}

#[tokio::test]
async fn dispatch_routine_handler_error_propagates() {
    struct AlwaysFails;
    impl handler::RoutineHandler for AlwaysFails {
        fn on_fire<'a>(
            &'a self,
            _payload: serde_json::Value,
        ) -> handler::HandlerFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Err(anyhow::anyhow!("simulated failure")) })
        }
    }
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("broken").routine_handler(Arc::new(AlwaysFails)),
    )])
    .await
    .unwrap();
    let err = hub
        .dispatch_routine("broken", &RoutineScope::Global, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("simulated"), "got: {err}");
}

/// Publishes a no-op Messenger through the instance `messenger()`
/// endpoint. Used by `cap_resolver_walks_instance_published_caps`.
fn instance_messenger_ext() -> Arc<dyn Extension> {
    struct NoopMessenger;
    impl caps::Messenger for NoopMessenger {
        fn send<'a>(
            &'a self,
            _target: String,
            _body: caps::MessageBody,
        ) -> caps::CapFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    struct MessengerInstance {
        manifest: manifest::ExtensionManifest,
    }
    impl instance::ExtensionInstance for MessengerInstance {
        fn manifest(&self) -> &manifest::ExtensionManifest {
            &self.manifest
        }
        fn messenger(&self) -> Option<Arc<dyn caps::Messenger>> {
            Some(Arc::new(NoopMessenger))
        }
    }
    struct MessengerExt;
    impl Extension for MessengerExt {
        fn name(&self) -> &'static str {
            "inst-messenger"
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
        fn instantiate<'a>(
            &'a self,
            _scope_ctx: instance::ScopeCtx<'a>,
        ) -> instance::InstantiateFuture<'a> {
            let manifest = self.manifest();
            Box::pin(async move {
                Ok(Arc::new(MessengerInstance { manifest })
                    as Arc<dyn instance::ExtensionInstance>)
            })
        }
    }
    Arc::new(MessengerExt)
}

#[tokio::test]
async fn cap_resolver_walks_instance_published_caps() {
    let mut hub = test_hub().await;
    hub.install_all(vec![instance_messenger_ext()])
        .await
        .unwrap();

    let resolver = hub.cap_resolver_for_turn(None, None).await;
    use instance::CapResolver as _;
    assert!(
        resolver.messenger().is_some(),
        "HubCapResolver should expose an instance-published Messenger"
    );
}

#[tokio::test]
async fn install_all_validates_manifests_before_instantiation() {
    // Manifest with empty name — should reject before any
    // instantiation runs.
    struct BadManifestExt;
    impl Extension for BadManifestExt {
        fn name(&self) -> &'static str {
            "named-in-trait-but-not-in-manifest"
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
        fn manifest(&self) -> manifest::ExtensionManifest {
            manifest::ExtensionManifest {
                name: String::new(), // <-- triggers EmptyName
                extension_ref: ExtensionRef::builtin("x"),
                supported_hooks: Vec::new(),
                required_capabilities: Vec::new(),
                requested_capabilities: Vec::new(),
                provides_capabilities: Vec::new(),
            }
        }
    }
    let mut hub = ExtensionHub::new();
    let err = hub
        .install_all(vec![Arc::new(BadManifestExt)])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("name"), "got: {err}");
}
