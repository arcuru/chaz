use super::*;
use crate::extension::{
    Extension, ExtensionEvent, ExtensionInstance, HookKind, PeerHandles, ScopeCtx, append_event,
    manifest::ExtensionManifest,
};
use crate::test_support::*;
use crate::{
    backends::BackendManager,
    context::ContextBuilder,
    runtime::{self, ModelCallScope, RuntimeMessage},
    session::{EntryType, SessionEntry},
    tool::{ToolPolicyRegistry, ToolRegistry},
    types::ConversationId,
};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Stub {
    calls: AtomicUsize,
    seen: Mutex<Vec<(String, Option<Value>)>>,
    mode: &'static str,
}
impl DurableContextContributor for Stub {
    fn contribute<'a>(
        &'a self,
        call: &'a DurableContextCall<'a>,
    ) -> CapFuture<'a, ContextContribution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push((call.invocation_id.into(), call.state.cloned()));
        let request = call.request_id.as_str().to_string();
        let n = call.state.and_then(|s| s["n"].as_u64()).unwrap_or(0) + 1;
        Box::pin(async move {
            match self.mode {
                "error" => anyhow::bail!("synthetic failure"),
                "late-error" if request == "fail-after-success" => {
                    anyhow::bail!("synthetic late failure")
                }
                "panic" => panic!("synthetic panic"),
                "timeout" => std::future::pending().await,
                "empty" => Ok(ContextContribution::default()),
                "large" => Ok(ContextContribution {
                    messages: vec!["x".repeat(MAX_BYTES + 1)],
                    state: Some(json!({"n": n})),
                }),
                _ => Ok(ContextContribution {
                    messages: vec![format!("durable:{request} <not-system>")],
                    state: Some(json!({"n": n, "historical_tools": ["forbidden"]})),
                }),
            }
        })
    }
}
struct StubExt {
    stub: Arc<Stub>,
    declares: bool,
    scopes: Vec<Scope>,
}
struct StubInstance {
    manifest: ExtensionManifest,
    stub: Arc<Stub>,
}
impl ExtensionInstance for StubInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
    fn durable_context_contributor(&self) -> Option<Arc<dyn DurableContextContributor>> {
        Some(self.stub.clone())
    }
}
impl Extension for StubExt {
    fn name(&self) -> &'static str {
        "synthetic"
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
    fn scopes(&self) -> &[Scope] {
        &self.scopes
    }
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: "synthetic".into(),
            extension_ref: super::super::ExtensionRef::builtin("synthetic"),
            supported_hooks: vec![],
            required_capabilities: vec![],
            requested_capabilities: vec![],
            provides_capabilities: if self.declares {
                vec![super::super::caps::CapabilityKind::DurableContext]
            } else {
                vec![]
            },
        }
    }
    fn instantiate<'a>(&'a self, _: ScopeCtx<'a>) -> super::super::instance::InstantiateFuture<'a> {
        Box::pin(async move {
            Ok(Arc::new(StubInstance {
                manifest: self.manifest(),
                stub: self.stub.clone(),
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct Fixture {
    _instance: eidetica::Instance,
    _registry_instance: eidetica::Instance,
    session: Arc<tokio::sync::Mutex<Session>>,
    hub: ExtensionHub,
    stub: Arc<Stub>,
    active: HashSet<String>,
}
impl Fixture {
    async fn new(required: bool, mode: &'static str) -> Self {
        Self::scoped(required, mode, true, vec![Scope::Global]).await
    }
    async fn scoped(
        required: bool,
        mode: &'static str,
        declares: bool,
        scopes: Vec<Scope>,
    ) -> Self {
        let (instance, session) = fresh_session().await;
        let (registry_instance, registry) = fresh_session_registry().await;
        let stub = Arc::new(Stub {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
            mode,
        });
        let mut hub = ExtensionHub::new();
        hub.set_peer_handles(Arc::new(PeerHandles {
            registry,
            agent_index: crate::hosted_index::HostedIndex::empty("agent"),
            memory_bank_index: crate::hosted_index::HostedIndex::empty("bank"),
            skill_bank_index: crate::hosted_index::HostedIndex::empty("skill_bank"),
            embedder: None,
            secrets: None,
            server_slot: Default::default(),
            mcp_registry: Arc::new(crate::mcp::McpRegistry::new()),
            agent_state_allowlist: Default::default(),
            tool_registry: Arc::new(ToolRegistry::new()),
        }));
        hub.set_durable_context_grants(&[DurableContextGrant {
            extension: "synthetic".into(),
            required,
        }])
        .unwrap();
        hub.install_all(vec![Arc::new(StubExt {
            stub: stub.clone(),
            declares,
            scopes,
        })])
        .await
        .unwrap();
        let db = session.lock().await.database().clone();
        hub.record_active(&db).await.unwrap();
        Self {
            _instance: instance,
            _registry_instance: registry_instance,
            session,
            hub,
            stub,
            active: HashSet::from(["synthetic".into()]),
        }
    }
    async fn prepare(&self, request: &str) -> anyhow::Result<SessionContextView> {
        let s = self.session.lock().await;
        self.hub
            .prepare_durable_context(
                &s,
                "test-agent",
                Some(&TurnRequestId::parse(request)),
                &self.active,
            )
            .await
    }
    async fn request(
        &self,
        request: &str,
        view: &SessionContextView,
        mock: Arc<MockBackend>,
        budget: usize,
    ) -> Result<runtime::RuntimeOutcome, String> {
        let assembled = ContextBuilder::new(
            &view.entries,
            "test-agent",
            "current instructions",
            &Default::default(),
        )
        .with_context_view(view)
        .build()
        .await;
        let mut ctx = tool_context(self.session.clone(), Arc::new(ToolRegistry::new()));
        ctx.active_extensions = self.active.clone();
        ctx.turn_request_id = Some(TurnRequestId::parse(request));
        runtime::execute_with_recorder(
            Some("mock-model"),
            assembled.messages,
            &BackendManager::with_mock(mock, empty_secrets().await),
            &permissive_security(),
            &ctx,
            &ToolPolicyRegistry::empty(),
            None,
            Some(&self.hub),
            None,
            ModelCallScope {
                attempt_id: Some("new-attempt".into()),
                request_budget_tokens: Some(budget),
                sources: assembled.sources,
            },
        )
        .await
    }
    async fn db_rows(&self) -> Vec<CommittedContribution> {
        let db = self.session.lock().await.database().clone();
        read_at(&db, &db.snapshot().await.unwrap()).await.unwrap()
    }
}
fn entry(text: &str, kind: EntryType) -> SessionEntry {
    SessionEntry {
        sender: "user".into(),
        content: text.into(),
        timestamp: chrono::Utc::now(),
        entry_type: kind,
        metadata: None,
        routing: None,
    }
}

#[tokio::test]
async fn durable_context_retry_reopen_and_new_request_reach_recording_backend() {
    let fx = Fixture::new(true, "ok").await;
    fx.session
        .lock()
        .await
        .add_entry(entry("current request", EntryType::Message))
        .await
        .unwrap();
    let original = serde_json::to_vec(fx.session.lock().await.entries()).unwrap();
    let view = fx.prepare("request-1").await.unwrap();
    assert_eq!(view.contributions.len(), 1);
    assert_eq!(view.contributions, fx.db_rows().await);
    println!(
        "committed database view: {}",
        serde_json::to_string(&view.contributions).unwrap()
    );
    println!(
        "selected database entries: {}",
        serde_json::to_string(&view.entries).unwrap()
    );
    let id = view.contributions[0].source_id(0);
    let warm = fx.prepare("request-1").await.unwrap();
    let db = fx.session.lock().await.database().clone();
    let reopened = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
    let cold = fx
        .hub
        .prepare_durable_context(
            &reopened,
            "test-agent",
            Some(&TurnRequestId::parse("request-1")),
            &fx.active,
        )
        .await
        .unwrap();
    assert_eq!(cold.contributions[0].source_id(0), id);
    assert_eq!(cold.contributions, warm.contributions);
    assert_eq!(fx.stub.calls.load(Ordering::SeqCst), 1);
    assert_eq!(serde_json::to_vec(reopened.entries()).unwrap(), original);
    let mock = Arc::new(MockBackend::new());
    mock.push_err(crate::error::LlmError::RateLimited {
        retry_after_duration: Some(std::time::Duration::ZERO),
        message: "retry".into(),
    });
    mock.push_text("ok");
    fx.request("request-1", &cold, mock.clone(), 10000)
        .await
        .unwrap();
    println!("recorded consumer requests: {:?}", mock.recorded_calls());
    assert_eq!(mock.recorded_calls().len(), 2);
    assert_eq!(
        mock.recorded_calls()[0].messages,
        mock.recorded_calls()[1].messages
    );
    assert!(mock.recorded_calls()[0].messages.iter().any(|m| matches!(m, RuntimeMessage::User(t) if t.contains("durable:request-1 &lt;not-system&gt;"))));
    assert_eq!(fx.stub.calls.load(Ordering::SeqCst), 1);
    let next = fx.prepare("request-2").await.unwrap();
    assert_eq!(next.contributions.len(), 2);
    assert_ne!(
        next.contributions[0].source_id(0),
        next.contributions[1].source_id(0)
    );
    assert_eq!(fx.stub.seen.lock().unwrap()[1].1.as_ref().unwrap()["n"], 1);
    assert_eq!(fx.db_rows().await.len(), 2);
    let warm_messages =
        ContextBuilder::new(&warm.entries, "test-agent", "current", &Default::default())
            .with_context_view(&warm)
            .build()
            .await;
    let cold_messages =
        ContextBuilder::new(&cold.entries, "test-agent", "current", &Default::default())
            .with_context_view(&cold)
            .build()
            .await;
    assert_eq!(warm_messages.messages, cold_messages.messages);
    assert_eq!(warm_messages.sources, cold_messages.sources);
}

#[tokio::test]
async fn durable_context_aborted_transaction_exposes_neither_state_text_nor_receipt() {
    let fx = Fixture::new(true, "ok").await;
    let db = fx.session.lock().await.database().clone();
    let txn = db.new_transaction().await.unwrap();
    let identity = ContributionIdentity {
        version: 1,
        session_db_id: db.root_id().to_string(),
        agent_name: "test-agent".into(),
        scope: "global".into(),
        extension: "synthetic".into(),
        request_id: TurnRequestId::parse("aborted"),
    };
    stage_contribution(
        &txn,
        CommittedContribution {
            identity,
            contribution: ContextContribution {
                messages: vec!["PARTIAL MUST NOT LEAK".into()],
                state: Some(json!({"n": 999})),
            },
        },
    )
    .await
    .unwrap();
    drop(txn); // same staged write path as production, deliberately no commit
    assert!(fx.db_rows().await.is_empty());
    let txn = db.new_transaction().await.unwrap();
    assert!(
        txn.get_store::<Table<StrategyState>>(STATE_STORE)
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap()
            .is_empty()
    );
    let view = fx.prepare("aborted").await.unwrap();
    assert_eq!(
        fx.stub.seen.lock().unwrap()[0].1,
        None,
        "aborted state leaked"
    );
    let mock = Arc::new(MockBackend::new());
    mock.push_text("ok");
    fx.request("aborted", &view, mock.clone(), 10000)
        .await
        .unwrap();
    assert!(!format!("{:?}", mock.recorded_calls()).contains("PARTIAL MUST NOT LEAK"));
    assert_eq!(fx.db_rows().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn durable_context_failures_stop_required_and_optional_use_committed_view() {
    for mode in ["error", "panic", "large", "timeout"] {
        for required in [false, true] {
            let fx = Fixture::new(required, mode).await;
            fx.session
                .lock()
                .await
                .add_entry(entry("current request", EntryType::Message))
                .await
                .unwrap();
            let prepared = fx.prepare("failure").await;
            let mock = Arc::new(MockBackend::new());
            mock.push_text("ok");
            if required {
                assert!(
                    prepared
                        .unwrap_err()
                        .to_string()
                        .contains("required durable")
                );
                let db = fx.session.lock().await.database().clone();
                let view = fx
                    .session
                    .lock()
                    .await
                    .context_view_at(db.snapshot().await.unwrap())
                    .await
                    .unwrap();
                assert!(
                    fx.request("failure", &view, mock.clone(), 10000)
                        .await
                        .is_err()
                );
                assert!(mock.recorded_calls().is_empty());
            } else {
                let view = prepared.unwrap();
                assert!(view.contributions.is_empty());
                fx.request("failure", &view, mock.clone(), 10000)
                    .await
                    .unwrap();
                assert!(
                    mock.recorded_calls()[0]
                        .messages
                        .contains(&RuntimeMessage::User("current request".into()))
                );
            }
            assert!(fx.db_rows().await.is_empty());
        }
    }
}

#[tokio::test]
async fn durable_context_declaration_activation_grants_and_scope_are_enforced() {
    let mut fx = Fixture::scoped(true, "ok", false, vec![Scope::Global]).await;
    for names in [vec![" "], vec!["synthetic", "synthetic"]] {
        let grants: Vec<_> = names
            .into_iter()
            .map(|name| DurableContextGrant {
                extension: name.into(),
                required: false,
            })
            .collect();
        assert!(fx.hub.set_durable_context_grants(&grants).is_err());
        assert!(
            fx.prepare("denied")
                .await
                .unwrap_err()
                .to_string()
                .contains("required durable contributor 'synthetic'"),
            "invalid configuration replaced the existing required grant"
        );
    }
    assert!(fx.prepare("denied").await.is_err());
    assert_eq!(fx.stub.calls.load(Ordering::SeqCst), 0);
    fx.hub.set_durable_context_grants(&[]).unwrap();
    assert!(
        fx.prepare("ungranted")
            .await
            .unwrap()
            .contributions
            .is_empty()
    );
    assert_eq!(fx.stub.calls.load(Ordering::SeqCst), 0);
    let mut fx = Fixture::new(true, "ok").await;
    fx.active.clear();
    assert!(fx.prepare("inactive").await.is_err());
    assert_eq!(fx.stub.calls.load(Ordering::SeqCst), 0);
    let fx = Fixture::scoped(true, "ok", true, vec![Scope::Global, Scope::PerSession]).await;
    let view = fx.prepare("scoped").await.unwrap();
    assert_eq!(view.contributions[0].identity.scope, "session");
    let s = fx.session.lock().await;
    let other = fx
        .hub
        .prepare_durable_context(
            &s,
            "other-agent",
            Some(&TurnRequestId::parse("scoped")),
            &fx.active,
        )
        .await
        .unwrap();
    assert_eq!(other.contributions.len(), 1);
    assert_ne!(
        other.contributions[0].source_id(0),
        view.contributions[0].source_id(0)
    );
    assert_eq!(
        fx.stub.seen.lock().unwrap()[1].1,
        None,
        "state crossed agent scope"
    );
}

#[tokio::test]
async fn durable_context_revocation_and_current_tools_override_stored_facts() {
    for required in [false, true] {
        let fx = Fixture::new(required, "ok").await;
        fx.session
            .lock()
            .await
            .add_entry(entry("input", EntryType::Message))
            .await
            .unwrap();
        let view = fx.prepare("revoked").await.unwrap();
        assert_eq!(
            view.contributions[0].contribution.state.as_ref().unwrap()["historical_tools"],
            json!(["forbidden"])
        );
        let mock = Arc::new(MockBackend::new());
        mock.push_text("ok");
        fx.request("revoked", &view, mock.clone(), 10000)
            .await
            .unwrap();
        assert!(
            mock.recorded_calls()[0].tools.is_empty(),
            "stored fact minted tool authority"
        );
        let db = fx.session.lock().await.database().clone();
        append_event(
            &db,
            ExtensionEvent::Deactivated {
                name: "synthetic".into(),
                timestamp: chrono::Utc::now() + chrono::Duration::seconds(1),
            },
        )
        .await
        .unwrap();
        let mock = Arc::new(MockBackend::new());
        mock.push_text("ok");
        let result = fx.request("revoked", &view, mock.clone(), 10000).await;
        if required {
            assert!(result.is_err());
            assert!(mock.recorded_calls().is_empty());
        } else {
            result.unwrap();
            assert!(!format!("{:?}", mock.recorded_calls()).contains("durable:revoked"));
        }
        assert_eq!(
            fx.db_rows().await.len(),
            1,
            "revocation destroyed originals"
        );
    }
}

#[tokio::test]
async fn durable_context_compaction_legacy_and_budget_preserve_originals() {
    let fx = Fixture::new(true, "ok").await;
    let mut session = fx.session.lock().await;
    session
        .add_entry(entry("old", EntryType::Message))
        .await
        .unwrap();
    session
        .add_entry(entry("raw-tool-original", EntryType::ToolResult))
        .await
        .unwrap();
    session
        .add_entry(entry("summary", EntryType::Summary))
        .await
        .unwrap();
    let current = session
        .add_entry(entry("current", EntryType::Message))
        .await
        .unwrap();
    let queued = session
        .add_entry(entry("queued", EntryType::Message))
        .await
        .unwrap();
    let original = serde_json::to_vec(session.entries()).unwrap();
    let db = session.database().clone();
    let legacy = session
        .context_view_at(db.snapshot().await.unwrap())
        .await
        .unwrap();
    assert!(legacy.contributions.is_empty());
    drop(session);
    let view = fx.prepare(current.as_str()).await.unwrap();
    assert!(
        view.entry_ids.contains(&Some(current.clone())) && view.entry_ids.contains(&Some(queued))
    );
    let mock = Arc::new(MockBackend::new());
    mock.push_text("ok");
    fx.request(current.as_str(), &view, mock.clone(), 10000)
        .await
        .unwrap();
    let calls = mock.recorded_calls();
    assert!(
        calls[0]
            .messages
            .contains(&RuntimeMessage::User("summary".into()))
    );
    assert!(
        calls[0]
            .messages
            .contains(&RuntimeMessage::User("current".into()))
    );
    assert!(
        calls[0]
            .messages
            .contains(&RuntimeMessage::User("queued".into()))
    );
    assert!(
        !calls[0]
            .messages
            .contains(&RuntimeMessage::User("old".into()))
    );
    let mock = Arc::new(MockBackend::new());
    mock.push_text("must not dispatch");
    assert!(
        fx.request(current.as_str(), &view, mock.clone(), 1)
            .await
            .unwrap_err()
            .contains("over the")
    );
    assert!(mock.recorded_calls().is_empty());
    assert_eq!(
        serde_json::to_vec(
            Session::new(ConversationId(db.root_id().to_string()), db)
                .await
                .entries()
        )
        .unwrap(),
        original
    );
}

#[tokio::test]
async fn durable_context_pinned_view_and_peer_update_reconstruct_coherently() {
    use eidetica::{
        Instance, NewUser,
        auth::{Permission, types::AuthKey},
        backend::database::InMemory,
        sync::{Address, transports::http::HttpTransport},
    };
    let fx = Fixture::new(true, "ok").await;
    let db = fx.session.lock().await.database().clone();
    let owner = &fx._instance;
    owner.enable_sync().await.unwrap();
    let txn = db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_global_auth_key(AuthKey::active(None, Permission::Write(0)))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    db.share().await.unwrap();
    let sync = owner.sync().unwrap();
    sync.register_transport("http", HttpTransport::builder().bind("127.0.0.1:0"))
        .await
        .unwrap();
    sync.accept_connections().await.unwrap();
    let address = Address::http(sync.get_server_address_for("http").await.unwrap());
    let (peer, mut user) =
        Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("peer"))
            .await
            .unwrap();
    peer.enable_sync().await.unwrap();
    let key = user.get_default_key().unwrap();
    let signing = user.get_signing_key(&key).unwrap();
    let ps = peer.sync().unwrap();
    ps.register_transport("http", HttpTransport::builder())
        .await
        .unwrap();
    ps.sync_with_peer_for_bootstrap_with_key(
        &address,
        db.root_id(),
        &signing,
        &key.to_string(),
        Permission::Write(10),
    )
    .await
    .unwrap();
    ps.flush().await.unwrap();
    let (sigkey, _) = Database::find_sigkeys(&peer, db.root_id(), &key)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    user.map_key(&key, db.root_id(), sigkey).await.unwrap();
    let pdb = user.open_database(db.root_id()).await.unwrap();
    let mut peer_session =
        Session::new(ConversationId(pdb.root_id().to_string()), pdb.clone()).await;
    let before = db.snapshot().await.unwrap();
    peer_session
        .add_entry(entry("PEER UPDATE", EntryType::Message))
        .await
        .unwrap();
    fx.hub
        .prepare_durable_context(
            &peer_session,
            "test-agent",
            Some(&TurnRequestId::parse("peer-request")),
            &fx.active,
        )
        .await
        .unwrap();
    ps.sync_with_peer(&address, Some(db.root_id()))
        .await
        .unwrap();
    let pinned = fx
        .session
        .lock()
        .await
        .context_view_at(before)
        .await
        .unwrap();
    assert!(pinned.entries.is_empty() && pinned.contributions.is_empty());
    let next = fx.prepare("local-request").await.unwrap();
    assert!(next.entries.iter().any(|e| e.content == "PEER UPDATE"));
    assert_eq!(next.contributions.len(), 2);
    let mock = Arc::new(MockBackend::new());
    mock.push_text("ok");
    fx.request("local-request", &next, mock.clone(), 10000)
        .await
        .unwrap();
    let sent = format!("{:?}", mock.recorded_calls());
    assert!(sent.contains("PEER UPDATE") && sent.contains("durable:peer-request"));
    assert_eq!(
        fx.stub.seen.lock().unwrap()[1].1.as_ref().unwrap()["n"],
        1,
        "synced state not reconstructed"
    );
}

#[tokio::test]
async fn durable_context_optional_failure_keeps_prior_commit_but_refuses_incoherent_state() {
    let fx = Fixture::new(false, "late-error").await;
    let before = fx.prepare("success").await.unwrap();
    let after = fx.prepare("fail-after-success").await.unwrap();
    assert_eq!(before.contributions, after.contributions);
    let mock = Arc::new(MockBackend::new());
    mock.push_text("ok");
    fx.request("fail-after-success", &after, mock.clone(), 10000)
        .await
        .unwrap();
    assert!(format!("{:?}", mock.recorded_calls()).contains("durable:success"));
    let db = fx.session.lock().await.database().clone();
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<StrategyState>>(STATE_STORE)
        .await
        .unwrap()
        .set(
            before.contributions[0].identity.namespace(),
            StrategyState {
                invocation_id: "uncommitted".into(),
                value: json!({"n": 999}),
            },
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        fx.prepare("fail-after-success")
            .await
            .unwrap_err()
            .to_string()
            .contains("matching committed receipt")
    );
}

#[tokio::test]
async fn durable_context_empty_receipt_missing_identity_and_namespace_collisions() {
    let fx = Fixture::new(true, "empty").await;
    let a = fx.prepare("empty-request").await.unwrap();
    let b = fx.prepare("empty-request").await.unwrap();
    assert_eq!(a.contributions, b.contributions);
    assert_eq!(fx.stub.calls.load(Ordering::SeqCst), 1);
    assert!(a.contributions[0].contribution.messages.is_empty());
    let s = fx.session.lock().await;
    assert!(
        fx.hub
            .prepare_durable_context(&s, "test-agent", None, &fx.active)
            .await
            .is_err()
    );
    let db = s.database().clone();
    drop(s);
    // A genuinely different session cannot see state or receipts.
    let (_other_instance, other) = fresh_session().await;
    let s = other.lock().await;
    fx.hub.record_active(s.database()).await.unwrap();
    let other_view = fx
        .hub
        .prepare_durable_context(
            &s,
            "test-agent",
            Some(&TurnRequestId::parse("empty-request")),
            &fx.active,
        )
        .await
        .unwrap();
    assert_ne!(
        a.contributions[0].source_id(0),
        other_view.contributions[0].source_id(0)
    );
    assert_eq!(fx.stub.seen.lock().unwrap()[1].1, None);
    let mut identity = a.contributions[0].identity.clone();
    identity.session_db_id = "db".into();
    identity.agent_name = "a:b".into();
    identity.extension = "c".into();
    let left = identity.namespace();
    identity.agent_name = "a".into();
    identity.extension = "b:c".into();
    assert_ne!(left, identity.namespace());
    assert_eq!(identity.namespace(), r#"[1,"db","a","global","b:c"]"#);
    assert_eq!(
        serde_json::from_str::<CommittedContribution>(
            &serde_json::to_string(&a.contributions[0]).unwrap()
        )
        .unwrap(),
        a.contributions[0]
    );
    assert_eq!(
        read_at(&db, &db.snapshot().await.unwrap())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn durable_context_agent_scope_and_current_optout_attenuate_writes() {
    let fx = Fixture::scoped(true, "ok", true, vec![Scope::Global, Scope::PerAgent]).await;
    let hub = Arc::new(fx.hub);
    let peer = hub.peer_handles.as_ref().unwrap().clone();
    let registry = &peer.registry;
    let seeded =
        crate::agent_db::ensure_agent_db(&mut *registry.user_for_tests().await, "test-agent")
            .await
            .unwrap();
    peer.agent_index.register(crate::hosted_index::DbEntry {
        db_id: seeded.db.id(),
        display_name: "test-agent".into(),
        pubkey: seeded.pubkey,
    });
    let mock = Arc::new(MockBackend::new());
    let server = crate::server::Server::new(
        registry.clone(),
        registry.agents.clone(),
        peer.agent_index.clone(),
        peer.memory_bank_index.clone(),
        peer.skill_bank_index.clone(),
        peer.tool_registry.clone(),
        Arc::new(ToolPolicyRegistry::empty()),
        permissive_security(),
        Default::default(),
        Default::default(),
        Arc::new(crate::tool_host::NativeToolHost::new()),
        hub.clone(),
        BackendManager::with_mock(mock.clone(), empty_secrets().await),
        peer.mcp_registry.clone(),
        None,
    );
    peer.server_slot.set(server.clone());
    let s = fx.session.lock().await;
    let view = hub
        .prepare_durable_context(
            &s,
            "test-agent",
            Some(&TurnRequestId::parse("agent-scope")),
            &fx.active,
        )
        .await
        .unwrap();
    assert_eq!(view.contributions[0].identity.scope, "agent");
    append_event(
        seeded.db.database(),
        ExtensionEvent::Deactivated {
            name: "synthetic".into(),
            timestamp: chrono::Utc::now() + chrono::Duration::seconds(1),
        },
    )
    .await
    .unwrap();
    assert!(
        hub.prepare_durable_context(
            &s,
            "test-agent",
            Some(&TurnRequestId::parse("new-after-optout")),
            &fx.active
        )
        .await
        .is_err()
    );
    assert_eq!(fx.stub.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        read_at(s.database(), &s.database().snapshot().await.unwrap())
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(mock.recorded_calls().is_empty());
    peer.server_slot.clear();
    server.shutdown().await;
}

#[tokio::test]
async fn durable_context_sqlite_reopen_with_fresh_host_reuses_committed_receipt() {
    use eidetica::{Instance, NewUser, backend::database::Sqlite, crdt::Doc};
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("context.db").display()
    );
    let (root, before, messages, sources) = {
        let (instance, mut user) = Instance::create_backend(
            Box::new(Sqlite::connect(&url).await.unwrap()),
            NewUser::passwordless("durable"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user.create_database(Doc::new(), &key).await.unwrap();
        let root = db.root_id().clone();
        let mut fx = Fixture::new(true, "ok").await;
        fx.session = Arc::new(tokio::sync::Mutex::new(
            Session::new(ConversationId(root.to_string()), db.clone()).await,
        ));
        fx.hub.record_active(&db).await.unwrap();
        fx.session
            .lock()
            .await
            .add_entry(entry("disk request", EntryType::Message))
            .await
            .unwrap();
        let view = fx.prepare("disk-request").await.unwrap();
        let assembled = ContextBuilder::new(
            &view.entries,
            "test-agent",
            "current instructions",
            &Default::default(),
        )
        .with_context_view(&view)
        .build()
        .await;
        let mock = Arc::new(MockBackend::new());
        mock.push_text("ok");
        fx.request("disk-request", &view, mock.clone(), 10000)
            .await
            .unwrap();
        assert_eq!(mock.recorded_calls()[0].messages, assembled.messages);
        assert_eq!(fx.stub.calls.load(Ordering::SeqCst), 1);
        // Drop every host/session/user/backend handle, retaining only plain data.
        drop(fx);
        drop(db);
        drop(user);
        drop(instance);
        (
            root,
            view.contributions,
            assembled.messages,
            assembled.sources,
        )
    };
    let instance = Instance::connect(&url).await.unwrap();
    let user = instance.login_user("durable", None).await.unwrap();
    let db = user.open_database(&root).await.unwrap();
    let mut fresh = Fixture::new(true, "ok").await;
    fresh.session = Arc::new(tokio::sync::Mutex::new(
        Session::new(ConversationId(root.to_string()), db).await,
    ));
    let after = fresh.prepare("disk-request").await.unwrap();
    assert_eq!(after.contributions, before);
    assert_eq!(
        fresh.stub.calls.load(Ordering::SeqCst),
        0,
        "reopened fresh extension ran despite persisted receipt"
    );
    let assembled = ContextBuilder::new(
        &after.entries,
        "test-agent",
        "current instructions",
        &Default::default(),
    )
    .with_context_view(&after)
    .build()
    .await;
    assert_eq!(assembled.sources, sources);
    let mock = Arc::new(MockBackend::new());
    mock.push_text("ok");
    fresh
        .request("disk-request", &after, mock.clone(), 10000)
        .await
        .unwrap();
    assert_eq!(mock.recorded_calls()[0].messages, messages);
    assert_eq!(fresh.db_rows().await.len(), 1);
}
