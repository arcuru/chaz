use super::*;
use crate::backends::BackendManager;
use crate::config::ContextConfig;
use crate::context::ContextBuilder;
use crate::extension::instance::PeerHandles;
use crate::extension::{ExtensionHub, read_settings, write_settings};
use crate::hosted_index::HostedIndex;
use crate::runtime::{
    self, LLMResponse, ResponseMetadata, RuntimeMessage, TokenUsage, ToolCallRequest,
};
use crate::session::{EntryType, Session, SessionEntry, SessionRegistry};
use crate::test_support::{
    MockBackend, empty_secrets, fresh_session_registry, permissive_security, tool_context,
};
use crate::tool::{ToolContext, ToolPolicyRegistry, ToolRegistry};
use tokio::sync::Mutex;

struct Fixture {
    _instance: eidetica::Instance,
    registry: Arc<SessionRegistry>,
    ctx: ToolContext,
    hub: Arc<ExtensionHub>,
    mock: Arc<MockBackend>,
    backend: BackendManager,
}

async fn hub(registry: Arc<SessionRegistry>) -> ExtensionHub {
    let mut hub = ExtensionHub::new();
    hub.set_session_registry(registry.clone());
    hub.set_peer_handles(Arc::new(PeerHandles {
        registry,
        agent_index: HostedIndex::empty("agent"),
        memory_bank_index: HostedIndex::empty("bank"),
        skill_bank_index: HostedIndex::empty("skill_bank"),
        embedder: None,
        secrets: None,
        server_slot: Default::default(),
        mcp_registry: Arc::new(crate::mcp::McpRegistry::new()),
        agent_state_allowlist: Default::default(),
        tool_registry: Arc::new(ToolRegistry::new()),
    }));
    hub.install_all(vec![Arc::new(SessionNaming)])
        .await
        .unwrap();
    hub
}

impl Fixture {
    async fn new(settings: serde_json::Value) -> Self {
        let (instance, registry) = fresh_session_registry().await;
        let (conv, db) = registry.create_session(Some("cli")).await.unwrap();
        write_settings(&db, "session_naming", settings)
            .await
            .unwrap();
        let session = Arc::new(Mutex::new(Session::new(conv, db).await));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(crate::tools::Calculate);
        let mut ctx = tool_context(session, tools);
        ctx.active_extensions.insert("session_naming".into());
        let hub = Arc::new(hub(registry.clone()).await);
        hub.record_active(ctx.session.lock().await.database())
            .await
            .unwrap();
        let mock = Arc::new(MockBackend::new());
        let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
        Self {
            _instance: instance,
            hub,
            registry,
            ctx,
            mock,
            backend,
        }
    }
    async fn run(&self, messages: Vec<RuntimeMessage>) -> Result<runtime::RuntimeOutcome, String> {
        runtime::execute(
            Some("turn-model"),
            messages,
            &self.backend,
            &permissive_security(),
            &self.ctx,
            &ToolPolicyRegistry::empty(),
            None,
            Some(&self.hub),
        )
        .await
    }
    async fn id(&self) -> String {
        self.ctx
            .session
            .lock()
            .await
            .database()
            .root_id()
            .to_string()
    }
    async fn settings(&self) -> serde_json::Value {
        read_settings(self.ctx.session.lock().await.database(), "session_naming").await
    }
}

fn entry(content: &str, entry_type: EntryType) -> SessionEntry {
    SessionEntry {
        sender: "user".into(),
        content: content.into(),
        timestamp: chrono::Utc::now(),
        entry_type,
        metadata: None,
        routing: None,
    }
}

