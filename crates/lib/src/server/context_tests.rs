use super::*;

#[test]
fn budget_source_agrees_with_the_turn_budget() {
    const DEFAULT: usize = 128_000;
    let cases = [
        (None, Some(32_000), ContextBudgetSource::ModelWindow),
        (Some(50_000), Some(200_000), ContextBudgetSource::AgentCap),
        (
            Some(500_000),
            Some(200_000),
            ContextBudgetSource::ModelWindow,
        ),
        (
            Some(200_000),
            Some(200_000),
            ContextBudgetSource::ModelWindow,
        ),
        (Some(64_000), None, ContextBudgetSource::AgentCap),
        (None, None, ContextBudgetSource::ConfiguredDefault),
    ];
    for (cap, window, source) in cases {
        let expected = clamp_budget_to_window(cap, window).unwrap_or(DEFAULT);
        assert_eq!(
            resolve_budget_source(cap, window, DEFAULT),
            (expected, source),
            "cap={cap:?} window={window:?}"
        );
    }
}

/// Server whose hub carries the built-in `context` extension, with the
/// session's default-active extension set recorded the way
/// `fire_session_start_hook` does in production.
async fn context_extension_fixture() -> (Instance, Arc<Server>, Arc<crate::session::SessionRegistry>)
{
    let (instance, registry) = crate::test_support::fresh_session_registry().await;
    let server = context_extension_server(
        registry.clone(),
        Some(crate::instance::ExecutorCapability::for_test()),
    )
    .await;
    (instance, server, registry)
}

async fn context_extension_server(
    registry: Arc<crate::session::SessionRegistry>,
    executor: Option<crate::instance::ExecutorCapability>,
) -> Arc<Server> {
    let agents = registry.agents.clone();
    let slot = crate::instance::ServerSlot::default();
    let mut hub = crate::extension::ExtensionHub::new();
    hub.set_session_registry(registry.clone());
    hub.set_peer_handles(Arc::new(crate::extension::PeerHandles {
        registry: registry.clone(),
        agent_index: HostedIndex::empty("agent"),
        memory_bank_index: HostedIndex::empty("bank"),
        skill_bank_index: HostedIndex::empty("skill_bank"),
        embedder: None,
        secrets: None,
        server_slot: slot.clone(),
        mcp_registry: Arc::new(crate::mcp::McpRegistry::new()),
        agent_state_allowlist: Default::default(),
        tool_registry: Arc::new(ToolRegistry::new()),
    }));
    let extensions: Vec<Arc<dyn crate::extension::Extension>> = vec![
        Arc::new(crate::extensions::context::BaselineContext),
        Arc::new(crate::extensions::context_report::ContextExtension::new(
            slot.clone(),
        )),
    ];
    if executor.is_some() {
        hub.set_context_strategy_grant(&Default::default()).unwrap();
        hub.install_all(extensions).await.unwrap();
    } else {
        // Match production service/TUI clients: commands, no execution providers.
        hub.install_commands(extensions).await.unwrap();
    }
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    let server = Server::new(
        registry.clone(),
        agents,
        HostedIndex::empty("agent"),
        HostedIndex::empty("bank"),
        HostedIndex::empty("skill_bank"),
        Arc::new(ToolRegistry::new()),
        Arc::new(crate::tool::ToolPolicyRegistry::empty()),
        SecurityContext {
            leak_detector: crate::security::LeakDetector::new(
                crate::security::LeakPolicy::default(),
            ),
            auto_approved_tools: std::collections::HashSet::new(),
            approval_callback: None,
        },
        HashMap::new(),
        Default::default(),
        Arc::new(crate::tool_host::NativeToolHost::new()),
        Arc::new(hub),
        crate::backends::BackendManager::new(&None, secrets),
        Arc::new(crate::mcp::McpRegistry::new()),
        executor,
    );
    slot.set(server.clone());
    server
}

