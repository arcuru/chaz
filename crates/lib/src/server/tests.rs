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

    // Single backend whose first (default) model is flash — the Ava shape.
    let mut b = crate::config::Backend::new(crate::config::BackendType::OpenAICompatible);
    b.name = Some("openrouter".to_string());
    b.models = Some(vec![crate::config::Model {
        name: "deepseek/deepseek-v4-flash".to_string(),
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
    );
    (instance, server, registry)
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
    // `system_prompt`) — the exact shape of the Ava config.
    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("AGENTS.md");
    std::fs::write(&prompt_path, "You are Ava. Operating manual v1.").unwrap();
    let ac: crate::config::AgentConfig = serde_yaml::from_str(&format!(
        "name: ava\nsystem_prompt_files: [\"{}\"]\n",
        prompt_path.display()
    ))
    .unwrap();

    // Bootstrap the agent DB the way startup would: declarative config
    // (paths), but no resolved-prompt ref yet.
    let (db, pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            "ava",
            &crate::agent_db::AgentDbConfig::from_agent_config(&ac),
            &AgentMeta {
                display_name: Some("ava".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    server.agent_index().register(DbEntry {
        db_id: db.id(),
        display_name: "ava".to_string(),
        pubkey,
    });

    // First reconcile applies and sets the prompt ref.
    assert!(server.reconcile_agent_from_yaml(&ac).await.unwrap());
    let cfg = db.read_config().await.unwrap();
    assert!(cfg.system_prompt_ref.is_some(), "ref set after reconcile");
    assert!(cfg.applied_config_hash.is_some());

    // Hydration resolves the prompt from the blob (config has no inline text).
    let input = crate::agent::Agent {
        name: "ava".to_string(),
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
        grants: HashMap::new(),
    };
    let hydrated = server.hydrate_agent_from_db(input.clone()).await;
    assert_eq!(hydrated.system_prompt, "You are Ava. Operating manual v1.");

    // Unchanged yaml + file → gate matches → no-op.
    assert!(!server.reconcile_agent_from_yaml(&ac).await.unwrap());

    // Editing the file content makes the resolved prompt change, so
    // reconcile applies again and hydration reflects the new text.
    std::fs::write(&prompt_path, "You are Ava. Operating manual v2!").unwrap();
    assert!(server.reconcile_agent_from_yaml(&ac).await.unwrap());
    let hydrated2 = server.hydrate_agent_from_db(input).await;
    assert_eq!(hydrated2.system_prompt, "You are Ava. Operating manual v2!");
}

#[tokio::test]
async fn reload_config_for_rereads_yaml_from_disk() {
    // `/agent reload` path: a config file on disk drives the reconcile via
    // the server-held config path, not a pre-parsed Config in hand.
    let (_instance, server, registry) = server_fixture().await;

    let dir = tempfile::tempdir().unwrap();
    let prompt_path = dir.path().join("AGENTS.md");
    std::fs::write(&prompt_path, "Ava manual v1.").unwrap();
    let config_path = dir.path().join("config.yaml");
    let write_config = |body: &str| {
        std::fs::write(
                &config_path,
                format!(
                    "homeserver_url: http://localhost\nusername: test\nagents:\n  - name: ava\n    system_prompt_files: [\"{}\"]\n{}",
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
        "name: ava\nsystem_prompt_files: [\"{}\"]\n",
        prompt_path.display()
    ))
    .unwrap();
    let (db, pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            "ava",
            &crate::agent_db::AgentDbConfig::from_agent_config(&ac),
            &AgentMeta {
                display_name: Some("ava".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    server.agent_index().register(DbEntry {
        db_id: db.id(),
        display_name: "ava".to_string(),
        pubkey,
    });

    // Scoped reload applies and reports the change.
    let report = server.reload_config_for(Some("ava")).await.unwrap();
    assert_eq!(report.changed, vec!["ava".to_string()]);
    assert_eq!(report.considered, 1);

    // A second reload with the file unchanged is a gated no-op.
    let report2 = server.reload_config_for(Some("ava")).await.unwrap();
    assert!(report2.changed.is_empty());
    assert_eq!(report2.considered, 1);

    // Editing the prompt file and reloading reaches hydration.
    std::fs::write(&prompt_path, "Ava manual v2!").unwrap();
    let report3 = server.reload_config_for(None).await.unwrap();
    assert_eq!(report3.changed, vec!["ava".to_string()]);
    let input = crate::agent::Agent {
        name: "ava".to_string(),
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
        grants: HashMap::new(),
    };
    let hydrated = server.hydrate_agent_from_db(input).await;
    assert_eq!(hydrated.system_prompt, "Ava manual v2!");

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
    // register_session is what real callers (gateways) do; without it
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
        })
        .await;
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

    server.process_session(&sid).await.unwrap();

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

    server.process_session(&sid).await.unwrap();

    // Gate passed → spawn_agent_task was called → spawned tokio task
    // is pending on current_thread runtime; lock is still held.
    assert!(server.processing.lock().await.contains(&sid));
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

    server.process_session(&sid).await.unwrap();

    assert!(server.processing.lock().await.contains(&sid));
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

    let payload = fresh_schedule_payload(&entry.db_id.to_string(), "f1", "wake");
    let _ = server.fire_agent_schedule(payload).await;
    // Even with the LLM call failing (no backends), a ScheduleFire is
    // recorded — which only happens when we get past the gate.
    let fires = adb.list_schedule_fires().await.unwrap();
    assert_eq!(fires.len(), 1, "legacy None must let the fire through");
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