#[tokio::test]
async fn naming_receives_full_public_context_and_persists_canonically() {
    let mut f = Fixture::new(json!({"enabled": true, "model": "name-model"})).await;
    let (changed_tx, mut changed_rx) = tokio::sync::mpsc::unbounded_channel();
    f.ctx
        .session
        .lock()
        .await
        .database()
        .on_write(move |_, db| {
            let tx = changed_tx.clone();
            let db = db.clone();
            Box::pin(async move {
                if let Ok(Some(name)) = crate::session::read_session_name_from_db(&db).await {
                    let _ = tx.send(name);
                }
                Ok(())
            })
        })
        .await
        .unwrap()
        .detach();
    let (_, other) = f.registry.create_session(Some("cli")).await.unwrap();
    let mut other_session = Session::new(
        crate::types::ConversationId(other.root_id().to_string()),
        other,
    )
    .await;
    other_session
        .add_entry(entry("OTHER-SESSION-PRIVATE", EntryType::Message))
        .await
        .unwrap();
    let entries = vec![
        entry("earlier summary", EntryType::Summary),
        entry("find the migration problem", EntryType::Message),
    ];
    let defs = f.ctx.tools.definitions(&f.ctx.profile);
    let assembled = ContextBuilder::new(
        &entries,
        "test-agent",
        "system rules + skill augmentation",
        &ContextConfig::default(),
    )
    .with_room_participants(&["test-agent".into(), "other-agent".into()])
    .with_tools(&defs)
    .build()
    .await;
    let mut initial = assembled.messages;
    initial.push(RuntimeMessage::AssistantToolCalls {
        exchange_id: None,
        content: Some("historical public text".into()),
        tool_calls: vec![ToolCallRequest {
            id: "history".into(),
            name: "calculate".into(),
            arguments: "{}".into(),
        }],
        provider_extra: serde_json::from_value(json!({"reasoning_content": "HIDDEN-HISTORY"}))
            .unwrap(),
    });
    initial.push(RuntimeMessage::ToolResult {
        call_id: "history".into(),
        content: "historical tool result".into(),
    });
    f.mock.push_response(Ok(LLMResponse::ToolCalls {
        content: Some("current public text".into()),
        tool_calls: vec![ToolCallRequest {
            id: "current".into(),
            name: "calculate".into(),
            arguments: r#"{"expression":"2+2"}"#.into(),
        }],
        provider_extra: serde_json::from_value(json!({"reasoning_details": "HIDDEN-CURRENT"}))
            .unwrap(),
        metadata: None,
    }));
    f.mock.push_text("main reply");
    f.mock.push_response(Ok(LLMResponse::Text {
        content: "Migration diagnosis".into(),
        provider_extra: serde_json::from_value(json!({"reasoning_content":"HIDDEN-AUXILIARY"}))
            .unwrap(),
        metadata: Some(ResponseMetadata {
            model: "name-model".into(),
            usage: TokenUsage {
                prompt_tokens: 200,
                completion_tokens: 5,
                total_tokens: 205,
                cost_usd: Some(0.001),
                ..Default::default()
            },
            extra: serde_json::from_value(json!({"unknown": "HIDDEN-METADATA"})).unwrap(),
            ..Default::default()
        }),
    }));
    let outcome = f.run(initial).await.unwrap();
    assert_eq!(outcome.body, "main reply");
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), changed_rx.recv())
            .await
            .unwrap()
            .as_deref(),
        Some("Migration diagnosis"),
        "canonical name write must notify session observers"
    );
    assert_eq!(
        outcome.metadata.unwrap().model,
        "mock-model",
        "auxiliary accounting must not overwrite the main turn"
    );
    let calls = f.mock.recorded_calls();
    assert_eq!(
        calls.len(),
        3,
        "one auxiliary call, no recursive runtime dispatch"
    );
    assert_eq!(calls[2].model, "name-model");
    assert!(calls[2].tools.is_empty());
    let public = format!("{:?}", calls[2].messages);
    for text in [
        "system rules + skill augmentation",
        "@other-agent",
        "earlier summary",
        "find the migration problem",
        "historical public text",
        "historical tool result",
        "current public text",
        "<tool_output",
        "main reply",
        "calculate",
    ] {
        assert!(public.contains(text), "missing {text}: {public}");
    }
    for text in ["HIDDEN-HISTORY", "HIDDEN-CURRENT", "OTHER-SESSION-PRIVATE"] {
        assert!(!public.contains(text));
    }
    assert!(
        format!("{:?}", calls[1].messages).contains("HIDDEN-CURRENT"),
        "the main runtime's provider continuation must remain intact"
    );
    let id = f.id().await;
    assert_eq!(
        f.registry
            .find_by_name("Migration diagnosis")
            .await
            .unwrap(),
        Some(id.clone())
    );
    let (conv, db) = f.registry.open_session(&id).await.unwrap();
    assert_eq!(
        Session::new(conv, db)
            .await
            .read_meta()
            .await
            .name
            .as_deref(),
        Some("Migration diagnosis")
    );
    assert_eq!(
        f.registry
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .find(|s| s.session_db_id == id)
            .unwrap()
            .name
            .as_deref(),
        Some("Migration diagnosis")
    );
    let settings = f.settings().await;
    assert_eq!(settings["attempted"], true);
    assert_eq!(settings["status"], "named");
    assert_eq!(settings["last_usage"]["usage"]["cost_usd"], 0.001);
    assert_eq!(settings["last_usage"]["extra"], serde_json::Value::Null);
    // A new extension instance and clearing the name do not reset the durable
    // attempt. Retrying requires an explicit settings write.
    f.registry.clear_session_name(&id).await.unwrap();
    f.hub = Arc::new(hub(f.registry.clone()).await);
    f.mock.push_text("second main reply");
    f.run(vec![RuntimeMessage::User("second request".into())])
        .await
        .unwrap();
    assert_eq!(f.mock.recorded_calls().len(), 4);
}