fn user_messages(call: &crate::test_support::RecordedCall) -> Vec<&str> {
    call.messages
        .iter()
        .filter_map(|m| match m {
            crate::runtime::RuntimeMessage::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn context_command_reports_and_reaches_only_the_next_request() {
    let (_instance, server, registry) = context_extension_fixture().await;
    exercise_context_command(server, registry, None).await;
}

#[tokio::test]
async fn context_command_service_client_delivers_once_without_executor_authority() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("context.sock");
    let (owner, _) = crate::test_support::fresh_eidetica().await;
    let service = eidetica::service::ServiceServer::bind(owner, &socket)
        .await
        .unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(());
    let task = tokio::spawn(service.run(receiver));
    let settings = crate::config::EideticaConfig {
        connection: format!("unix://{}", socket.display()),
        login: crate::config::EideticaLoginConfig {
            username: "test".into(),
            password: None,
            passwordless: true,
        },
        max_frame_bytes: None,
        sync: None,
    };
    let connected =
        crate::instance::connect_with(&settings, crate::config::ExecutionRole::Executor)
            .await
            .unwrap();
    let capability = connected.capabilities.executor().unwrap();
    let registry = Arc::new(
        crate::session::SessionRegistry::new(
            connected.instance,
            connected.user,
            Arc::new(AgentRegistry::with_default_agent()),
        )
        .await
        .unwrap(),
    );
    let executor = context_extension_server(registry.clone(), Some(capability)).await;
    exercise_context_command(executor, registry, Some(&settings)).await;
    drop(shutdown);
    task.await.unwrap().unwrap();
}

async fn exercise_context_command(
    server: Arc<Server>,
    registry: Arc<crate::session::SessionRegistry>,
    service: Option<&crate::config::EideticaConfig>,
) {
    use crate::commands::{Command, CommandContext, CommandOutcome, dispatch};
    use crate::runtime::{LLMResponse, ResponseMetadata, TokenUsage};
    use crate::test_support::MockBackend;

    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("ctx")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    server.extensions().record_active(&db).await.unwrap();
    server.refresh_active_extensions(&sid).await;

    let mock = Arc::new(MockBackend::new());
    mock.push_response(Ok(LLMResponse::Text {
        content: "first reply".into(),
        provider_extra: Default::default(),
        metadata: Some(ResponseMetadata {
            model: "test-model".into(),
            usage: TokenUsage {
                prompt_tokens: 45_210,
                completion_tokens: 12,
                total_tokens: 45_222,
                ..Default::default()
            },
            ..Default::default()
        }),
    }));
    mock.push_text("second reply");
    mock.push_text("third reply");
    mock.push_text("fourth reply");
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    let backend = crate::backends::BackendManager::with_mock(mock.clone(), secrets.clone());
    server
        .register_session(&db, backend.clone(), Some("alpha".into()), None)
        .await
        .unwrap();

    write_user_message_with_content(&db, &sid, "hello").await;
    await_call_count(&mock, 1).await;
    await_no_processing(&server, &sid).await;

    // A service login loads the Account's held-key view. Connect after the
    // session exists, just as opening an existing session in a fresh TUI does.
    let command_server = if let Some(settings) = service {
        let connected =
            crate::instance::connect_with(settings, crate::config::ExecutionRole::Client)
                .await
                .unwrap();
        assert!(matches!(
            connected.capabilities.executor(),
            Err(crate::instance::ConnectError::Capability(_))
        ));
        let registry = Arc::new(
            crate::session::SessionRegistry::new(
                connected.instance,
                connected.user,
                Arc::new(AgentRegistry::with_default_agent()),
            )
            .await
            .unwrap(),
        );
        context_extension_server(registry, None).await
    } else {
        server.clone()
    };
    assert_eq!(
        command_server.registry().default_pubkey().await.unwrap(),
        registry.default_pubkey().await.unwrap()
    );
    register_alpha_agent_runtime(&command_server);
    command_server
        .cache_model_info(&crate::backends::ModelInfo {
            id: "test-model".into(),
            context_window: Some(200_000),
            ..Default::default()
        })
        .await;
    let (_, command_db) = command_server.registry().open_session(&sid).await.unwrap();
    command_server.refresh_active_extensions(&sid).await;
    let client_mock = Arc::new(MockBackend::new());
    let client_backend = BackendManager::with_mock(client_mock.clone(), secrets.clone());
    let ctx = CommandContext {
        server: &command_server,
        secrets: &secrets,
        backend: &client_backend,
        session_db_id: &sid,
        session_db: &command_db,
        current_agent: "alpha",
        session_name: Some("ctx"),
    };
    let before = command_db.snapshot().await.unwrap();
    assert!(
        matches!(dispatch(Command::Extension { name: "context".into(), args: "unexpected".into() }, &ctx).await,
        CommandOutcome::Error(message) if message == "Usage: /context (takes no arguments)")
    );
    assert_eq!(
        before,
        command_db.snapshot().await.unwrap(),
        "invalid command must not persist a report"
    );
    let readonly =
        eidetica::Database::open(command_server.registry().instance(), command_db.root_id())
            .await
            .unwrap();
    let readonly_ctx = CommandContext {
        session_db: &readonly,
        ..ctx
    };
    let denied = dispatch(
        Command::Extension {
            name: "context".into(),
            args: String::new(),
        },
        &readonly_ctx,
    )
    .await;
    assert!(
        matches!(denied, CommandOutcome::Text(message) if message.contains("Not shared with alpha: failed to record the snapshot"))
    );
    assert_eq!(
        before,
        command_db.snapshot().await.unwrap(),
        "unsigned client must not persist a report"
    );
    let shown = match dispatch(
        Command::Extension {
            name: "context".into(),
            args: String::new(),
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Text(text) => text,
        CommandOutcome::Error(e) => panic!("/context failed: {e}"),
        _ => panic!("unexpected /context outcome"),
    };
    for expected in [
        "Context for agent 'alpha'",
        "Last completed request: 45,210 input tokens",
        "on test-model",
        "Next request: not yet measured; 0 transcript entries added since the measurement",
        "Context budget: 200,000 tokens for test-model — model catalog window; 4,096 reserved for the reply",
        "Model window: 200,000 tokens",
        "Used: 45,210 / 200,000 = 22.6% of budget",
        "alpha will see this snapshot on its next turn",
    ] {
        assert!(
            shown.contains(expected),
            "missing {expected:?} in:\n{shown}"
        );
    }
    let report = shown.split("\n\n(").next().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        mock.recorded_calls().len(),
        1,
        "/context must not wake the agent"
    );

    // A pending snapshot waits while the extension is disabled; neither a
    // disabled command nor the intervening model turn consumes it.
    let removed = dispatch(parsed_command("/extensions remove context"), &ctx).await;
    assert!(matches!(removed, CommandOutcome::Text(message) if message.contains("Deactivated")));
    server.refresh_active_extensions(&sid).await;
    assert!(
        matches!(dispatch(Command::Extension { name: "context".into(), args: String::new() }, &ctx).await,
        CommandOutcome::Error(message) if message.contains("not active on this session"))
    );
    write_user_message_with_content(&db, &sid, "while disabled").await;
    await_call_count(&mock, 2).await;
    await_no_processing(&server, &sid).await;
    assert!(
        !user_messages(&mock.recorded_calls()[1])
            .iter()
            .any(|m| m.contains("/context diagnostic snapshot"))
    );
    let added = dispatch(parsed_command("/extensions add context"), &ctx).await;
    assert!(matches!(added, CommandOutcome::Text(message) if message.contains("Activated")));
    server.refresh_active_extensions(&sid).await;

    write_user_message_with_content(&db, &sid, "what did /context say?").await;
    await_call_count(&mock, 3).await;
    await_no_processing(&server, &sid).await;
    let next = &mock.recorded_calls()[2];
    let tail = *user_messages(next).last().unwrap();
    assert!(
        tail.starts_with("[/context diagnostic snapshot"),
        "next request must end with the framed snapshot, got:\n{tail}"
    );
    assert!(
        tail.ends_with(report),
        "snapshot differs from the shown report:\n{tail}"
    );
    assert!(
        user_messages(next)
            .iter()
            .any(|text| text.contains("what did /context say?"))
    );

    write_user_message_with_content(&db, &sid, "and now?").await;
    await_call_count(&mock, 4).await;
    await_no_processing(&server, &sid).await;
    assert!(
        client_mock.recorded_calls().is_empty(),
        "the command client must never call its model"
    );
    let later = &mock.recorded_calls()[3];
    assert!(
        !user_messages(later)
            .iter()
            .any(|m| m.contains("/context diagnostic snapshot")),
        "snapshot must be delivered to one turn only"
    );
    server.shutdown().await;
    command_server.shutdown().await;
}

fn parsed_command(text: &str) -> crate::commands::Command {
    let crate::commands::Parsed::Command(command) = crate::commands::parse(text) else {
        panic!("expected a command: {text}");
    };
    command
}
