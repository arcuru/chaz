//! Unit tests for the agent server. Extracted from `mod.rs`.

use super::*;

#[test]
fn budget_clamps_to_model_window() {
    // Known window drives the budget — no static default caps it. A small
    // window prevents overflow; a large one is used in full (the 1M model
    // no longer truncates at 128k).
    assert_eq!(clamp_budget_to_window(None, Some(32_000)), Some(32_000));
    assert_eq!(
        clamp_budget_to_window(None, Some(1_000_000)),
        Some(1_000_000)
    );
    // An explicit agent cap lower than the window holds (cost control).
    assert_eq!(
        clamp_budget_to_window(Some(50_000), Some(200_000)),
        Some(50_000)
    );
    // An agent cap above the window cannot raise past it — window is a ceiling.
    assert_eq!(
        clamp_budget_to_window(Some(500_000), Some(200_000)),
        Some(200_000)
    );
    // Unknown window: pass the agent cap through untouched (None => builder default).
    assert_eq!(clamp_budget_to_window(None, None), None);
    assert_eq!(clamp_budget_to_window(Some(64_000), None), Some(64_000));
}

#[tokio::test]
async fn budget_model_falls_back_to_backend_default() {
    let (_instance, _server, registry) = server_fixture().await;
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;

    // Single backend whose first (default) model is flash — the Chaz shape.
    let mut b = crate::config::Backend::new(crate::config::BackendType::OpenAICompatible);
    b.name = Some("openrouter".to_string());
    b.models = Some(vec![crate::config::Model {
        name: "deepseek/deepseek-v4-flash".to_string(),
        reasoning: None,
        price_input: None,
        price_output: None,
        price_cache_read: None,
        context_window: None,
    }]);
    let backend = crate::backends::BackendManager::new(&Some(vec![b]), secrets.clone());

    // No session pin, no agent default → resolve to the backend default
    // instead of None. This is the fix: previously `None` here meant the
    // window fetch never fired and budgeting fell to the 128k static default.
    assert_eq!(
        budget_model_id(&backend, None, None).as_deref(),
        Some("deepseek/deepseek-v4-flash")
    );
    // An agent default still wins over the backend default.
    assert_eq!(
        budget_model_id(&backend, None, Some("pinned")).as_deref(),
        Some("pinned")
    );
    // A session pin wins over both.
    assert_eq!(
        budget_model_id(&backend, Some("sess"), Some("pinned")).as_deref(),
        Some("sess")
    );
    // No backends configured → nothing to fall back to.
    let empty = crate::backends::BackendManager::new(&None, secrets);
    assert_eq!(budget_model_id(&empty, None, None), None);
}

use crate::agent::AgentRegistry;
use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
use crate::hosted_index::DbEntry;
use eidetica::backend::database::InMemory;
use eidetica::{Instance, NewUser};

/// Build a Server with the minimum wiring needed to exercise hydration.
async fn server_fixture() -> (Instance, Arc<Server>, Arc<crate::session::SessionRegistry>) {
    let backend = InMemory::new();
    let (instance, user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();
    let agents = Arc::new(AgentRegistry::with_default_agent());
    let registry = Arc::new(
        crate::session::SessionRegistry::new(instance.clone(), user, agents.clone())
            .await
            .unwrap(),
    );
    let index = HostedIndex::empty("agent");
    let bank_index = HostedIndex::empty("bank");
    let tools = Arc::new(ToolRegistry::new());
    let policies = Arc::new(crate::tool::ToolPolicyRegistry::empty());
    let security = SecurityContext {
        leak_detector: crate::security::LeakDetector::new(crate::security::LeakPolicy::default()),
        auto_approved_tools: std::collections::HashSet::new(),
        approval_callback: None,
    };
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    let default_backend = crate::backends::BackendManager::new(&None, secrets);
    let server = Server::new(
        registry.clone(),
        agents,
        index,
        bank_index,
        crate::hosted_index::HostedIndex::empty("skill_bank"),
        tools,
        policies,
        security,
        HashMap::new(),
        Default::default(),
        Arc::new(crate::tool_host::NativeToolHost::new()),
        Arc::new(crate::extension::ExtensionHub::new()),
        default_backend,
        Arc::new(crate::mcp::McpRegistry::new()),
        Some(crate::instance::ExecutorCapability::for_test()),
    );
    (instance, server, registry)
}

async fn server_fixture_from_registry(
    registry: Arc<crate::session::SessionRegistry>,
) -> (Instance, Arc<Server>, Arc<crate::session::SessionRegistry>) {
    let instance = registry.instance().clone();
    let agents = registry.agents.clone();
    let security = SecurityContext {
        leak_detector: crate::security::LeakDetector::new(crate::security::LeakPolicy::default()),
        auto_approved_tools: std::collections::HashSet::new(),
        approval_callback: None,
    };
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    let default_backend = crate::backends::BackendManager::new(&None, secrets);
    let server = Server::new(
        registry.clone(),
        agents,
        HostedIndex::empty("agent"),
        HostedIndex::empty("bank"),
        HostedIndex::empty("skill_bank"),
        Arc::new(ToolRegistry::new()),
        Arc::new(crate::tool::ToolPolicyRegistry::empty()),
        security,
        HashMap::new(),
        Default::default(),
        Arc::new(crate::tool_host::NativeToolHost::new()),
        Arc::new(crate::extension::ExtensionHub::new()),
        default_backend,
        Arc::new(crate::mcp::McpRegistry::new()),
        Some(crate::instance::ExecutorCapability::for_test()),
    );
    (instance, server, registry)
}

async fn client_server_fixture_from_registry(
    registry: Arc<crate::session::SessionRegistry>,
) -> Arc<Server> {
    let agents = registry.agents.clone();
    let security = SecurityContext {
        leak_detector: crate::security::LeakDetector::new(crate::security::LeakPolicy::default()),
        auto_approved_tools: std::collections::HashSet::new(),
        approval_callback: None,
    };
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    Server::new(
        registry,
        agents,
        HostedIndex::empty("agent"),
        HostedIndex::empty("bank"),
        HostedIndex::empty("skill_bank"),
        Arc::new(ToolRegistry::new()),
        Arc::new(crate::tool::ToolPolicyRegistry::empty()),
        security,
        HashMap::new(),
        Default::default(),
        Arc::new(crate::tool_host::NativeToolHost::new()),
        Arc::new(crate::extension::ExtensionHub::new()),
        crate::backends::BackendManager::new(&None, secrets),
        Arc::new(crate::mcp::McpRegistry::new()),
        None,
    )
}

#[tokio::test]
async fn shutdown_aborts_an_in_flight_model_turn_before_replacement() {
    let (_instance, server, registry) = server_fixture().await;
    let (sid, db) = registry
        .create_session(Some("shutdown-test"))
        .await
        .unwrap();
    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("must not commit after shutdown");
    let gate = mock.block_next_call();
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, backend, Some("agent".into()), None)
        .await
        .unwrap();
    let mut session = Session::new(sid, db.clone()).await;
    session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "block this turn".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), gate.wait_started())
        .await
        .expect("model turn must start");

    tokio::time::timeout(std::time::Duration::from_secs(2), server.shutdown())
        .await
        .expect("shutdown must await cancellation");
    tokio::time::timeout(std::time::Duration::from_secs(2), gate.wait_stopped())
        .await
        .expect("shutdown must cancel the old model future before returning");

    let observed = Session::new(ConversationId(db.root_id().to_string()), db).await;
    assert!(
        !observed
            .entries()
            .iter()
            .any(|entry| entry.content == "must not commit after shutdown")
    );
}