#[tokio::test]
async fn naming_is_opt_in_and_respects_explicit_names_activation_and_depth() {
    for case in ["disabled", "named", "inactive", "child", "silent"] {
        let mut f = Fixture::new(json!({"enabled": case != "disabled"})).await;
        if case == "named" {
            f.registry
                .set_session_name(&f.id().await, "User's name".into())
                .await
                .unwrap();
        }
        if case == "inactive" {
            f.ctx.active_extensions.clear();
        }
        if case == "child" {
            f.ctx.call_depth = 1;
        }
        if case == "silent" {
            f.ctx.allow_no_reply = true;
            f.mock
                .push_tool_calls([("silence".into(), "no_reply".into(), "{}".into())]);
        } else {
            f.mock.push_text("main reply");
        }
        assert!(
            f.run(vec![RuntimeMessage::User("request".into())])
                .await
                .is_ok()
        );
        assert_eq!(
            f.mock.recorded_calls().len(),
            1,
            "unexpected call for {case}"
        );
        assert!(f.settings().await.get("attempted").is_none());
    }
    let f = Fixture::new(json!({"enabled": true})).await;
    f.mock.push_err(crate::error::LlmError::Configuration {
        message: "failed main turn".into(),
    });
    assert!(f.run(vec![]).await.is_err());
    assert_eq!(f.mock.recorded_calls().len(), 1);
    assert!(f.settings().await.get("attempted").is_none());
}

#[tokio::test]
async fn auxiliary_failures_do_not_fail_or_repeat_the_user_turn() {
    for case in [
        "backend",
        "empty",
        "multiline",
        "unicode-line",
        "unicode-paragraph",
        "long",
        "tools",
        "secret",
        "collision",
    ] {
        let f = Fixture::new(json!({"enabled": true})).await;
        if case == "collision" {
            let (_, db) = f.registry.create_session(None).await.unwrap();
            f.registry
                .set_session_name(&db.root_id().to_string(), "Taken".into())
                .await
                .unwrap();
        }
        f.mock.push_text("main reply");
        match case {
            "backend" => f.mock.push_err(crate::error::LlmError::Configuration {
                message: "naming unavailable".into(),
            }),
            "tools" => f.mock.push_tool_calls([(
                "aux-tool".into(),
                "calculate".into(),
                r#"{"expression":"1+1"}"#.into(),
            )]),
            "empty" => f.mock.push_text(""),
            "multiline" => f.mock.push_text("Bad\nname"),
            "unicode-line" => f.mock.push_text("Bad\u{2028}name"),
            "unicode-paragraph" => f.mock.push_text("Bad\u{2029}name"),
            "long" => f.mock.push_text("x".repeat(81)),
            "secret" => f.mock.push_text("sk-1234567890123456789012345"),
            "collision" => f.mock.push_text("Taken"),
            _ => unreachable!(),
        }
        assert_eq!(
            f.run(vec![RuntimeMessage::User("request".into())])
                .await
                .unwrap()
                .body,
            "main reply"
        );
        assert_eq!(f.settings().await["status"], "failed", "case {case}");
        assert!(f.ctx.session.lock().await.read_meta().await.name.is_none());
        f.mock.push_text("next reply");
        assert!(f.run(vec![]).await.is_ok());
        assert_eq!(
            f.mock.recorded_calls().len(),
            3,
            "no retry or tool execution for {case}"
        );
        if case == "backend" {
            let db = f.ctx.session.lock().await.database().clone();
            write_settings(
                &db,
                "session_naming",
                json!({"enabled": true, "attempted": false}),
            )
            .await
            .unwrap();
            f.mock.push_text("retry reply");
            f.mock.push_text("Recovered name");
            assert_eq!(f.run(vec![]).await.unwrap().body, "retry reply");
            assert_eq!(f.mock.recorded_calls().len(), 5);
            assert_eq!(
                f.ctx.session.lock().await.read_meta().await.name.as_deref(),
                Some("Recovered name")
            );
            assert_eq!(f.settings().await["status"], "named");
        }
    }
}