#[tokio::test]
async fn hydrate_picks_up_db_config_edits() {
    let (_instance, server, registry) = server_fixture().await;

    // Create an Agent DB with V1 config: haiku / 5 iters.
    let (db, pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            "alpha",
            &AgentDbConfig {
                model: Some("haiku".to_string()),
                max_iterations: Some(5),
                ..Default::default()
            },
            &AgentMeta {
                display_name: Some("alpha".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    server.agent_index().register(DbEntry {
        db_id: db.id(),
        display_name: "alpha".to_string(),
        pubkey,
    });

    // Seed the in-memory registry with a stale entry (model="opus", iter=999)
    // — exactly what would happen if yaml drifted from DB, or if a prior
    // hydration happened before a DB edit.
    let mut stale = crate::agent::Agent {
        name: "alpha".to_string(),
        system_prompt: String::new(),
        system_prompt_files: vec![],
        default_model: Some("opus".to_string()),
        allowed_tools: None,
        workers: HashMap::new(),
        max_iterations: 999,
        autonomous: false,
        presets: HashMap::new(),
        tool_profile: None,
        max_context_tokens: None,
        capabilities: crate::grants::Grants::default(),
        grants: HashMap::new(),
    };
    server.agents().upsert(stale.clone());

    // First hydrate: should pick up V1 from DB (haiku / 5).
    let input = stale.clone();
    let hydrated = server.hydrate_agent_from_db(input).await;
    assert_eq!(hydrated.default_model.as_deref(), Some("haiku"));
    assert_eq!(hydrated.max_iterations, 5);
    // And the registry reflects the live state too.
    assert_eq!(
        server
            .agents()
            .get("alpha")
            .unwrap()
            .default_model
            .as_deref(),
        Some("haiku")
    );

    // Write V2 to the DB.
    db.write_config(&AgentDbConfig {
        model: Some("sonnet".to_string()),
        max_iterations: Some(42),
        ..Default::default()
    })
    .await
    .unwrap();

    stale.default_model = Some("opus".to_string()); // re-enter with stale snapshot
    let hydrated_v2 = server.hydrate_agent_from_db(stale).await;
    assert_eq!(hydrated_v2.default_model.as_deref(), Some("sonnet"));
    assert_eq!(hydrated_v2.max_iterations, 42);
    assert_eq!(
        server
            .agents()
            .get("alpha")
            .unwrap()
            .default_model
            .as_deref(),
        Some("sonnet")
    );
}

#[tokio::test]
async fn hydrate_returns_input_when_agent_not_in_index() {
    let (_instance, server, _registry) = server_fixture().await;

    // No DB for "phantom"; hydration should return the input unchanged.
    let input = crate::agent::Agent {
        name: "phantom".to_string(),
        system_prompt: String::new(),
        system_prompt_files: vec![],
        default_model: Some("ghost".to_string()),
        allowed_tools: None,
        workers: HashMap::new(),
        max_iterations: 7,
        autonomous: false,
        presets: HashMap::new(),
        tool_profile: None,
        max_context_tokens: None,
        capabilities: crate::grants::Grants::default(),
        grants: HashMap::new(),
    };
    let result = server.hydrate_agent_from_db(input.clone()).await;
    assert_eq!(result.name, "phantom");
    assert_eq!(result.default_model.as_deref(), Some("ghost"));
    assert_eq!(result.max_iterations, 7);
}

#[tokio::test]
async fn reconcile_resolves_prompt_into_blob_and_is_gated() {
    let (_instance, server, registry) = server_fixture().await;

    // A yaml agent whose entire system prompt comes from a file (no inline
    // `system_prompt`) — the exact shape of the Chaz config.
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("AGENTS.md");
    std::fs::write(&prompt_path, "You are Chaz. Operating manual v1.").unwrap();
    let ac: crate::config::AgentConfig = serde_yaml::from_str(&format!(
        "name: chaz\nsystem_prompt_files: [\"{}\"]\n",
        prompt_path.display()
    ))
    .unwrap();

    // Bootstrap the agent DB the way startup would: declarative config
    // (paths), but no resolved-prompt ref yet.
    let (db, pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            "chaz",
            &crate::agent_db::AgentDbConfig::from_agent_config(&ac),
            &AgentMeta {
                display_name: Some("chaz".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    server.agent_index().register(DbEntry {
        db_id: db.id(),
        display_name: "chaz".to_string(),
        pubkey,
    });

    // First reconcile applies and sets the prompt ref.
    assert!(server.reconcile_agent_from_yaml(&ac).await.unwrap());
    let cfg = db.read_config().await.unwrap();
    assert!(cfg.system_prompt_ref.is_some(), "ref set after reconcile");
    assert!(cfg.applied_config_hash.is_some());

    // Hydration resolves the prompt from the blob (config has no inline text).
    let input = crate::agent::Agent {
        name: "chaz".to_string(),
        system_prompt: String::new(),
        system_prompt_files: vec![],
        default_model: None,
        allowed_tools: None,
        workers: HashMap::new(),
        max_iterations: 10,
        autonomous: false,
        presets: HashMap::new(),
        tool_profile: None,
        max_context_tokens: None,
        capabilities: crate::grants::Grants::default(),
        grants: HashMap::new(),
    };
    let hydrated = server.hydrate_agent_from_db(input.clone()).await;
    assert_eq!(hydrated.system_prompt, "You are Chaz. Operating manual v1.");

    // Unchanged yaml + file → gate matches → no-op.
    assert!(!server.reconcile_agent_from_yaml(&ac).await.unwrap());

    // Editing the file content makes the resolved prompt change, so
    // reconcile applies again and hydration reflects the new text.
    std::fs::write(&prompt_path, "You are Chaz. Operating manual v2!").unwrap();
    assert!(server.reconcile_agent_from_yaml(&ac).await.unwrap());
    let hydrated2 = server.hydrate_agent_from_db(input).await;
    assert_eq!(
        hydrated2.system_prompt,
        "You are Chaz. Operating manual v2!"
    );
}

#[tokio::test]
async fn reload_config_for_rereads_yaml_from_disk() {
    // `/agent reload` path: a config file on disk drives the reconcile via
    // the server-held config path, not a pre-parsed Config in hand.
    let (_instance, server, registry) = server_fixture().await;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("AGENTS.md");
    std::fs::write(&prompt_path, "Chaz manual v1.").unwrap();
    let config_path = dir.path().join("config.yaml");
    let write_config = |body: &str| {
        std::fs::write(
                &config_path,
                format!(
                    "homeserver_url: http://localhost\nusername: test\nagents:\n  - name: chaz\n    system_prompt_files: [\"{}\"]\n{}",
                    prompt_path.display(),
                    body
                ),
            )
            .unwrap();
    };
    write_config("");
    server.set_config_path(config_path.clone());

    // Bootstrap the agent DB the way startup would.
    let ac: crate::config::AgentConfig = serde_yaml::from_str(&format!(
        "name: chaz\nsystem_prompt_files: [\"{}\"]\n",
        prompt_path.display()
    ))
    .unwrap();
    let (db, pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            "chaz",
            &crate::agent_db::AgentDbConfig::from_agent_config(&ac),
            &AgentMeta {
                display_name: Some("chaz".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    server.agent_index().register(DbEntry {
        db_id: db.id(),
        display_name: "chaz".to_string(),
        pubkey,
    });

    // Scoped reload applies and reports the change.
    let report = server.reload_config_for(Some("chaz")).await.unwrap();
    assert_eq!(report.changed, vec!["chaz".to_string()]);
    assert_eq!(report.considered, 1);

    // A second reload with the file unchanged is a gated no-op.
    let report2 = server.reload_config_for(Some("chaz")).await.unwrap();
    assert!(report2.changed.is_empty());
    assert_eq!(report2.considered, 1);

    // Editing the prompt file and reloading reaches hydration.
    std::fs::write(&prompt_path, "Chaz manual v2!").unwrap();
    let report3 = server.reload_config_for(None).await.unwrap();
    assert_eq!(report3.changed, vec!["chaz".to_string()]);
    let input = crate::agent::Agent {
        name: "chaz".to_string(),
        system_prompt: String::new(),
        system_prompt_files: vec![],
        default_model: None,
        allowed_tools: None,
        workers: HashMap::new(),
        max_iterations: 10,
        autonomous: false,
        presets: HashMap::new(),
        tool_profile: None,
        max_context_tokens: None,
        capabilities: crate::grants::Grants::default(),
        grants: HashMap::new(),
    };
    let hydrated = server.hydrate_agent_from_db(input).await;
    assert_eq!(hydrated.system_prompt, "Chaz manual v2!");

    // A name that isn't in the yaml is considered zero times.
    let missing = server.reload_config_for(Some("ghost")).await.unwrap();
    assert_eq!(missing.considered, 0);
    assert!(missing.changed.is_empty());
}

#[tokio::test]
async fn reload_config_for_errors_without_config_path() {
    let (_instance, server, _registry) = server_fixture().await;
    // No set_config_path call → reload is unavailable.
    assert!(server.reload_config_for(None).await.is_err());
}

// -----------------------------------------------------------------
// Agent-Owned Schedule integration tests
// -----------------------------------------------------------------
//
// These tests exercise `fire_agent_schedule` through the full plumbing
// (session creation, agent attachment, schedule-fire audit, one-shot
// cleanup, processing lock). The LLM call fails deterministically
// (empty backend), which is fine — the plumbing around the call is
// what we're testing.

use crate::agent_db::Schedule;
use crate::routine::{AgentSchedulePayload, Trigger};

/// Create an agent DB, register it in the HostedIndex, seed its
/// config, and return the DbEntry and AgentDb handle.
async fn seed_agent(
    server: &Server,
    registry: &crate::session::SessionRegistry,
    name: &str,
) -> (DbEntry, crate::agent_db::AgentDb) {
    let (adb, pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            name,
            &AgentDbConfig {
                model: Some("test-model".to_string()),
                ..Default::default()
            },
            &AgentMeta {
                display_name: Some(name.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    let entry = DbEntry {
        db_id: adb.id(),
        display_name: name.to_string(),
        pubkey,
    };
    server.agent_index().register(entry.clone());
    (entry, adb)
}

/// Build an `AgentSchedulePayload` for a Fresh (non-recurring) schedule.
fn fresh_schedule_payload(
    owner_agent_db_id: &str,
    schedule_id: &str,
    prompt: &str,
) -> AgentSchedulePayload {
    AgentSchedulePayload {
        owner_agent_db_id: owner_agent_db_id.to_string(),
        schedule_id: schedule_id.to_string(),
        generation: None,
        prompt: prompt.to_string(),
        target: serde_json::to_value(crate::agent_db::ScheduleTarget::Fresh).unwrap(),
        one_shot: true,
    }
}

/// Build an `AgentSchedulePayload` for a Pinned schedule.
fn pinned_schedule_payload(
    owner_agent_db_id: &str,
    schedule_id: &str,
    prompt: &str,
    session_db_id: &str,
) -> AgentSchedulePayload {
    AgentSchedulePayload {
        owner_agent_db_id: owner_agent_db_id.to_string(),
        schedule_id: schedule_id.to_string(),
        generation: None,
        prompt: prompt.to_string(),
        target: serde_json::to_value(crate::agent_db::ScheduleTarget::Pinned {
            session_db_id: session_db_id.to_string(),
        })
        .unwrap(),
        one_shot: true,
    }
}

#[tokio::test]
async fn agent_schedule_host_check_skips_non_hosted() {
    let (_instance, server, registry) = server_fixture().await;

    // Create an agent DB but DON'T register it in the hosted index —
    // its ID is valid but find_by_id will return None.
    let (adb, _pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            "ghost",
            &AgentDbConfig::default(),
            &AgentMeta {
                display_name: Some("ghost".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    let unhosted_id = adb.id().to_string();

    let payload = fresh_schedule_payload(&unhosted_id, "t1", "wake up");
    let result = server.fire_agent_schedule(payload).await;
    assert!(
        result.is_ok(),
        "host check should return Ok(()) — just skip: {result:?}"
    );
    // No sessions should have been created.
    let sessions = registry.list_sessions().await.unwrap_or_default();
    assert!(
        !sessions.iter().any(|s| {
            s.source
                .as_deref()
                .is_some_and(|src| src.contains("ghost") || src.contains("schedule:"))
        }),
        "no schedule session should exist for a non-hosted agent"
    );
}

#[tokio::test]
async fn agent_schedule_fresh_creates_session_and_attaches_agent() {
    let (_instance, server, registry) = server_fixture().await;

    // Seed an agent.
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;

    // Add a schedule to the agent DB.
    adb.upsert_schedule(Schedule::new(
        "morning".to_string(),
        Trigger::OneShot {
            fire_at: chrono::Utc::now(),
        },
        "good morning".to_string(),
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();

    let payload = fresh_schedule_payload(&entry.db_id.to_string(), "morning", "good morning");
    let result = server.fire_agent_schedule(payload).await;
    // LLM call fails (no backends), but the plumbing should succeed.
    // Errors from the LLM are propagated through the outcome.
    match result {
        Ok(()) => {} // if somehow it succeeded, that's fine too
        Err(e) => assert!(
            e.to_string().contains("No backends configured"),
            "expected backend error, got: {e}"
        ),
    }

    // Verify a Fresh session was created with the correct source tag.
    let sessions = registry.list_sessions().await.unwrap_or_default();
    let schedule_session = sessions
        .iter()
        .find(|s| {
            s.source
                .as_deref()
                .is_some_and(|src| src.starts_with("schedule:"))
        })
        .expect("a schedule session should exist");
    assert!(
        schedule_session
            .source
            .as_deref()
            .is_some_and(|s| s.contains("morning")),
        "session source should contain schedule id"
    );

    // Verify the agent is attached to the session.
    let (_conv, session_db) = registry
        .open_session(&schedule_session.session_db_id)
        .await
        .unwrap();
    let session = Session::new(
        ConversationId(schedule_session.session_db_id.clone()),
        session_db,
    )
    .await;
    let meta = session.read_meta().await;
    assert!(
        meta.agents.iter().any(|a| a.display_name == "alpha"),
        "agent should be attached to the fresh session: {:?}",
        meta.agents
    );

    // Verify ScheduleFire was recorded in the agent DB.
    let fires = adb.list_schedule_fires().await.unwrap();
    assert_eq!(fires.len(), 1, "one ScheduleFire should be recorded");
    let fire = &fires[0];
    assert_eq!(fire.schedule_id, "morning");
    assert!(fire.fresh, "should be marked as fresh");
    assert_eq!(
        fire.session_db_id, schedule_session.session_db_id,
        "fire should reference the created session"
    );

    // One-shot: schedule should be deleted.
    let remaining = adb.list_schedules().await.unwrap();
    assert!(
        remaining.is_empty(),
        "one-shot schedule should be deleted after fire, got {} schedules",
        remaining.len()
    );
}

#[tokio::test]
async fn agent_schedule_pinned_reuses_existing_session() {
    let (_instance, server, registry) = server_fixture().await;

    // Seed an agent.
    let (entry, adb) = seed_agent(&server, &registry, "beta").await;

    // Create a session, register it with the server, attach the agent.
    // register_session is what real callers (bridges) do; without it
    // the closed-session retirement check at fire time would self-skip.
    let (_conv, session_db) = registry.create_session(Some("chat")).await.unwrap();
    let session_db_id = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&session_db_id, &entry)
        .await
        .unwrap();
    let backend = crate::backends::BackendManager::new(
        &None,
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&session_db, backend, Some("beta".to_string()), None)
        .await
        .unwrap();

    // Add a Pinned schedule targeting this session.
    adb.upsert_schedule(Schedule::new(
        "checkin".to_string(),
        Trigger::OneShot {
            fire_at: chrono::Utc::now(),
        },
        "checking in".to_string(),
        crate::agent_db::ScheduleTarget::Pinned {
            session_db_id: session_db_id.clone(),
        },
    ))
    .await
    .unwrap();

    let session_count_before = registry.list_sessions().await.unwrap_or_default().len();

    let payload = pinned_schedule_payload(
        &entry.db_id.to_string(),
        "checkin",
        "checking in",
        &session_db_id,
    );
    let result = server.fire_agent_schedule(payload).await;
    match result {
        Ok(()) => {}
        Err(e) => assert!(e.to_string().contains("No backends configured"), "{e}"),
    }

    // No new session should have been created.
    let session_count_after = registry.list_sessions().await.unwrap_or_default().len();
    assert_eq!(
        session_count_before, session_count_after,
        "Pinned fire should not create a new session"
    );

    // ScheduleFire should still be recorded.
    let fires = adb.list_schedule_fires().await.unwrap();
    assert_eq!(fires.len(), 1);
    assert!(!fires[0].fresh, "should NOT be marked as fresh");
    assert_eq!(fires[0].session_db_id, session_db_id);
}

#[tokio::test]
async fn agent_schedule_pinned_closed_session_self_disables() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "epsilon").await;

    // Create + register the session, attach the agent.
    let (_conv, session_db) = registry.create_session(Some("chat")).await.unwrap();
    let session_db_id = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&session_db_id, &entry)
        .await
        .unwrap();
    let backend = crate::backends::BackendManager::new(
        &None,
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&session_db, backend, Some("epsilon".to_string()), None)
        .await
        .unwrap();

    // Add a Pinned schedule targeting this session.
    adb.upsert_schedule(Schedule::new(
        "checkin".to_string(),
        Trigger::OneShot {
            fire_at: chrono::Utc::now(),
        },
        "checking in".to_string(),
        crate::agent_db::ScheduleTarget::Pinned {
            session_db_id: session_db_id.clone(),
        },
    ))
    .await
    .unwrap();

    // Close the session.
    server.deregister_session(&session_db_id).await;
    assert!(!server.is_session_open(&session_db_id).await);

    // Fire the schedule — should self-skip cleanly (no LLM call, no
    // ScheduleFire), and the schedule row should be persistently disabled.
    let payload = pinned_schedule_payload(
        &entry.db_id.to_string(),
        "checkin",
        "checking in",
        &session_db_id,
    );
    server.fire_agent_schedule(payload).await.unwrap();

    let fires = adb.list_schedule_fires().await.unwrap();
    assert!(
        fires.is_empty(),
        "closed-session fire should be skipped, got {} fires",
        fires.len()
    );

    let schedule = adb
        .find_schedule("checkin")
        .await
        .unwrap()
        .expect("schedule row should still exist (soft-disabled, not deleted)");
    assert!(
        !schedule.enabled,
        "Pinned schedule targeting closed session should self-disable"
    );
}

#[tokio::test]
async fn agent_schedule_processing_lock_skips_busy_session() {
    let (_instance, server, registry) = server_fixture().await;

    let (entry, _adb) = seed_agent(&server, &registry, "gamma").await;

    // Create a session and attach the agent.
    let (_conv, session_db) = registry.create_session(Some("chat")).await.unwrap();
    let session_db_id = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&session_db_id, &entry)
        .await
        .unwrap();

    // Manually insert the session into the processing set to simulate
    // a busy session.
    server.processing.lock().await.insert(session_db_id.clone());

    let payload = pinned_schedule_payload(&entry.db_id.to_string(), "t1", "wake", &session_db_id);
    let result = server.fire_agent_schedule(payload).await;
    assert!(result.is_ok(), "busy session should be skipped gracefully");

    // The lock should still be held (we inserted it manually).
    assert!(server.processing.lock().await.contains(&session_db_id));
    // Clean up.
    server.processing.lock().await.remove(&session_db_id);
}

#[tokio::test]
async fn agent_schedule_records_fire_even_on_llm_failure() {
    let (_instance, server, registry) = server_fixture().await;

    let (entry, adb) = seed_agent(&server, &registry, "delta").await;

    adb.upsert_schedule(Schedule::new(
        "f1",
        Trigger::Interval {
            period: std::time::Duration::from_secs(60),
        },
        "do thing",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();

    let payload = fresh_schedule_payload(&entry.db_id.to_string(), "f1", "do thing");
    let _ = server.fire_agent_schedule(payload).await;

    // ScheduleFire should be recorded regardless of LLM outcome.
    let fires = adb.list_schedule_fires().await.unwrap();
    assert_eq!(
        fires.len(),
        1,
        "ScheduleFire should be recorded even on failure"
    );
    assert_eq!(fires[0].schedule_id, "f1");
    // Usage metadata will be None since the LLM call failed.
    assert!(fires[0].usage.is_none());
}

#[tokio::test]
async fn schedule_admission_serially_reserves_and_retires_at_max_fires() {
    let (_instance, server, registry) = server_fixture().await;
    let (_entry, adb) = seed_agent(&server, &registry, "accounting-serial").await;
    let mut schedule = Schedule::new(
        "recurring",
        Trigger::Cron {
            expr: "0 0 * * * *".into(),
        },
        "wake",
        crate::agent_db::ScheduleTarget::Fresh,
    );
    schedule.max_fires = Some(3);
    adb.upsert_schedule(schedule).await.unwrap();

    // Three admissions spend the budget; the third retires the row.
    for expected_count in 1..=3u32 {
        match server
            .try_admit_schedule_fire(&adb, "recurring", Utc::now(), true)
            .await
            .unwrap()
        {
            crate::server::schedule::ScheduleAdmission::Admitted { retired_now } => {
                assert_eq!(retired_now.is_some(), expected_count == 3);
            }
            crate::server::schedule::ScheduleAdmission::Retired(_) => {
                panic!("admission {expected_count} of 3 must succeed")
            }
        }
        let schedule = adb.find_schedule("recurring").await.unwrap().unwrap();
        assert_eq!(schedule.fire_count, expected_count);
    }

    let schedule = adb.find_schedule("recurring").await.unwrap().unwrap();
    assert_eq!(schedule.fire_count, 3);
    assert!(!schedule.enabled);

    // A fourth admission is denied — the cap holds serially too.
    assert!(
        matches!(
            server
                .try_admit_schedule_fire(&adb, "recurring", Utc::now(), true)
                .await
                .unwrap(),
            crate::server::schedule::ScheduleAdmission::Retired(_)
        ),
        "admission past max_fires must retire-skip"
    );
}

#[tokio::test]
async fn schedule_admission_rejects_missing_and_disabled_rows() {
    let (_instance, server, registry) = server_fixture().await;
    let (_entry, adb) = seed_agent(&server, &registry, "alpha").await;

    assert!(matches!(
        server
            .try_admit_schedule_fire(&adb, "missing", Utc::now(), true)
            .await
            .unwrap(),
        crate::server::schedule::ScheduleAdmission::Retired(reason)
            if reason == "schedule was removed"
    ));

    let mut schedule = Schedule::new(
        "disabled",
        Trigger::Interval {
            period: std::time::Duration::from_secs(60),
        },
        "check",
        crate::agent_db::ScheduleTarget::Fresh,
    );
    schedule.enabled = false;
    adb.upsert_schedule(schedule).await.unwrap();
    assert!(matches!(
        server
            .try_admit_schedule_fire(&adb, "disabled", Utc::now(), true)
            .await
            .unwrap(),
        crate::server::schedule::ScheduleAdmission::Retired(reason)
            if reason == "schedule is disabled"
    ));
}

#[tokio::test]
async fn schedule_admission_caps_overlapping_turns_at_max_fires() {
    // Hard-cap regression test: 12 concurrent overlapping admissions
    // against max_fires=3 must admit exactly 3 — the reservation happens
    // under the per-schedule lock, so no two turns can both pass at
    // count 0.
    let (_instance, server, registry) = server_fixture().await;
    let (_entry, adb) = seed_agent(&server, &registry, "accounting-concurrent").await;
    let mut schedule = Schedule::new(
        "recurring",
        Trigger::Cron {
            expr: "0 0 * * * *".into(),
        },
        "wake",
        crate::agent_db::ScheduleTarget::Fresh,
    );
    schedule.max_fires = Some(3);
    adb.upsert_schedule(schedule).await.unwrap();

    const ATTEMPTS: u32 = 12;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..ATTEMPTS {
        let server = server.clone();
        let adb = adb.clone();
        tasks.spawn(async move {
            server
                .try_admit_schedule_fire(&adb, "recurring", Utc::now(), true)
                .await
        });
    }
    let mut admitted = 0u32;
    let mut retired = 0u32;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap().unwrap() {
            crate::server::schedule::ScheduleAdmission::Admitted { .. } => admitted += 1,
            crate::server::schedule::ScheduleAdmission::Retired(_) => retired += 1,
        }
    }
    assert_eq!(admitted, 3, "exactly max_fires admissions may succeed");
    assert_eq!(retired, ATTEMPTS - 3);

    let schedule = adb.find_schedule("recurring").await.unwrap().unwrap();
    assert_eq!(schedule.fire_count, 3);
    assert!(!schedule.enabled);
}

#[tokio::test]
async fn schedule_overlapping_fires_respect_max_fires_end_to_end() {
    // Full-path hard-cap proof: 12 concurrent overlapping
    // `fire_agent_schedule` turns against max_fires=3 must run exactly
    // 3 turns. The pre-fire admission (not post-hoc counting) is the
    // enforcement point, so this holds no matter how the turns
    // interleave. (The test backend fails every turn, so the 3 admitted
    // turns refund their slots afterwards — the audit row count is the
    // stable witness: denied fires record nothing.)
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "accounting-e2e").await;
    let owner_id = entry.db_id.to_string();
    let mut schedule = Schedule::new(
        "recurring",
        Trigger::Cron {
            expr: "0 0 * * * *".into(),
        },
        "wake",
        crate::agent_db::ScheduleTarget::Fresh,
    );
    schedule.max_fires = Some(3);
    adb.upsert_schedule(schedule).await.unwrap();
    let engine = schedule_engine_for(&server, &registry, &owner_id, &adb).await;
    let routine_id = crate::routine::RoutineId::new(format!("agent:{owner_id}:recurring"));
    let generation = engine.current_generation(&routine_id).await;
    assert!(
        generation.is_some(),
        "engine must seed a generation for the schedule"
    );

    const ATTEMPTS: usize = 12;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..ATTEMPTS {
        let server = server.clone();
        let mut payload = fresh_schedule_payload(&owner_id, "recurring", "wake");
        payload.generation = generation.clone();
        // Recurring fire: one-shot payloads check the bounds but reserve
        // no slot (their row is deleted after the turn).
        payload.one_shot = false;
        tasks.spawn(async move { server.fire_agent_schedule(payload).await });
    }
    while let Some(result) = tasks.join_next().await {
        // Admitted turns err (empty test backend); denied ones return Ok.
        let _ = result.unwrap();
    }

    // Exactly max_fires turns ran: one audit row + one Fresh session each.
    // (Post-hoc counting would have run all 12.)
    let fires = adb.list_schedule_fires().await.unwrap();
    assert_eq!(
        fires.len(),
        3,
        "only max_fires overlapping turns may run, got {}",
        fires.len()
    );
    assert_eq!(
        registry.list_sessions().await.unwrap_or_default().len(),
        3,
        "each admitted Fresh turn creates exactly one session"
    );
    // All three turns failed, so all three slots refunded.
    let schedule = adb.find_schedule("recurring").await.unwrap().unwrap();
    assert_eq!(schedule.fire_count, 0);
    assert!(schedule.enabled);
}

#[tokio::test]
async fn schedule_admission_refund_frees_slot_after_failed_turn() {
    // Failed turns must not burn the "successful fires" budget: a refund
    // drops the count and re-enables a max-retired row, so the next fire
    // can spend the slot.
    let (_instance, server, registry) = server_fixture().await;
    let (_entry, adb) = seed_agent(&server, &registry, "accounting-refund").await;
    let mut schedule = Schedule::new(
        "recurring",
        Trigger::Cron {
            expr: "0 0 * * * *".into(),
        },
        "wake",
        crate::agent_db::ScheduleTarget::Fresh,
    );
    schedule.max_fires = Some(1);
    adb.upsert_schedule(schedule).await.unwrap();

    assert!(
        matches!(
            server
                .try_admit_schedule_fire(&adb, "recurring", Utc::now(), true)
                .await
                .unwrap(),
            crate::server::schedule::ScheduleAdmission::Admitted { .. }
        ),
        "first admission must succeed"
    );
    let schedule = adb.find_schedule("recurring").await.unwrap().unwrap();
    assert_eq!(schedule.fire_count, 1);
    assert!(
        !schedule.enabled,
        "admission spending the last slot retires"
    );

    server
        .refund_schedule_fire_admission(&adb, "recurring", Utc::now())
        .await
        .unwrap();
    let schedule = adb.find_schedule("recurring").await.unwrap().unwrap();
    assert_eq!(schedule.fire_count, 0);
    assert!(schedule.enabled, "refund must revive a max-retired row");

    assert!(
        matches!(
            server
                .try_admit_schedule_fire(&adb, "recurring", Utc::now(), true)
                .await
                .unwrap(),
            crate::server::schedule::ScheduleAdmission::Admitted { .. }
        ),
        "refunded slot must be re-admissible"
    );
}

// ---- Home-peer gate ---------------------------------------------------

fn make_agent_ref(db_id: &str, home: Option<&str>) -> crate::session::AgentRef {
    crate::session::AgentRef {
        db_id: db_id.to_string(),
        display_name: "x".to_string(),
        home_pubkey: home.map(str::to_string),
    }
}

#[tokio::test]
async fn is_home_returns_true_when_no_agent_ref_matches() {
    let (_inst, _server, registry) = server_fixture().await;
    let pk = registry.new_ephemeral_key("t").await.unwrap();
    let agents = vec![make_agent_ref("sha256:other", Some(&pk.to_string()))];
    assert!(is_home_for_agent_ref(&agents, "sha256:missing", &pk));
}

#[tokio::test]
async fn is_home_returns_true_when_home_pubkey_unset_legacy() {
    let (_inst, _server, registry) = server_fixture().await;
    let pk = registry.new_ephemeral_key("t").await.unwrap();
    let agents = vec![make_agent_ref("sha256:agent", None)];
    assert!(is_home_for_agent_ref(&agents, "sha256:agent", &pk));
}

#[tokio::test]
async fn is_home_returns_true_when_home_pubkey_matches_self() {
    let (_inst, _server, registry) = server_fixture().await;
    let pk = registry.new_ephemeral_key("t").await.unwrap();
    let agents = vec![make_agent_ref("sha256:agent", Some(&pk.to_string()))];
    assert!(is_home_for_agent_ref(&agents, "sha256:agent", &pk));
}

#[tokio::test]
async fn is_home_returns_false_when_home_pubkey_is_another_peer() {
    let (_inst, _server, registry) = server_fixture().await;
    let me = registry.new_ephemeral_key("me").await.unwrap();
    let other = registry.new_ephemeral_key("other").await.unwrap();
    let agents = vec![make_agent_ref("sha256:agent", Some(&other.to_string()))];
    assert!(!is_home_for_agent_ref(&agents, "sha256:agent", &me));
}

#[tokio::test]
async fn is_home_returns_true_on_corrupt_home_pubkey() {
    // Defensive: corrupt value yields legacy "any keyholder runs" rather
    // than silencing the agent permanently.
    let (_inst, _server, registry) = server_fixture().await;
    let pk = registry.new_ephemeral_key("t").await.unwrap();
    let agents = vec![make_agent_ref("sha256:agent", Some("not-a-pubkey"))];
    assert!(is_home_for_agent_ref(&agents, "sha256:agent", &pk));
}

#[tokio::test]
async fn peer_is_home_for_returns_true_when_agent_not_in_index() {
    let (_inst, server, _registry) = server_fixture().await;
    // No agent registered. Any session/agent name should pass (the
    // resolver wouldn't have picked us either way).
    assert!(server.peer_is_home_for("sha256:any", "ghost").await);
}

#[tokio::test]
async fn peer_is_home_for_returns_true_on_legacy_none_session() {
    let (_inst, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();
    // Insert an AgentRef with explicit None home (mimics a session that
    // predates this feature, or one created without using attach).
    crate::session::update_meta_on_db(&session_db, |m| {
        m.agents.push(crate::session::AgentRef {
            db_id: entry.db_id.to_string(),
            display_name: "alpha".to_string(),
            home_pubkey: None,
        });
    })
    .await
    .unwrap();
    assert!(server.peer_is_home_for(&sid, "alpha").await);
}

#[tokio::test]
async fn peer_is_home_for_returns_true_when_home_matches_self() {
    let (_inst, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();
    // attach_agent_to_session defaults home_pubkey to the attacher's key.
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    assert!(server.peer_is_home_for(&sid, "alpha").await);
}

#[tokio::test]
async fn peer_is_home_for_returns_false_when_home_is_another_peer() {
    let (_inst, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    let other = registry.new_ephemeral_key("other-peer").await.unwrap();
    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();
    crate::session::update_meta_on_db(&session_db, |m| {
        m.agents.push(crate::session::AgentRef {
            db_id: entry.db_id.to_string(),
            display_name: "alpha".to_string(),
            home_pubkey: Some(other.to_string()),
        });
    })
    .await
    .unwrap();
    assert!(!server.peer_is_home_for(&sid, "alpha").await);
}

// ---- Bridge-home migration ------------------------------------------

fn bridge_set(keys: &[&eidetica::auth::crypto::PublicKey]) -> std::collections::HashSet<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

#[tokio::test]
async fn a_session_hosted_on_a_bridge_is_migrated() {
    let (_inst, _server, registry) = server_fixture().await;
    let me = registry.new_ephemeral_key("me").await.unwrap();
    let bridge = registry.new_ephemeral_key("bridge").await.unwrap();
    let agents = vec![make_agent_ref("sha256:agent", Some(&bridge.to_string()))];
    assert_eq!(
        crate::server::bridge_home_to_migrate(
            &agents,
            "sha256:agent",
            &me,
            &bridge_set(&[&bridge])
        ),
        Some(bridge.to_string())
    );
}

#[tokio::test]
async fn a_session_hosted_on_another_agent_peer_is_left_alone() {
    // The case that must not regress: a legitimate remote host is not a
    // bridge, and seizing its sessions would break the single-owner
    // guarantee the home pubkey exists to provide.
    let (_inst, _server, registry) = server_fixture().await;
    let me = registry.new_ephemeral_key("me").await.unwrap();
    let bridge = registry.new_ephemeral_key("bridge").await.unwrap();
    let peer = registry.new_ephemeral_key("other-daemon").await.unwrap();
    let agents = vec![make_agent_ref("sha256:agent", Some(&peer.to_string()))];
    assert_eq!(
        crate::server::bridge_home_to_migrate(
            &agents,
            "sha256:agent",
            &me,
            &bridge_set(&[&bridge])
        ),
        None
    );
}

#[tokio::test]
async fn a_session_already_hosted_here_is_left_alone() {
    let (_inst, _server, registry) = server_fixture().await;
    let me = registry.new_ephemeral_key("me").await.unwrap();
    let agents = vec![make_agent_ref("sha256:agent", Some(&me.to_string()))];
    assert_eq!(
        crate::server::bridge_home_to_migrate(&agents, "sha256:agent", &me, &bridge_set(&[&me])),
        None
    );
}

#[tokio::test]
async fn a_legacy_or_corrupt_home_is_left_alone() {
    // Both fall through to the existing "any keyholder runs" behaviour
    // rather than being rewritten on a guess.
    let (_inst, _server, registry) = server_fixture().await;
    let me = registry.new_ephemeral_key("me").await.unwrap();
    let bridge = registry.new_ephemeral_key("bridge").await.unwrap();
    let set = bridge_set(&[&bridge]);
    let legacy = vec![make_agent_ref("sha256:agent", None)];
    assert_eq!(
        crate::server::bridge_home_to_migrate(&legacy, "sha256:agent", &me, &set),
        None
    );
    let corrupt = vec![make_agent_ref("sha256:agent", Some("not-a-pubkey"))];
    assert_eq!(
        crate::server::bridge_home_to_migrate(&corrupt, "sha256:agent", &me, &set),
        None
    );
}

#[tokio::test]
async fn no_published_bridge_keys_migrates_nothing() {
    // Logins predating the published-identity field leave the set empty.
    // That must cost a missed migration, never a wrong one.
    let (_inst, _server, registry) = server_fixture().await;
    let me = registry.new_ephemeral_key("me").await.unwrap();
    let bridge = registry.new_ephemeral_key("bridge").await.unwrap();
    let agents = vec![make_agent_ref("sha256:agent", Some(&bridge.to_string()))];
    assert_eq!(
        crate::server::bridge_home_to_migrate(
            &agents,
            "sha256:agent",
            &me,
            &std::collections::HashSet::new()
        ),
        None
    );
}

/// Helper mirroring what a bridge actually registers: a login carrying *both*
/// of its identities, which are different keys.
fn bridge_login(
    agent_pubkey: Option<&eidetica::auth::crypto::PublicKey>,
    peer_pubkey: Option<&eidetica::auth::crypto::PublicKey>,
) -> crate::agent_db::LoginRef {
    crate::agent_db::LoginRef {
        kind: "matrix".to_string(),
        identifier: "@chaz:example".to_string(),
        bridge_db_id: "sha256:bridgedb".to_string(),
        peer_pubkey: peer_pubkey.map(|k| k.to_string()),
        agent_pubkey: agent_pubkey.map(|k| k.to_string()),
        sync_addresses: Vec::new(),
    }
}

/// A bridge holds two unrelated keys, and only one of them can ever match a
/// session's home pubkey.
///
/// It authenticates to an agent DB with its bootstrapped *user* key — the one
/// `--print-pubkey` reports and an operator pre-authorizes — and that is the
/// key `attach` records as `home_pubkey`. Its *device* key is a separate
/// identity naming only where it is reachable on the transport. Publishing the
/// device key and comparing it against a home pubkey compares two disjoint key
/// spaces, so the migration cannot fire on any real deployment no matter how
/// many unit tests pass with a single key standing in for both.
#[tokio::test]
async fn the_published_bridge_key_is_the_one_sessions_are_homed_on() {
    let (_inst, _server, registry) = server_fixture().await;
    let me = registry.new_ephemeral_key("me").await.unwrap();
    // What the bridge bootstraps with, and what `attach` writes as home.
    let bridge_user_key = registry.new_ephemeral_key("bridge").await.unwrap();
    // The bridge's separate transport identity.
    let bridge_device_key = registry.new_ephemeral_key("bridge-device").await.unwrap();
    assert_ne!(bridge_user_key, bridge_device_key);

    let published = crate::server::build::bridge_pubkeys_from_logins(vec![bridge_login(
        Some(&bridge_user_key),
        Some(&bridge_device_key),
    )]);

    // The device key must not be what the daemon compares against...
    assert!(!published.contains(&bridge_device_key.to_string()));
    // ...and a session homed on the bridge's authorization key is recognised.
    let agents = vec![make_agent_ref(
        "sha256:agent",
        Some(&bridge_user_key.to_string()),
    )];
    assert_eq!(
        crate::server::bridge_home_to_migrate(&agents, "sha256:agent", &me, &published),
        Some(bridge_user_key.to_string())
    );
}

/// A login written before the bridge published its authorization key
/// contributes nothing, rather than falling back to the device key and
/// comparing across key spaces again.
#[tokio::test]
async fn a_login_without_a_published_agent_key_contributes_nothing() {
    let (_inst, _server, registry) = server_fixture().await;
    let bridge_device_key = registry.new_ephemeral_key("bridge-device").await.unwrap();
    let published = crate::server::build::bridge_pubkeys_from_logins(vec![bridge_login(
        None,
        Some(&bridge_device_key),
    )]);
    assert!(published.is_empty());
}

// ---- process_session gate -------------------------------------------

/// Register an Agent in the in-memory registry so resolve_agent_for_entry
/// can return it. Mirrors the shape used in `hydrate_picks_up_db_config_edits`.
fn register_alpha_agent_runtime(server: &Server) {
    server.agents().upsert(crate::agent::Agent {
        name: "alpha".to_string(),
        system_prompt: String::new(),
        system_prompt_files: vec![],
        default_model: Some("test-model".to_string()),
        allowed_tools: None,
        workers: HashMap::new(),
        max_iterations: 1,
        autonomous: false,
        presets: HashMap::new(),
        tool_profile: None,
        max_context_tokens: None,
        capabilities: crate::grants::Grants::default(),
        grants: HashMap::new(),
    });
}

async fn write_user_message(session_db: &eidetica::Database, sid: &str) {
    let mut session = crate::session::Session::new(
        crate::types::ConversationId(sid.to_string()),
        session_db.clone(),
    )
    .await;
    session
        .add_entry(crate::session::SessionEntry {
            sender: "user".to_string(),
            content: "hello".to_string(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .expect("write user message");
}

#[tokio::test]
async fn process_session_skips_when_not_home_peer() {
    let (_inst, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);

    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();

    // Pin home to a different peer so the gate fires.
    let other = registry.new_ephemeral_key("other-peer").await.unwrap();
    crate::session::update_meta_on_db(&session_db, |m| {
        m.agents[0].home_pubkey = Some(other.to_string());
    })
    .await
    .unwrap();

    let backend = crate::backends::BackendManager::new(
        &None,
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&session_db, backend, Some("alpha".to_string()), None)
        .await
        .unwrap();

    write_user_message(&session_db, &sid).await;

    let entries_before = {
        let session = crate::session::Session::new(
            crate::types::ConversationId(sid.clone()),
            session_db.clone(),
        )
        .await;
        session.entries().len()
    };

    server.process_session(&sid, None).await.unwrap();

    // Gate released the lock inline before returning.
    assert!(!server.processing.lock().await.contains(&sid));

    let entries_after = {
        let session = crate::session::Session::new(
            crate::types::ConversationId(sid.clone()),
            session_db.clone(),
        )
        .await;
        session.entries().len()
    };
    assert_eq!(
        entries_before, entries_after,
        "non-home peer must not write any new entries"
    );
}

#[tokio::test]
async fn process_session_runs_when_home_pubkey_unset_legacy() {
    let (_inst, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);

    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();

    // Simulate a legacy session: clear the home_pubkey we just set on attach.
    crate::session::update_meta_on_db(&session_db, |m| {
        m.agents[0].home_pubkey = None;
    })
    .await
    .unwrap();

    let backend = crate::backends::BackendManager::new(
        &None,
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&session_db, backend, Some("alpha".to_string()), None)
        .await
        .unwrap();

    write_user_message(&session_db, &sid).await;

    server.process_session(&sid, None).await.unwrap();

    // Gate passed → no home-skip was recorded. Asserted on the skip counter
    // rather than the `processing` set: the latter is cleared by the spawned
    // task, so observing it holds only while that task is unpolled.
    assert_eq!(server.home_skip_count(&sid, "alpha").await, 0);
}

#[tokio::test]
async fn process_session_runs_when_home_matches_self() {
    let (_inst, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);

    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();
    // attach defaults home_pubkey to this peer's key on alpha.
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();

    let backend = crate::backends::BackendManager::new(
        &None,
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&session_db, backend, Some("alpha".to_string()), None)
        .await
        .unwrap();

    write_user_message(&session_db, &sid).await;

    server.process_session(&sid, None).await.unwrap();

    // Gate passed → no home-skip recorded. See the note in the legacy test
    // above on why this is not asserted via the `processing` set.
    assert_eq!(server.home_skip_count(&sid, "alpha").await, 0);
}

// ---- fire_agent_schedule gate ---------------------------------------

#[tokio::test]
async fn fire_fresh_skips_when_agent_home_is_another_peer() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;

    // Overwrite the agent-level home_pubkey to a foreign key.
    let other = registry.new_ephemeral_key("other-peer").await.unwrap();
    crate::db_kind::write_agent_home_pubkey(adb.database(), &other)
        .await
        .unwrap();

    let sessions_before = registry.list_sessions().await.unwrap_or_default().len();
    let payload = fresh_schedule_payload(&entry.db_id.to_string(), "f1", "do the thing");
    let result = server.fire_agent_schedule(payload).await;
    assert!(result.is_ok(), "skip path returns Ok: {result:?}");

    // No new Fresh session should have been created.
    let sessions_after = registry.list_sessions().await.unwrap_or_default().len();
    assert_eq!(sessions_before, sessions_after);

    // No ScheduleFire recorded (the gate fires before any of the
    // schedule-fire bookkeeping runs).
    let fires = adb.list_schedule_fires().await.unwrap();
    assert!(fires.is_empty(), "non-home gate should not record a fire");
}

#[tokio::test]
async fn fire_fresh_runs_when_agent_home_is_unset_legacy() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;

    // Mimic a pre-feature agent DB by clearing the home_pubkey written
    // at create time.
    crate::db_kind::clear_agent_home_pubkey(adb.database())
        .await
        .unwrap();

    adb.upsert_schedule(Schedule::new(
        "f1",
        Trigger::OneShot {
            fire_at: Utc::now(),
        },
        "wake",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();

    let payload = fresh_schedule_payload(&entry.db_id.to_string(), "f1", "wake");
    let _ = server.fire_agent_schedule(payload).await;
    // Even with the LLM call failing (no backends), a ScheduleFire is
    // recorded — which only happens when we get past the gate.
    let fires = adb.list_schedule_fires().await.unwrap();
    assert_eq!(fires.len(), 1, "legacy None must let the fire through");
}

async fn schedule_engine_for(
    server: &Arc<Server>,
    registry: &Arc<crate::session::SessionRegistry>,
    owner_id: &str,
    adb: &crate::agent_db::AgentDb,
) -> Arc<crate::routine::RoutineEngine> {
    let engine = crate::routine::RoutineEngine::new(registry.chaz_peer().clone(), None)
        .await
        .unwrap();
    engine.register_agent(owner_id, adb).await.unwrap();
    server.set_routine_engine(engine.clone());
    engine
}

#[tokio::test]
async fn fire_schedule_skips_dispatched_payload_after_removal() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;
    let owner_id = entry.db_id.to_string();
    adb.upsert_schedule(Schedule::new(
        "wake",
        Trigger::Cron {
            expr: "0 0 9 * * *".into(),
        },
        "old prompt",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();
    let engine = schedule_engine_for(&server, &registry, &owner_id, &adb).await;
    let routine_id = crate::routine::RoutineId::new(format!("agent:{owner_id}:wake"));
    let mut payload = fresh_schedule_payload(&owner_id, "wake", "old prompt");
    payload.generation = engine.current_generation(&routine_id).await;

    adb.remove_schedule("wake").await.unwrap();
    engine.deregister_agent(&owner_id).await;

    server.fire_agent_schedule(payload).await.unwrap();
    assert!(adb.list_schedule_fires().await.unwrap().is_empty());
    assert!(
        registry
            .list_sessions()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn fire_schedule_skips_dispatched_payload_after_same_id_replacement() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;
    let owner_id = entry.db_id.to_string();
    adb.upsert_schedule(Schedule::new(
        "wake",
        Trigger::Cron {
            expr: "0 0 9 * * *".into(),
        },
        "old prompt",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();
    let engine = schedule_engine_for(&server, &registry, &owner_id, &adb).await;
    let routine_id = crate::routine::RoutineId::new(format!("agent:{owner_id}:wake"));
    let mut payload = fresh_schedule_payload(&owner_id, "wake", "old prompt");
    payload.generation = engine.current_generation(&routine_id).await;

    adb.upsert_schedule(Schedule::new(
        "wake",
        Trigger::Cron {
            expr: "0 0 10 * * *".into(),
        },
        "replacement prompt",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();
    engine.reload_agent(&owner_id, &adb).await.unwrap();

    server.fire_agent_schedule(payload).await.unwrap();
    assert!(adb.list_schedule_fires().await.unwrap().is_empty());
    assert!(
        registry
            .list_sessions()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn fire_schedule_skips_dispatched_payload_after_metadata_edit() {
    // Gap (a): a prompt-only edit keeps the same trigger/slot, so the
    // detached old payload must no longer validate — the generation now
    // names the executable content, not just the slot.
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;
    let owner_id = entry.db_id.to_string();
    adb.upsert_schedule(Schedule::new(
        "wake",
        Trigger::Cron {
            expr: "0 0 9 * * *".into(),
        },
        "old prompt",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();
    let engine = schedule_engine_for(&server, &registry, &owner_id, &adb).await;
    let routine_id = crate::routine::RoutineId::new(format!("agent:{owner_id}:wake"));
    let mut payload = fresh_schedule_payload(&owner_id, "wake", "old prompt");
    payload.generation = engine.current_generation(&routine_id).await;

    // Metadata-only edit: same trigger, new prompt.
    adb.upsert_schedule(Schedule::new(
        "wake",
        Trigger::Cron {
            expr: "0 0 9 * * *".into(),
        },
        "replacement prompt",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();
    engine.reload_agent(&owner_id, &adb).await.unwrap();

    server.fire_agent_schedule(payload).await.unwrap();
    assert!(adb.list_schedule_fires().await.unwrap().is_empty());
    assert!(
        registry
            .list_sessions()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn fire_schedule_runs_dispatched_payload_for_live_entry() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;
    let owner_id = entry.db_id.to_string();
    adb.upsert_schedule(Schedule::new(
        "wake",
        Trigger::Cron {
            expr: "0 0 9 * * *".into(),
        },
        "wake",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();
    let engine = schedule_engine_for(&server, &registry, &owner_id, &adb).await;
    let routine_id = crate::routine::RoutineId::new(format!("agent:{owner_id}:wake"));
    let mut payload = fresh_schedule_payload(&owner_id, "wake", "wake");
    payload.generation = engine.current_generation(&routine_id).await;

    let result = server.fire_agent_schedule(payload).await;
    assert!(result.is_err(), "empty test backend fails after the gate");
    assert_eq!(adb.list_schedule_fires().await.unwrap().len(), 1);
    assert_eq!(registry.list_sessions().await.unwrap_or_default().len(), 1);
}

#[tokio::test]
async fn fire_schedule_stale_at_turn_start_tears_down_fresh_session() {
    // A remove/replace landing between the pre-check and turn start must
    // not orphan the just-created Fresh session. Hold the processing lock
    // so the fire parks after target resolution (session created), retire
    // the engine entry mid-flight, then release: the late re-check fails
    // and the Fresh session must be torn down, not left Active.
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;
    let owner_id = entry.db_id.to_string();
    adb.upsert_schedule(Schedule::new(
        "wake",
        Trigger::Cron {
            expr: "0 0 9 * * *".into(),
        },
        "wake",
        crate::agent_db::ScheduleTarget::Fresh,
    ))
    .await
    .unwrap();
    let engine = schedule_engine_for(&server, &registry, &owner_id, &adb).await;
    let routine_id = crate::routine::RoutineId::new(format!("agent:{owner_id}:wake"));
    let mut payload = fresh_schedule_payload(&owner_id, "wake", "wake");
    payload.generation = engine.current_generation(&routine_id).await;
    assert!(
        payload.generation.is_some(),
        "engine must seed a generation for the schedule"
    );

    // Park the fire after target resolution (Fresh session already
    // created) by holding the processing mutex it must acquire next.
    let guard = server.processing.lock().await;
    let server_clone = server.clone();
    let fire = tokio::spawn(async move { server_clone.fire_agent_schedule(payload).await });

    // Wait for the Fresh session to appear, then pull the engine entry
    // out from under the parked fire (simulates a remove/replace landing
    // mid-flight).
    let sid = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let sessions = registry.list_sessions().await.unwrap_or_default();
            if let Some(s) = sessions.first() {
                return s.session_db_id.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fresh session should appear while the fire is parked");
    engine.deregister_agent(&owner_id).await;
    drop(guard);

    let result = fire.await.expect("fire task join");
    assert!(
        result.is_ok(),
        "stale turn-start skip returns Ok: {result:?}"
    );

    // No orphaned session: the row survives (append-only history) but is
    // Closed, the server holds no runtime claim, and nothing was recorded.
    let sessions = registry.list_sessions().await.unwrap_or_default();
    assert_eq!(sessions.len(), 1, "exactly the torn-down session");
    assert!(
        sessions
            .iter()
            .all(|s| s.status == crate::session::SessionStatus::Closed),
        "stale Fresh session must be Closed, not Active"
    );
    assert_eq!(sessions[0].session_db_id, sid);
    assert!(
        !server.is_session_open(&sid).await,
        "server runtime claim must be released"
    );
    assert!(
        !server.processing.lock().await.contains(&sid),
        "processing lock must be released"
    );
    assert!(
        adb.list_schedule_fires().await.unwrap().is_empty(),
        "stale fire must not record a ScheduleFire"
    );
}

#[tokio::test]
async fn fire_pinned_skips_when_session_home_is_another_peer() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, adb) = seed_agent(&server, &registry, "alpha").await;

    let (_conv, session_db) = registry.create_session(Some("chat")).await.unwrap();
    let sid = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();

    // Rewrite the AgentRef's home_pubkey to another peer.
    let other = registry.new_ephemeral_key("other-peer").await.unwrap();
    crate::session::update_meta_on_db(&session_db, |m| {
        m.agents[0].home_pubkey = Some(other.to_string());
    })
    .await
    .unwrap();

    let payload = pinned_schedule_payload(&entry.db_id.to_string(), "p1", "wake", &sid);
    let result = server.fire_agent_schedule(payload).await;
    assert!(result.is_ok(), "skip path returns Ok: {result:?}");

    // No ScheduleFire — gate fires before bookkeeping.
    let fires = adb.list_schedule_fires().await.unwrap();
    assert!(
        fires.is_empty(),
        "non-home pinned fire should not record a fire"
    );
}

#[tokio::test]
async fn watcher_registers_exposed_sessions_only() {
    // The daemon's agent-registry watcher must register sessions a bridge
    // exposed (so the agent answers them) and leave attached-but-unexposed
    // sessions alone. Single-peer stand-in for the cross-peer flow.
    let (_instance, server, registry) = server_fixture().await;
    let agent = crate::session::test_helpers::make_agent_entry(&registry, "alpha").await;
    server.agent_index().register(agent.clone());

    // Exposed on a bridge → must be registered.
    let (_c, sdb) = registry.create_session(Some("exposed")).await.unwrap();
    let exposed = sdb.root_id().to_string();
    registry
        .attach_agent_to_session(&exposed, &agent)
        .await
        .unwrap();
    let adb = registry
        .open_agent_db(&agent.db_id, Some(&agent.pubkey))
        .await
        .unwrap()
        .unwrap();
    adb.expose_session_on(&exposed, "matrix").await.unwrap();

    // Attached but never exposed → must be skipped.
    let (_c2, sdb2) = registry.create_session(Some("private")).await.unwrap();
    let private = sdb2.root_id().to_string();
    registry
        .attach_agent_to_session(&private, &agent)
        .await
        .unwrap();

    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    let backend = crate::backends::BackendManager::new(&None, secrets);
    super::build::register_exposed_sessions(&server, &registry, &backend).await;

    assert!(
        server.is_watching_session(&exposed).await,
        "exposed session must be registered for the agent to answer"
    );
    assert!(
        !server.is_watching_session(&private).await,
        "attached-but-unexposed session must be skipped"
    );
}

#[tokio::test]
async fn registry_watcher_rescans_while_initial_recovery_is_blocked() {
    // A restored daemon can have many old exposed sessions. Their bootstrap
    // must not prevent the watcher from reacting to a new bridge exposure.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let task = tokio::spawn({
        let started = started.clone();
        let release = release.clone();
        let runs = runs.clone();
        async move {
            super::build::run_exposed_session_rescans(rx, move || {
                let started = started.clone();
                let release = release.clone();
                let runs = runs.clone();
                async move {
                    let run = runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if run == 0 {
                        started.notify_one();
                        release.notified().await;
                    }
                }
            })
            .await;
        }
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .expect("initial recovery should start");
    tx.send(()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while runs.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("new exposure should rescan before initial recovery completes");

    release.notify_one();
    drop(tx);
    task.await.unwrap();
}

#[tokio::test]
async fn registry_watcher_bounds_and_coalesces_rescans() {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let started = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let task = tokio::spawn({
        let release = release.clone();
        let started = started.clone();
        let active = active.clone();
        let peak = peak.clone();
        async move {
            super::build::run_exposed_session_rescans(rx, move || {
                let release = release.clone();
                let started = started.clone();
                let active = active.clone();
                let peak = peak.clone();
                async move {
                    started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                    let permit = release.acquire().await.unwrap();
                    permit.forget();
                    active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            })
            .await;
        }
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while started.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tx.send(()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while started.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for _ in 0..8 {
        tx.send(()).await.unwrap();
    }
    tokio::task::yield_now().await;
    assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 2);

    release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while started.load(std::sync::atomic::Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a signal coalesced while both scans ran should start one follow-up");
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 2);

    release.add_permits(2);
    drop(tx);
    task.await.unwrap();
}

// ---------------------------------------------------------------------------
// Track A: transport/runtime split (`watch_session` + `claim_runtime`).
// ---------------------------------------------------------------------------

/// Make a fresh, registry-backed session and a no-op backend for the
/// runtime-ownership tests.
async fn fresh_session_and_backend(
    registry: &Arc<crate::session::SessionRegistry>,
) -> (eidetica::Database, crate::backends::BackendManager) {
    let (_conv, session_db) = registry.create_session(Some("chat")).await.unwrap();
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    let backend = crate::backends::BackendManager::new(&None, secrets);
    (session_db, backend)
}

#[tokio::test]
async fn watch_session_is_transport_only_no_runtime_claim() {
    let (_instance, server, registry) = server_fixture().await;
    let (session_db, backend) = fresh_session_and_backend(&registry).await;
    let session_db_id = session_db.root_id().to_string();

    // Transport only: subscribes to I/O, but takes no runtime ownership.
    server
        .watch_session(&session_db, backend, Some("agent".to_string()), None)
        .await
        .unwrap();

    assert!(
        server.is_watching_session(&session_db_id).await,
        "watch_session wires transport"
    );
    assert!(
        server.runtime_owner_of(&session_db_id).is_none(),
        "watch_session must NOT claim runtime — no agent, no status"
    );
}

#[tokio::test]
async fn claim_runtime_fires_exactly_once_second_caller_errors() {
    let (_instance, server, registry) = server_fixture().await;
    let (session_db, backend) = fresh_session_and_backend(&registry).await;
    let session_db_id = session_db.root_id().to_string();

    server
        .watch_session(&session_db, backend, Some("agent".to_string()), None)
        .await
        .unwrap();

    // First claimant wins.
    let claimed = server
        .claim_runtime(
            &session_db,
            "agent".to_string(),
            0,
            crate::config::RuntimeMode::Always,
        )
        .await
        .unwrap();
    assert!(claimed, "first claim_runtime must take ownership");
    assert!(
        server.runtime_owner_of(&session_db_id).is_some(),
        "ownership recorded after the first claim"
    );

    // A second distinct caller is refused under `Always`.
    let second = server
        .claim_runtime(
            &session_db,
            "agent".to_string(),
            0,
            crate::config::RuntimeMode::Always,
        )
        .await;
    assert!(
        second.is_err(),
        "second claim_runtime under Always must error — ownership fires exactly once"
    );
}

#[tokio::test]
async fn claim_runtime_auto_skips_when_already_claimed() {
    let (_instance, server, registry) = server_fixture().await;
    let (session_db, backend) = fresh_session_and_backend(&registry).await;

    server
        .watch_session(&session_db, backend, Some("agent".to_string()), None)
        .await
        .unwrap();

    assert!(
        server
            .claim_runtime(
                &session_db,
                "agent".to_string(),
                0,
                crate::config::RuntimeMode::Auto
            )
            .await
            .unwrap(),
        "first Auto claim takes ownership"
    );

    // Auto gracefully skips (no error) when the session is already owned.
    let again = server
        .claim_runtime(
            &session_db,
            "agent".to_string(),
            0,
            crate::config::RuntimeMode::Auto,
        )
        .await
        .unwrap();
    assert!(!again, "Auto must skip (Ok(false)) when already claimed");
}

#[tokio::test]
async fn claim_runtime_never_does_not_claim() {
    let (_instance, server, registry) = server_fixture().await;
    let (session_db, backend) = fresh_session_and_backend(&registry).await;
    let session_db_id = session_db.root_id().to_string();

    server
        .watch_session(&session_db, backend, Some("agent".to_string()), None)
        .await
        .unwrap();

    let claimed = server
        .claim_runtime(
            &session_db,
            "agent".to_string(),
            0,
            crate::config::RuntimeMode::Never,
        )
        .await
        .unwrap();
    assert!(!claimed, "Never must never claim");
    assert!(
        server.runtime_owner_of(&session_db_id).is_none(),
        "Never leaves the session unowned — pure transport"
    );
}

#[tokio::test]
async fn deregister_session_releases_runtime_claim() {
    let (_instance, server, registry) = server_fixture().await;
    let (session_db, backend) = fresh_session_and_backend(&registry).await;
    let session_db_id = session_db.root_id().to_string();

    server
        .register_session(&session_db, backend, Some("agent".to_string()), None)
        .await
        .unwrap();
    assert!(
        server.runtime_owner_of(&session_db_id).is_some(),
        "register_session (Auto default) claims runtime"
    );

    server.deregister_session(&session_db_id).await;
    assert!(
        server.runtime_owner_of(&session_db_id).is_none(),
        "deregister_session releases the runtime claim so it can be re-claimed"
    );
}

// ---- Write source: local vs remote -----------------------------------
//
// Every `on_write` subscription in chaz is deliberately source-agnostic: a
// session write wakes the agent whether it was committed here or arrived from
// a co-owner over sync. These tests pin both halves of that, because nothing
// else in the suite distinguishes them — a callback registration that quietly
// stopped seeing remote fires would look identical to a healthy one.

/// Both write sources reach a per-database callback, and `WriteEvent::source`
/// tells them apart. This is the eidetica contract the whole sharing story
/// rests on; it is pinned here so a dependency bump that regressed it fails
/// in chaz rather than silently going one-directional in production.
#[tokio::test]
async fn write_events_carry_the_source_that_produced_them() {
    use crate::test_support::replay_tips_as_remote;
    use eidetica::instance::WriteSource;

    let (_instance, _server, registry) = server_fixture().await;
    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();

    let seen: Arc<std::sync::Mutex<Vec<WriteSource>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    session_db
        .on_write(move |event, _db| {
            sink.lock().unwrap().push(event.source());
            Box::pin(async { Ok(()) })
        })
        .await
        .unwrap()
        .detach();

    write_user_message(&session_db, &sid).await;
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[WriteSource::Local],
        "a locally committed entry fires exactly one Local event"
    );

    replay_tips_as_remote(registry.instance(), &session_db).await;
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[WriteSource::Local, WriteSource::Remote],
        "sync-ingested entries fire on the same callback, tagged Remote"
    );
}

/// A remote-sourced write on a watched session runs the agent — the host half
/// of a co-owned session.
///
/// The user message is committed *before* `register_session`, so the local
/// commit fires into a session nobody is watching yet. The only event the
/// server ever sees is the remote-sourced replay, which makes the agent's
/// reply attributable to it and nothing else.
#[tokio::test]
async fn a_remote_write_wakes_the_agent_on_the_host() {
    use crate::test_support::{MockBackend, replay_tips_as_remote};

    let (_instance, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);

    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();

    write_user_message(&session_db, &sid).await;

    let mock = Arc::new(MockBackend::new());
    mock.push_text("ack from the host");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&session_db, backend, Some("alpha".to_string()), None)
        .await
        .unwrap();

    replay_tips_as_remote(registry.instance(), &session_db).await;

    assert!(
        await_agent_reply(&session_db, &sid, "alpha").await,
        "a remote-sourced write must drive the agent loop on the host"
    );
}

/// The local counterpart, so a regression that gated the wake path on
/// `WriteSource::Remote` is caught as readily as one that gated it on `Local`.
#[tokio::test]
async fn a_local_write_wakes_the_agent() {
    use crate::test_support::MockBackend;

    let (_instance, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);

    let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
    let sid = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();

    let mock = Arc::new(MockBackend::new());
    mock.push_text("ack from the host");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&session_db, backend, Some("alpha".to_string()), None)
        .await
        .unwrap();

    write_user_message(&session_db, &sid).await;

    assert!(
        await_agent_reply(&session_db, &sid, "alpha").await,
        "a locally committed write must drive the agent loop"
    );
}

async fn write_user_message_with_content(
    session_db: &eidetica::Database,
    sid: &str,
    content: &str,
) -> crate::session::TurnRequestId {
    let entry = crate::session::SessionEntry {
        sender: "user".to_string(),
        content: content.to_string(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    };
    let mut session = crate::session::Session::new(
        crate::types::ConversationId(sid.to_string()),
        session_db.clone(),
    )
    .await;
    session.add_entry(entry).await.unwrap()
}

async fn await_call_count(mock: &crate::test_support::MockBackend, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if mock.recorded_calls().len() >= expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("backend call count did not advance");
}

async fn await_no_processing(server: &Server, sid: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if !server.processing.lock().await.contains(sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn did not finish");
}

#[tokio::test]
async fn request_before_observer_attach_is_reconciled_once() {
    use crate::test_support::MockBackend;

    let (_instance, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid, "before attach").await;

    let mock = Arc::new(MockBackend::new());
    mock.push_text("only reply");
    mock.push_text("duplicate reply");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    await_call_count(&mock, 1).await;
    await_no_processing(&server, &sid).await;
    for _ in 0..3 {
        server
            .notify_tx
            .send(ProcessingCommand::Wake(sid.clone()))
            .await
            .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(mock.recorded_calls().len(), 1);
    let session = Session::new(ConversationId(sid), db).await;
    assert!(
        session
            .attempts_for_test()
            .await
            .iter()
            .all(|attempt| { attempt.status == crate::session::TurnAttemptStatus::Completed })
    );
}

#[tokio::test]
async fn legacy_transcript_is_baselined_before_executor_registration() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("legacy")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();

    // Remove the new-session marker to model an older database, then write
    // completed history that includes a sender no longer in the registry.
    let txn = db.new_transaction().await.unwrap();
    let meta = txn
        .get_store::<eidetica::store::DocStore>("meta")
        .await
        .unwrap();
    meta.delete("turn_request_schema").await.unwrap();
    meta.delete("turn_request_baseline").await.unwrap();
    let entries = txn
        .get_store::<eidetica::store::Table<SessionEntry>>("entries")
        .await
        .unwrap();
    for (sender, content) in [
        ("user", "old one"),
        ("retired-agent", "old reply"),
        ("user", "old two"),
        ("alpha", "old reply two"),
    ] {
        entries
            .insert(SessionEntry {
                sender: sender.into(),
                content: content.into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
    }
    txn.commit().await.unwrap();

    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("new reply");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(mock.recorded_calls().is_empty());

    write_user_message_with_content(&db, &sid, "new work").await;
    await_call_count(&mock, 1).await;
    await_no_processing(&server, &sid).await;
    assert_eq!(mock.recorded_calls().len(), 1);
}

#[tokio::test]
async fn rejected_retry_does_not_mutate_attempts() {
    let (_instance, server, registry) = server_fixture().await;
    let (_conv, db) = registry.create_session(Some("retry")).await.unwrap();
    let sid = db.root_id().to_string();
    let request_id = write_user_message_with_content(&db, &sid, "retry me").await;
    let session = Session::new(ConversationId(sid.clone()), db).await;
    session
        .start_turn_attempt(request_id.clone())
        .await
        .unwrap();
    let before = session.attempts_for_test().await.len();

    let error = server
        .retry_interrupted_turn(&sid, &request_id)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("did not accept retry"));
    assert_eq!(session.attempts_for_test().await.len(), before);
}

#[tokio::test]
async fn concurrent_retries_accept_exactly_one_attempt() {
    let (_instance, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("retry-race")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    let request_id = write_user_message_with_content(&db, &sid, "retry once").await;
    let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
    session
        .start_turn_attempt(request_id.clone())
        .await
        .unwrap();

    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("one retry");
    let gate = mock.block_next_call();
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();

    let first = {
        let server = server.clone();
        let sid = sid.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { server.retry_interrupted_turn(&sid, &request_id).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), gate.wait_started())
        .await
        .unwrap();
    let second = server.retry_interrupted_turn(&sid, &request_id).await;
    gate.release();
    assert!(first.await.unwrap().is_ok());
    assert!(second.is_err());
    await_no_processing(&server, &sid).await;
    assert_eq!(mock.recorded_calls().len(), 1);
    assert_eq!(session.attempts_for_test().await.len(), 2);
}

#[tokio::test]
async fn client_server_cannot_register_execution_or_retry() {
    let (_instance, executor, registry) = server_fixture().await;
    let (_conv, db) = registry.create_session(Some("client")).await.unwrap();
    let sid = db.root_id().to_string();
    let request_id = write_user_message_with_content(&db, &sid, "client request").await;
    let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
    let attempt = session
        .start_turn_attempt(request_id.clone())
        .await
        .unwrap();
    drop(executor);

    let client = client_server_fixture_from_registry(registry.clone()).await;
    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("must not run");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    assert!(
        client
            .register_session(&db, backend, Some("agent".into()), None)
            .await
            .unwrap_err()
            .to_string()
            .contains("no executor loop")
    );
    assert_eq!(session.attempts_for_test().await, vec![attempt]);
    assert!(mock.recorded_calls().is_empty());
    assert!(client.routine_engine().is_none());

    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    client
        .watch_session(&db, backend, Some("agent".into()), None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(client.interrupted_turns(&sid).await.is_ok());
    assert!(mock.recorded_calls().is_empty());
}

#[tokio::test]
async fn request_before_attempt_start_survives_restart_as_queued() {
    let (_instance, old_server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&old_server, &registry, "alpha").await;
    register_alpha_agent_runtime(&old_server);
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid, "queued across restart").await;
    drop(old_server);

    let (_instance2, restarted, _registry2) = server_fixture_from_registry(registry.clone()).await;
    register_alpha_agent_runtime(&restarted);
    restarted.agent_index().register(entry);
    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("recovered");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    restarted
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    await_call_count(&mock, 1).await;
    await_no_processing(&restarted, &sid).await;
    assert!(restarted.interrupted_turns(&sid).await.unwrap().is_empty());
}

#[tokio::test]
async fn second_turn_during_first_response_is_preserved() {
    use crate::test_support::MockBackend;

    let (_instance, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    let mock = Arc::new(MockBackend::new());
    mock.push_text("first reply");
    mock.push_text("second reply");
    mock.push_text("duplicate reply");
    let gate = mock.block_next_call();
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();

    write_user_message_with_content(&db, &sid, "first").await;
    tokio::time::timeout(std::time::Duration::from_secs(1), gate.wait_started())
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid, "second").await;
    gate.release();
    await_call_count(&mock, 2).await;
    await_no_processing(&server, &sid).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(mock.recorded_calls().len(), 2);
    assert!(
        mock.recorded_calls()[1]
            .messages
            .iter()
            .any(|message| matches!(
                message,
                crate::runtime::RuntimeMessage::User(content) if content == "second"
            ))
    );
}

#[tokio::test]
async fn transcript_commits_full_tool_exchange_before_progression() {
    use crate::test_support::MockBackend;
    use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError};
    use serde_json::{Value, json};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct LongTool(Arc<AtomicUsize>);
    impl Tool for LongTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "long".into(),
                description: "returns a long result".into(),
                parameters: json!({"type":"object"}),
            }
        }
        fn execute<'a>(
            &'a self,
            _: Value,
            _: &'a ToolContext,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("x".repeat(700)) })
        }
    }

    let (_instance, server, registry) = server_fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    server.tools.register(LongTool(calls.clone()));
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("transcript")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    let request_id = write_user_message_with_content(&db, &sid, "run it").await;
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![(
        "call-stable".into(),
        "long".into(),
        "{\"k\":1}".into(),
    )]);
    mock.push_text("done");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    await_call_count(&mock, 2).await;
    await_no_processing(&server, &sid).await;

    let reopened = Session::new(ConversationId(sid), db).await;
    let attempt = reopened
        .attempts_for_test()
        .await
        .into_iter()
        .next()
        .unwrap();
    let transcript = reopened.turn_transcript(&attempt.attempt_id).await.unwrap();
    assert_eq!(transcript.len(), 3);
    assert!(
        transcript
            .iter()
            .all(|record| record.request_id == request_id)
    );
    assert!(matches!(&transcript[0].message,
        crate::session::TurnTranscriptMessage::ModelResponse { tool_calls, .. }
        if tool_calls[0].id == "call-stable" && tool_calls[0].arguments == "{\"k\":1}"
    ));
    assert!(matches!(&transcript[1].message,
        crate::session::TurnTranscriptMessage::ToolResult { call_id, output, .. }
        if call_id == "call-stable" && output.len() == 700
    ));
    assert!(matches!(
        &transcript[2].message,
        crate::session::TurnTranscriptMessage::ModelResponse { terminal: true, .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        reopened
            .entries()
            .iter()
            .filter(|e| e.content == "done")
            .count(),
        1
    );
}

#[tokio::test]
async fn transcript_failure_before_tool_prevents_side_effect_and_completion() {
    use crate::test_support::MockBackend;
    use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError};
    use serde_json::{Value, json};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct EffectTool(Arc<AtomicUsize>);
    impl Tool for EffectTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "effect".into(),
                description: "records a call".into(),
                parameters: json!({"type":"object"}),
            }
        }
        fn execute<'a>(
            &'a self,
            _: Value,
            _: &'a ToolContext,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("effect".into()) })
        }
    }

    let (_instance, server, registry) = server_fixture().await;
    let effects = Arc::new(AtomicUsize::new(0));
    server.tools.register(EffectTool(effects.clone()));
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("failure")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    let request_id = write_user_message_with_content(&db, &sid, "do not run").await;
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![("call".into(), "effect".into(), "{}".into())]);
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server.fail_transcript_after(0);
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    await_call_count(&mock, 1).await;
    await_no_processing(&server, &sid).await;

    let reopened = Session::new(ConversationId(sid), db).await;
    let attempt = reopened
        .attempts_for_test()
        .await
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(attempt.status, crate::session::TurnAttemptStatus::Started);
    assert!(
        reopened
            .turn_transcript(&attempt.attempt_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(
        reopened
            .entries()
            .iter()
            .all(|entry| entry.entry_type != EntryType::Error)
    );
    assert_eq!(
        reopened
            .turn_request(&request_id, |name| name == "alpha", &Default::default())
            .await
            .unwrap()
            .unwrap()
            .state,
        TurnRequestState::Interrupted {
            attempt_id: attempt.attempt_id
        }
    );
}

#[tokio::test]
async fn transcript_failure_after_tool_leaves_ambiguous_effect_interrupted() {
    use crate::test_support::MockBackend;
    use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError};
    use serde_json::{Value, json};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct EffectTool(Arc<AtomicUsize>);
    impl Tool for EffectTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "effect".into(),
                description: "records a call".into(),
                parameters: json!({"type":"object"}),
            }
        }
        fn execute<'a>(
            &'a self,
            _: Value,
            _: &'a ToolContext,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("effect happened".into()) })
        }
    }

    let (_instance, server, registry) = server_fixture().await;
    let effects = Arc::new(AtomicUsize::new(0));
    server.tools.register(EffectTool(effects.clone()));
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("ambiguous")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid, "run once").await;
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![("call".into(), "effect".into(), "{}".into())]);
    mock.push_text("must not be called");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server.fail_transcript_after(1);
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    await_call_count(&mock, 1).await;
    await_no_processing(&server, &sid).await;

    let reopened = Session::new(ConversationId(sid), db).await;
    let attempt = reopened
        .attempts_for_test()
        .await
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(attempt.status, crate::session::TurnAttemptStatus::Started);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(mock.recorded_calls().len(), 1);
    assert_eq!(
        reopened
            .turn_transcript(&attempt.attempt_id)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn transcript_persists_denied_multi_tool_and_forced_summary_branches() {
    use crate::bridge::ApprovalDecision;
    use crate::test_support::MockBackend;
    use crate::tool::{
        ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy,
    };
    use serde_json::{Value, json};
    use std::pin::Pin;

    enum BranchTool {
        Gated,
        Limited,
        Failing,
    }
    impl Tool for BranchTool {
        fn descriptor(&self) -> ToolDescriptor {
            let name = match self {
                Self::Gated => "gated",
                Self::Limited => "limited",
                Self::Failing => "failing",
            };
            ToolDescriptor {
                name: name.into(),
                description: name.into(),
                parameters: json!({"type":"object"}),
            }
        }
        fn execute<'a>(
            &'a self,
            _: Value,
            _: &'a ToolContext,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
        {
            Box::pin(async move {
                match self {
                    Self::Failing => Err(ToolError::Execution("failed".into())),
                    _ => Ok("must not run".into()),
                }
            })
        }
        fn default_policy(&self) -> ToolPolicy {
            match self {
                Self::Gated => ToolPolicy {
                    risk: RiskLevel::High,
                    approval: ApprovalRequirement::Always,
                    ..Default::default()
                },
                Self::Limited => ToolPolicy {
                    rate_limit: Some(0),
                    ..Default::default()
                },
                Self::Failing => ToolPolicy::default(),
            }
        }
    }

    let (_instance, server, registry) = server_fixture().await;
    server.tools.register(BranchTool::Gated);
    server.tools.register(BranchTool::Limited);
    server.tools.register(BranchTool::Failing);
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("branches")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid, "branches").await;
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![
        ("denied".into(), "gated".into(), "{}".into()),
        ("limited".into(), "limited".into(), "{}".into()),
        ("failed".into(), "failing".into(), "{}".into()),
        ("missing".into(), "missing".into(), "{\"full\":true}".into()),
    ]);
    mock.push_text("summarized");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    let (approval_tx, mut approval_rx) =
        tokio::sync::mpsc::channel::<crate::bridge::ApprovalExchange>(1);
    tokio::spawn(async move {
        let exchange = approval_rx.recv().await.unwrap();
        exchange.decision_tx.send(ApprovalDecision::Deny).unwrap();
    });
    server
        .register_session(&db, backend, Some("alpha".into()), Some(approval_tx))
        .await
        .unwrap();
    await_call_count(&mock, 2).await;
    await_no_processing(&server, &sid).await;

    let reopened = Session::new(ConversationId(sid), db).await;
    let attempt = reopened
        .attempts_for_test()
        .await
        .into_iter()
        .next()
        .unwrap();
    let transcript = reopened.turn_transcript(&attempt.attempt_id).await.unwrap();
    assert!(matches!(
        &transcript[1].message,
        crate::session::TurnTranscriptMessage::ToolResult {
            outcome: crate::runtime::ToolResultOutcome::Denied,
            ..
        }
    ));
    assert!(matches!(
        &transcript[2].message,
        crate::session::TurnTranscriptMessage::ToolResult {
            outcome: crate::runtime::ToolResultOutcome::RateLimited,
            call_index: 1,
            ..
        }
    ));
    assert!(matches!(
        &transcript[3].message,
        crate::session::TurnTranscriptMessage::ToolResult {
            outcome: crate::runtime::ToolResultOutcome::Error,
            call_index: 2,
            ..
        }
    ));
    assert!(matches!(
        &transcript[4].message,
        crate::session::TurnTranscriptMessage::ToolResult {
            outcome: crate::runtime::ToolResultOutcome::Unavailable,
            call_index: 3,
            ..
        }
    ));
    assert!(matches!(
        &transcript[5].message,
        crate::session::TurnTranscriptMessage::ModelResponse { terminal: true, .. }
    ));
}

#[tokio::test]
async fn interrupted_attempt_requires_explicit_retry() {
    use crate::test_support::MockBackend;

    let (_instance, first_server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&first_server, &registry, "alpha").await;
    register_alpha_agent_runtime(&first_server);
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    let request_id = write_user_message_with_content(&db, &sid, "side effect turn").await;

    // Simulate a process dying after it records the start and causes an
    // external effect. The new process has no completion or live attempt id.
    let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
    let old_attempt = session
        .start_turn_attempt(request_id.clone())
        .await
        .unwrap();
    drop(first_server);

    let (_instance2, restarted, _registry2) = server_fixture_from_registry(registry.clone()).await;
    register_alpha_agent_runtime(&restarted);
    restarted.agent_index().register(entry);
    let mock = Arc::new(MockBackend::new());
    mock.push_text("retry reply");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    restarted
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(mock.recorded_calls().is_empty(), "restart must not replay");
    assert_eq!(
        restarted.interrupted_turns(&sid).await.unwrap(),
        vec![super::InterruptedTurn {
            request_id: request_id.clone(),
            attempt_id: old_attempt.attempt_id.clone(),
        }]
    );

    let retry_id = restarted
        .retry_interrupted_turn(&sid, &request_id)
        .await
        .unwrap();
    assert_ne!(retry_id, old_attempt.attempt_id);
    await_call_count(&mock, 1).await;
    await_no_processing(&restarted, &sid).await;
    assert!(restarted.interrupted_turns(&sid).await.unwrap().is_empty());
}

#[tokio::test]
async fn abort_after_recorded_host_effect_stays_interrupted_until_retry() {
    use crate::test_support::{MockBackend, MockHost};

    let (_instance, server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("effect")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();

    let host = Arc::new(MockHost::new());
    host.push_shell("effect recorded", "", 0);
    host.request(
        &crate::tool_host::Capability::Shell {
            command: "external-side-effect".into(),
            working_dir: None,
        },
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(host.recorded_calls().len(), 1);

    let request_id = write_user_message_with_content(&db, &sid, "ambiguous effect").await;
    let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
    session
        .start_turn_attempt(request_id.clone())
        .await
        .unwrap();
    drop(server);

    let (_instance2, restarted, _) = server_fixture_from_registry(registry.clone()).await;
    register_alpha_agent_runtime(&restarted);
    restarted.agent_index().register(entry);
    let mock = Arc::new(MockBackend::new());
    mock.push_text("explicit retry");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    restarted
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(mock.recorded_calls().is_empty());
    assert_eq!(
        host.recorded_calls().len(),
        1,
        "restart must not repeat the effect"
    );

    restarted
        .retry_interrupted_turn(&sid, &request_id)
        .await
        .unwrap();
    await_call_count(&mock, 1).await;
}

#[tokio::test]
async fn crash_after_completed_relation_does_not_reexecute() {
    let (_instance, old_server, registry) = server_fixture().await;
    let (entry, _adb) = seed_agent(&old_server, &registry, "alpha").await;
    register_alpha_agent_runtime(&old_server);
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    let request_id = write_user_message_with_content(&db, &sid, "already completed").await;
    let mut session = Session::new(ConversationId(sid.clone()), db.clone()).await;
    let attempt = session.start_turn_attempt(request_id).await.unwrap();
    session
        .complete_turn_attempt(
            &attempt,
            Some(SessionEntry {
                sender: "alpha".into(),
                content: "durable reply".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            }),
        )
        .await
        .unwrap();
    drop(old_server);

    let (_instance2, restarted, _registry2) = server_fixture_from_registry(registry.clone()).await;
    register_alpha_agent_runtime(&restarted);
    restarted.agent_index().register(entry);
    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("duplicate reply");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    restarted
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(mock.recorded_calls().is_empty());
    let reopened = Session::new(ConversationId(sid), db).await;
    assert_eq!(
        reopened
            .entries()
            .iter()
            .filter(|entry| entry.sender == "alpha" && entry.entry_type == EntryType::Message)
            .count(),
        1
    );
}

#[tokio::test]
async fn shared_service_clients_observe_durable_attempt_completion() {
    use eidetica::NewUser;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;
    use eidetica::service::ServiceServer;
    use tokio::sync::watch;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("eidetica.sock");
    let (owner, mut owner_user) =
        Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("shared"))
            .await
            .unwrap();
    let key = owner_user.get_default_key().unwrap();
    let db = owner_user.create_database(Doc::new(), &key).await.unwrap();
    crate::session::Session::initialize_turn_schema(&db)
        .await
        .unwrap();
    let root = db.root_id().clone();
    let service = ServiceServer::bind(owner, &socket).await.unwrap();
    let (shutdown, receiver) = watch::channel(());
    let task = tokio::spawn(service.run(receiver));
    let url = format!("unix://{}", socket.display());
    let settings = || crate::config::EideticaConfig {
        connection: url.clone(),
        login: crate::config::EideticaLoginConfig {
            username: "shared".into(),
            password: None,
            passwordless: true,
        },
        sync: None,
    };
    let executor =
        crate::instance::connect_with(&settings(), crate::config::ExecutionRole::Executor)
            .await
            .unwrap();
    let client = crate::instance::connect_with(&settings(), crate::config::ExecutionRole::Client)
        .await
        .unwrap();
    assert!(client.capabilities.executor().is_err());
    executor.capabilities.executor().unwrap();
    let executor_db = executor.user.open_database(&root).await.unwrap();
    let client_db = client.user.open_database(&root).await.unwrap();
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::unbounded_channel();
    let cursor = client_db.snapshot().await.unwrap();
    let _callback = client_db
        .on_write_at_tips(cursor, move |_, _| {
            let wake_tx = wake_tx.clone();
            async move {
                wake_tx.send(()).unwrap();
                Ok(())
            }
        })
        .await
        .unwrap();

    let agents = Arc::new(AgentRegistry::with_default_agent());
    let registry = Arc::new(
        crate::session::SessionRegistry::new(executor.instance.clone(), executor.user, agents)
            .await
            .unwrap(),
    );
    let server = server_fixture_from_registry(registry.clone()).await.1;
    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("shared response");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&executor_db, backend, Some("agent".into()), None)
        .await
        .unwrap();
    let mut client_session =
        Session::new(ConversationId(root.to_string()), client_db.clone()).await;
    let request_id = client_session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "shared service turn".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    await_call_count(&mock, 1).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            wake_rx.recv().await.unwrap();
            let observed = Session::new(ConversationId(root.to_string()), client_db.clone()).await;
            if observed
                .turn_requests(|name| name == "agent", &Default::default())
                .await
                .unwrap()
                .iter()
                .any(|request| {
                    request.id == request_id
                        && matches!(request.state, TurnRequestState::Completed { .. })
                })
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let client_session = Session::new(ConversationId(root.to_string()), client_db).await;
    let requests = client_session
        .turn_requests(|name| name == "agent", &Default::default())
        .await
        .unwrap();
    assert!(matches!(
        requests
            .iter()
            .find(|request| request.id == request_id)
            .unwrap()
            .state,
        crate::session::TurnRequestState::Completed { .. }
    ));
    let attempt_id = match &requests
        .iter()
        .find(|request| request.id == request_id)
        .unwrap()
        .state
    {
        TurnRequestState::Completed { attempt_id } => attempt_id,
        state => panic!("expected completed request, got {state:?}"),
    };
    let transcript = client_session.turn_transcript(attempt_id).await.unwrap();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].request_id, request_id);
    assert!(matches!(
        transcript[0].message,
        crate::session::TurnTranscriptMessage::ModelResponse { terminal: true, .. }
    ));
    assert!(
        client_session
            .entries()
            .iter()
            .any(|entry| entry.content == "shared response")
    );

    drop(server);
    drop(client);
    drop(shutdown);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn direct_peer_sync_preserves_request_identity_and_completion() {
    use eidetica::auth::Permission;
    use eidetica::auth::types::AuthKey;
    use eidetica::crdt::Doc;
    use eidetica::sync::Address;
    use eidetica::sync::transports::http::HttpTransport;

    let (owner, mut owner_user) =
        Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("owner"))
            .await
            .unwrap();
    owner.enable_sync().await.unwrap();
    let owner_key = owner_user.get_default_key().unwrap();
    let owner_db = owner_user
        .create_database(Doc::new(), &owner_key)
        .await
        .unwrap();
    Session::initialize_turn_schema(&owner_db).await.unwrap();
    let txn = owner_db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_global_auth_key(AuthKey::active(None, Permission::Write(0)))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    owner_user.enable_sync(owner_db.root_id()).await.unwrap();
    let owner_sync = owner.sync().unwrap();
    owner_sync
        .register_transport("http", HttpTransport::builder().bind("127.0.0.1:0"))
        .await
        .unwrap();
    owner_sync.accept_connections().await.unwrap();
    let address = Address::http(owner_sync.get_server_address_for("http").await.unwrap());

    let (peer, mut peer_user) =
        Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("peer"))
            .await
            .unwrap();
    peer.enable_sync().await.unwrap();
    let peer_key = peer_user.get_default_key().unwrap();
    let peer_signing = peer_user.get_signing_key(&peer_key).unwrap();
    let peer_sync = peer.sync().unwrap();
    peer_sync
        .register_transport("http", HttpTransport::builder())
        .await
        .unwrap();
    peer_sync
        .sync_with_peer_for_bootstrap_with_key(
            &address,
            owner_db.root_id(),
            &peer_signing,
            &peer_key.to_string(),
            Permission::Write(10),
        )
        .await
        .unwrap();
    peer_sync.flush().await.unwrap();
    let (sigkey, _) = eidetica::Database::find_sigkeys(&peer, owner_db.root_id(), &peer_key)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    peer_user
        .map_key(&peer_key, owner_db.root_id(), sigkey)
        .await
        .unwrap();
    let peer_db = peer_user.open_database(owner_db.root_id()).await.unwrap();

    let agents = Arc::new(AgentRegistry::with_default_agent());
    let registry = Arc::new(
        crate::session::SessionRegistry::new(owner.clone(), owner_user, agents)
            .await
            .unwrap(),
    );
    let server = server_fixture_from_registry(registry.clone()).await.1;
    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("synced response");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&owner_db, backend, Some("agent".into()), None)
        .await
        .unwrap();

    let mut peer_session = Session::new(ConversationId("peer".into()), peer_db.clone()).await;
    let request_id = peer_session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "synced request".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    peer_sync
        .sync_with_peer(&address, Some(owner_db.root_id()))
        .await
        .unwrap();
    await_call_count(&mock, 1).await;
    let owner_session = Session::new(ConversationId("owner".into()), owner_db.clone()).await;
    assert_eq!(
        owner_session
            .turn_requests(|name| name == "agent", &Default::default())
            .await
            .unwrap()
            .into_iter()
            .find(|request| request.entry.content == "synced request")
            .unwrap()
            .id,
        request_id
    );
    peer_sync
        .sync_with_peer(&address, Some(owner_db.root_id()))
        .await
        .unwrap();

    let peer_session = Session::new(ConversationId("peer".into()), peer_db).await;
    let request = peer_session
        .turn_requests(|name| name == "agent", &Default::default())
        .await
        .unwrap()
        .into_iter()
        .find(|request| request.id == request_id)
        .unwrap();
    let attempt_id = match request.state {
        TurnRequestState::Completed { attempt_id } => attempt_id,
        state => panic!("expected completed request, got {state:?}"),
    };
    let transcript = peer_session.turn_transcript(&attempt_id).await.unwrap();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].request_id, request_id);
    assert!(
        peer_session
            .entries()
            .iter()
            .any(|entry| entry.content == "synced response")
    );
    drop(server);
    owner_sync.stop_server().await.unwrap();
}

/// Poll the session for a `Message` entry sent by `agent`, up to a few
/// seconds. The wake path runs on the server's spawned processing loop, so
/// the reply lands asynchronously with no completion signal to await.
async fn await_agent_reply(session_db: &eidetica::Database, sid: &str, agent: &str) -> bool {
    for _ in 0..100 {
        let session = crate::session::Session::new(
            crate::types::ConversationId(sid.to_string()),
            session_db.clone(),
        )
        .await;
        if session
            .entries()
            .iter()
            .any(|e| e.sender == agent && e.entry_type == EntryType::Message)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// Register a Living Agent DB named `name` so auto-attach can find it.
async fn register_agent(
    server: &Arc<Server>,
    registry: &Arc<crate::session::SessionRegistry>,
    name: &str,
) {
    let (db, pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            name,
            &AgentDbConfig::default(),
            &AgentMeta {
                display_name: Some(name.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    server.agent_index().register(DbEntry {
        db_id: db.id(),
        display_name: name.to_string(),
        pubkey,
    });
}

#[tokio::test]
async fn auto_attach_honours_the_selected_agent_group() {
    let (_instance, server, registry) = server_fixture().await;
    register_agent(&server, &registry, "alpha").await;
    register_agent(&server, &registry, "beta").await;

    server.set_default_agents(vec!["alpha".to_string()]);
    server.set_agent_groups(HashMap::from([
        (
            "pair".to_string(),
            vec!["beta".to_string(), "alpha".to_string()],
        ),
        ("solo".to_string(), vec!["beta".to_string()]),
        ("empty".to_string(), Vec::new()),
    ]));

    // No group named: the peer default.
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    assert_eq!(
        server
            .auto_attach_agents(&db.root_id().to_string(), None)
            .await,
        vec!["alpha".to_string()]
    );

    // A named group replaces the default, and keeps configured order —
    // the first entry is the routing host.
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    let session_db_id = db.root_id().to_string();
    assert_eq!(
        server
            .auto_attach_agents(&session_db_id, Some("pair"))
            .await,
        vec!["beta".to_string(), "alpha".to_string()]
    );
    let meta = crate::session::read_meta_from_db(&db).await;
    let attached: Vec<&str> = meta
        .agents
        .iter()
        .map(|a| a.display_name.as_str())
        .collect();
    assert_eq!(attached, vec!["beta", "alpha"]);

    // An explicitly-empty group attaches nothing rather than falling back.
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    assert!(
        server
            .auto_attach_agents(&db.root_id().to_string(), Some("empty"))
            .await
            .is_empty()
    );

    // An unknown group attaches nothing — the command layer rejects it
    // before a session is ever created.
    let (_conv, db) = registry.create_session(Some("t")).await.unwrap();
    assert!(
        server
            .auto_attach_agents(&db.root_id().to_string(), Some("nope"))
            .await
            .is_empty()
    );

    assert_eq!(
        server.agent_group_names(),
        vec!["empty".to_string(), "pair".to_string(), "solo".to_string()]
    );
    assert_eq!(server.agent_group("solo"), Some(vec!["beta".to_string()]));
    assert!(server.agent_group("nope").is_none());
}