#[tokio::test]
async fn explicit_rename_during_naming_inference_wins() {
    let f = Fixture::new(json!({"enabled": true})).await;
    f.mock.push_text("main reply");
    f.mock.push_text("Generated name");
    let main_gate = f.mock.block_next_call();
    let registry = f.registry.clone();
    let id = f.id().await;
    let mock = f.mock.clone();
    let task = tokio::spawn(async move {
        let outcome = f.run(vec![]).await.unwrap();
        (f, outcome)
    });
    main_gate.wait_started().await;
    let naming_gate = mock.block_next_call();
    main_gate.release();
    naming_gate.wait_started().await;
    registry
        .set_session_name(&id, "Explicit name".into())
        .await
        .unwrap();
    naming_gate.release();
    let (f, outcome) = task.await.unwrap();
    assert_eq!(outcome.body, "main reply");
    assert_eq!(
        f.ctx.session.lock().await.read_meta().await.name.as_deref(),
        Some("Explicit name")
    );
    assert_eq!(f.settings().await["status"], "preserved");
    assert!(
        registry
            .find_by_name("Generated name")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn auxiliary_timeout_is_bounded_and_keeps_main_reply() {
    let f = Fixture::new(json!({"enabled": true})).await;
    f.mock.push_text("main reply");
    f.mock.push_text("never delivered");
    let main_gate = f.mock.block_next_call();
    let mock = f.mock.clone();
    let task = tokio::spawn(async move {
        let outcome = f.run(vec![]).await.unwrap();
        (f, outcome)
    });
    main_gate.wait_started().await;
    let naming_gate = mock.block_next_call();
    main_gate.release();
    naming_gate.wait_started().await;
    let (f, outcome) = tokio::time::timeout(std::time::Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    naming_gate.wait_stopped().await;
    assert_eq!(outcome.body, "main reply");
    assert_eq!(f.settings().await["status"], "failed");
    assert_eq!(f.mock.recorded_calls().len(), 2);
}

#[tokio::test]
async fn naming_refuses_unknown_session_and_smaller_model_window() {
    let mut f = Fixture::new(json!({"enabled": true, "model": "tiny"})).await;
    f.backend.insert_model_window("tiny".into(), 10);
    f.mock.push_text("main reply");
    assert!(
        f.run(vec![RuntimeMessage::System(
            "must retain all system context".into()
        )])
        .await
        .is_ok()
    );
    assert_eq!(
        f.mock.recorded_calls().len(),
        1,
        "refuse instead of silently sending a shorter context"
    );
    assert_eq!(f.settings().await["status"], "failed");
    let (_foreign, session) = crate::test_support::fresh_session().await;
    let foreign_id = session.lock().await.database().root_id().to_string();
    assert!(
        f.registry
            .set_session_name_if_unnamed(&foreign_id, "Unauthorized".into())
            .await
            .is_err()
    );
    assert!(
        f.registry
            .find_by_name("Unauthorized")
            .await
            .unwrap()
            .is_none(),
        "no index write before key authorization"
    );
    f.ctx.session = session;
    f.mock.push_text("foreign main reply");
    assert!(f.run(vec![]).await.is_ok());
    assert_eq!(
        f.mock.recorded_calls().len(),
        2,
        "no extension observation of a session outside the registry's key scope"
    );
}

#[tokio::test]
async fn scoped_api_limits_calls_and_settings_and_supports_no_tools_turns() {
    let mut f = Fixture::new(json!({"enabled": true})).await;
    f.ctx.tools = crate::tool::ScopedTools::new(Arc::new(ToolRegistry::new()), None);
    f.mock.push_text("no-tools reply");
    f.mock.push_text("No-tools title");
    assert_eq!(
        f.run(vec![RuntimeMessage::System("full no-tools context".into())])
            .await
            .unwrap()
            .body,
        "no-tools reply"
    );
    let calls = f.mock.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].model, "turn-model",
        "default naming model is the effective turn model"
    );
    assert!(format!("{:?}", calls[1].messages).contains("full no-tools context"));

    let db = f.ctx.session.lock().await.database().clone();
    write_settings(&db, "other_extension", json!({"private": "untouched"}))
        .await
        .unwrap();
    let security = permissive_security();
    let messages = vec![RuntimeMessage::User("this session only".into())];
    let ctx = TurnCompleteContext {
        agent_name: "test-agent",
        model: Some("turn-model"),
        call_depth: 0,
        messages: &messages,
        tools: &[],
        extension: "session_naming",
        db: &db,
        registry: &f.registry,
        backend: &f.backend,
        security: &security,
        completion_used: std::sync::atomic::AtomicBool::new(false),
    };
    assert!(ctx.complete("task", None, 513).await.is_err());
    assert!(ctx.complete("task", None, 0).await.is_err());
    f.mock.push_text("auxiliary text");
    assert_eq!(
        ctx.complete("task", None, 128).await.unwrap().text,
        "auxiliary text"
    );
    assert!(ctx.complete("task", None, 128).await.is_err());
    assert_eq!(f.mock.recorded_calls().len(), 3);
    ctx.update_settings(json!({"enabled": false}))
        .await
        .unwrap();
    assert_eq!(
        read_settings(&db, "other_extension").await,
        json!({"private": "untouched"})
    );
    assert_eq!(ctx.settings().await.unwrap()["enabled"], false);
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<eidetica::store::DocStore>(crate::extension::EXTENSION_SETTINGS_STORE)
        .await
        .unwrap()
        .set_string("session_naming", "{broken")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(ctx.settings().await.is_err());
    assert!(ctx.update_settings(json!({"enabled": true})).await.is_err());
    let txn = db.new_transaction().await.unwrap();
    assert_eq!(
        txn.get_store::<eidetica::store::DocStore>(crate::extension::EXTENSION_SETTINGS_STORE)
            .await
            .unwrap()
            .get_string("session_naming")
            .await
            .unwrap(),
        "{broken"
    );

    // No session/agent pin: the backend default still has a known window.
    let mut config = crate::config::Backend::new(crate::config::BackendType::OpenAICompatible);
    config.api_base = Some("http://127.0.0.1:1/v1".into());
    config.models = Some(vec![crate::config::Model {
        name: "default-tiny".into(),
        api: None,
        reasoning: None,
        price_input: None,
        price_output: None,
        price_cache_read: None,
        context_window: Some(10),
    }]);
    let backend = BackendManager::new(&Some(vec![config]), empty_secrets().await);
    let default_ctx = TurnCompleteContext {
        model: None,
        backend: &backend,
        completion_used: std::sync::atomic::AtomicBool::new(false),
        ..ctx
    };
    let error = default_ctx.complete("task", None, 128).await.err().unwrap();
    assert!(
        error.to_string().contains("model window"),
        "refuse the full request before dispatch, not a missing-key/network error: {error}"
    );
}

#[tokio::test]
async fn read_only_session_cannot_gain_a_canonical_name_index_entry() {
    let f = Fixture::new(json!({"enabled": true})).await;
    let db = f.ctx.session.lock().await.database().clone();
    let key = f.registry.default_pubkey().await.unwrap();
    let txn = db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_auth_key(
            &key,
            eidetica::auth::types::AuthKey::active(
                Some("read-only"),
                eidetica::auth::Permission::Read,
            ),
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(!db.current_permission().await.unwrap().can_write());
    assert!(
        f.registry
            .set_session_name_if_unnamed(&f.id().await, "Unauthorized rename".into())
            .await
            .is_err()
    );
    assert!(
        f.registry
            .find_by_name("Unauthorized rename")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db.new_transaction()
            .await
            .unwrap()
            .get_store::<eidetica::store::DocStore>("meta")
            .await
            .unwrap()
            .get_string("name")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn naming_observes_the_accepted_projection_without_reinvoking_it() {
    use crate::extension::projection::{ContextProjectionGrant, ProjectionAuthority};
    use crate::test_support::projection_integration::probe;
    for with_tools in [true, false] {
        let mut f = Fixture::new(json!({"enabled": true})).await;
        if !with_tools {
            f.ctx.tools = crate::tool::ScopedTools::new(Arc::new(ToolRegistry::new()), None);
        }
        let projector = probe("naming_projection", |_, mut request| {
            request.messages[0] = RuntimeMessage::System("projected system".into());
            request.messages[1] = RuntimeMessage::User("projected user".into());
            if let Some(tool) = request.tools.first_mut() {
                tool.description = "projected tool description".into();
            }
            Ok(request)
        });
        let hub = Arc::get_mut(&mut f.hub).unwrap();
        hub.install_all(vec![projector.ext.clone()]).await.unwrap();
        hub.set_context_projection_grants(&[ContextProjectionGrant {
            extension: "naming_projection".into(),
            authority: ProjectionAuthority::FullContext,
            required: true,
        }])
        .unwrap();
        hub.record_active(f.ctx.session.lock().await.database())
            .await
            .unwrap();
        f.ctx.active_extensions.insert("naming_projection".into());
        f.mock.push_text("main reply");
        f.mock.push_text("Projected name");
        f.run(vec![
            RuntimeMessage::System("original system".into()),
            RuntimeMessage::User("original user".into()),
            RuntimeMessage::AssistantWithExtra {
                exchange_id: None,
                content: Some("public assistant history".into()),
                provider_extra: serde_json::from_value(json!({"reasoning":"HIDDEN-TEXT-HISTORY"}))
                    .unwrap(),
            },
        ])
        .await
        .unwrap();
        assert_eq!(
            projector.count(),
            1,
            "naming must not run context projectors again"
        );
        let calls = f.mock.recorded_calls();
        assert_eq!(calls.len(), 2);
        let public = crate::extension::turn::public_messages(&calls[0].messages, "main reply");
        assert_eq!(&calls[1].messages[..public.len()], public);
        let auxiliary = format!("{:?}", calls[1].messages);
        assert_eq!(auxiliary.contains("projected tool description"), with_tools);
        assert!(auxiliary.contains("public assistant history"));
        for excluded in ["original system", "original user", "HIDDEN-TEXT-HISTORY"] {
            assert!(!auxiliary.contains(excluded), "exposed {excluded}");
        }
        assert_eq!(f.settings().await["status"], "named");
    }
}

#[tokio::test]
async fn naming_refuses_corrupt_settings_and_live_revocation() {
    use eidetica::store::DocStore;
    for case in ["corrupt", "revoked"] {
        let f = Fixture::new(json!({"enabled": true})).await;
        let db = f.ctx.session.lock().await.database().clone();
        if case == "corrupt" {
            let txn = db.new_transaction().await.unwrap();
            txn.get_store::<DocStore>(crate::extension::EXTENSION_SETTINGS_STORE)
                .await
                .unwrap()
                .set_string("session_naming", "{broken")
                .await
                .unwrap();
            txn.commit().await.unwrap();
        }
        f.mock.push_text("main reply");
        let gate = f.mock.block_next_call();
        let task = tokio::spawn(async move {
            let outcome = f
                .run(vec![RuntimeMessage::User("request".into())])
                .await
                .unwrap();
            (f, outcome)
        });
        gate.wait_started().await;
        if case == "revoked" {
            crate::extension::append_event(
                &db,
                crate::extension::ExtensionEvent::Deactivated {
                    name: "session_naming".into(),
                    timestamp: chrono::Utc::now() + chrono::Duration::seconds(1),
                },
            )
            .await
            .unwrap();
        }
        gate.release();
        let (f, outcome) = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.body, "main reply");
        assert_eq!(
            f.mock.recorded_calls().len(),
            1,
            "unexpected auxiliary call for {case}"
        );
        assert!(f.ctx.session.lock().await.read_meta().await.name.is_none());
        if case == "corrupt" {
            assert!(
                crate::extension::read_settings_checked(&db, "session_naming")
                    .await
                    .is_err()
            );
        } else {
            assert!(f.settings().await.get("attempted").is_none());
        }
    }
}

#[tokio::test]
async fn resumed_turn_names_once_from_its_restored_exchange() {
    let f = Fixture::new(json!({"enabled": true})).await;
    let call = ToolCallRequest {
        id: "restored".into(),
        name: "calculate".into(),
        arguments: r#"{"expression":"6*7"}"#.into(),
    };
    f.mock.push_text("resumed reply");
    f.mock.push_text("Restored calculation");
    let now = chrono::Utc::now();
    let outcome = runtime::resume_with_recorder(
        Some("turn-model"),
        vec![RuntimeMessage::User("multiply before restart".into())],
        &f.backend,
        &permissive_security(),
        &f.ctx,
        &ToolPolicyRegistry::empty(),
        None,
        Some(&f.hub),
        None,
        runtime::RuntimeResume {
            prior_model_sequences: vec![0],
            prior_messages: vec![RuntimeMessage::AssistantToolCalls {
                exchange_id: None,
                content: Some("restored public text".into()),
                tool_calls: vec![call.clone()],
                provider_extra: serde_json::from_value(json!({"reasoning_content": "HIDDEN"}))
                    .unwrap(),
            }],
            model_sequence: 0,
            call_index: 0,
            call,
            deadline: now + chrono::Duration::seconds(30),
            policy_deadline: now + chrono::Duration::seconds(60),
            policy_timeout_seconds: 60,
            metadata: Vec::new(),
            matrix_send_queued: false,
            rate_limited_calls: Vec::new(),
        },
        runtime::ModelCallScope::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.body, "resumed reply");
    let calls = f.mock.recorded_calls();
    assert_eq!(
        calls.len(),
        2,
        "one resumed model call plus one naming call"
    );
    let public = format!("{:?}", calls[1].messages);
    for text in [
        "multiply before restart",
        "restored public text",
        "42",
        "resumed reply",
    ] {
        assert!(public.contains(text), "missing {text}: {public}");
    }
    assert!(!public.contains("HIDDEN"));
    assert_eq!(
        f.registry
            .find_by_name("Restored calculation")
            .await
            .unwrap(),
        Some(f.id().await)
    );
    assert_eq!(f.settings().await["status"], "named");
}
