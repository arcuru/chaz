//! Real due-fire loop, installed handler, and both server generation guards.
//! Model requests use MockBackend; no provider clients or external tools exist.

use super::*;
use crate::routine::AgentSchedulePayload;

struct OneShotDispatchFixture {
    server: Arc<Server>,
    registry: Arc<crate::session::SessionRegistry>,
    engine: Arc<crate::routine::RoutineEngine>,
    adb: crate::agent_db::AgentDb,
    owner_id: String,
    routine_id: crate::routine::RoutineId,
    session_db: eidetica::Database,
    mock: Arc<crate::test_support::MockBackend>,
    gate: Arc<crate::extensions::agent_schedule::ScheduleDispatchBarrier>,
    dispatch: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

async fn one_shot_dispatch_fixture(trigger: Trigger) -> OneShotDispatchFixture {
    use crate::extension::{ExtensionHub, PeerHandles};
    use crate::extensions::agent_schedule::{AgentScheduleExtension, ScheduleDispatchBarrier};

    let (_instance, original, registry) = server_fixture().await;
    original.shutdown().await;
    let slot = crate::instance::ServerSlot::default();
    let gate = Arc::new(ScheduleDispatchBarrier::default());
    let tools = Arc::new(ToolRegistry::new());
    let mcp = Arc::new(crate::mcp::McpRegistry::new());
    let index = HostedIndex::empty("agent");
    let banks = HostedIndex::empty("bank");
    let skills = HostedIndex::empty("skill_bank");
    let mut hub = ExtensionHub::new();
    hub.set_session_registry(registry.clone());
    hub.set_peer_handles(Arc::new(PeerHandles {
        registry: registry.clone(),
        agent_index: index.clone(),
        memory_bank_index: banks.clone(),
        skill_bank_index: skills.clone(),
        embedder: None,
        secrets: None,
        server_slot: slot.clone(),
        mcp_registry: mcp.clone(),
        agent_state_allowlist: Default::default(),
        tool_registry: tools.clone(),
    }));
    hub.set_context_strategy_grant(&Default::default()).unwrap();
    hub.install_all(vec![
        Arc::new(crate::extensions::context::BaselineContext),
        Arc::new(AgentScheduleExtension::new(slot.clone()).with_dispatch_barrier(gate.clone())),
    ])
    .await
    .unwrap();
    let hub = Arc::new(hub);
    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_text("scheduled-probe-marker");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    assert!(
        backend.list_known_backends().is_empty(),
        "no real API clients"
    );
    let server = Server::new(
        registry.clone(),
        registry.agents.clone(),
        index,
        banks,
        skills,
        tools,
        Arc::new(crate::tool::ToolPolicyRegistry::empty()),
        crate::test_support::permissive_security(),
        HashMap::new(),
        Default::default(),
        Arc::new(crate::test_support::MockHost::new()),
        hub.clone(),
        backend.clone(),
        mcp,
        Some(crate::instance::ExecutorCapability::for_test()),
    );
    slot.set(server.clone());
    let (entry, adb) = seed_agent(&server, &registry, "probe-agent").await;
    let owner_id = entry.db_id.to_string();
    let (_conv, session_db) = registry.create_session(Some("probe-pinned")).await.unwrap();
    let sid = session_db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    server
        .register_session(&session_db, backend, Some("probe-agent".into()), None)
        .await
        .unwrap();
    adb.upsert_schedule(Schedule::new(
        "probe-wake",
        trigger,
        "private-probe-prompt",
        crate::agent_db::ScheduleTarget::Pinned { session_db_id: sid },
    ))
    .await
    .unwrap();
    let adb = registry.ensure_scheduling_agent_db(&entry).await.unwrap();
    let schedule = adb.find_schedule("probe-wake").await.unwrap().unwrap();
    for _ in 0..100 {
        if adb
            .reconcile_schedule_accounting(&schedule, 32)
            .await
            .unwrap()
            .complete
        {
            break;
        }
    }
    let engine = crate::routine::RoutineEngine::new(registry.chaz_peer().clone(), Some(hub))
        .await
        .unwrap();
    engine.register_agent(&owner_id, &adb).await.unwrap();
    server.set_routine_engine(engine.clone());
    let routine_id = crate::routine::RoutineId::new(format!("agent:{owner_id}:probe-wake"));
    OneShotDispatchFixture {
        server,
        registry,
        engine,
        adb,
        owner_id,
        routine_id,
        session_db,
        mock,
        gate,
        dispatch: Mutex::new(None),
    }
}

async fn one_shot_dispatch_park(f: &OneShotDispatchFixture) -> AgentSchedulePayload {
    let calls_before = f.mock.recorded_calls().len();
    let generation = f.engine.current_generation(&f.routine_id).await.unwrap();
    // Run the actual due loop and installed handler. Bounded admission keeps
    // the token live until handoff, so the timer parks alongside the callback.
    let engine = f.engine.clone();
    let id = f.routine_id.clone();
    *f.dispatch.lock().await = Some(tokio::spawn(async move {
        engine.fire_due_for_test(&id).await;
    }));
    tokio::time::timeout(std::time::Duration::from_secs(10), f.gate.parked.notified())
        .await
        .expect("actual handler spawned and parked callback");
    let payload = f.gate.payload.lock().unwrap().clone().unwrap();
    assert_eq!(payload.generation.as_deref(), Some(generation.as_str()));
    assert_eq!(payload.prompt, "private-probe-prompt");
    assert_eq!(
        f.mock.recorded_calls().len(),
        calls_before,
        "callback still behind barrier"
    );
    payload
}

async fn one_shot_dispatch_finish(f: &OneShotDispatchFixture) -> (usize, usize, usize) {
    f.gate.release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.gate.finished.notified(),
    )
    .await
    .expect("actual callback must finish, including stale-skip paths");
    assert_eq!(f.gate.result.lock().unwrap().as_ref(), Some(&Ok(())));
    if let Some(dispatch) = f.dispatch.lock().await.take() {
        tokio::time::timeout(std::time::Duration::from_secs(10), dispatch)
            .await
            .unwrap()
            .unwrap();
    }
    let view = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    let handoffs = view.list_schedule_fires().await.unwrap();
    if !handoffs.is_empty() {
        let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                pump.step(&f.server).await;
                let session = Session::new(
                    ConversationId(f.session_db.root_id().to_string()),
                    f.session_db.clone(),
                )
                .await;
                if session
                    .entries()
                    .iter()
                    .any(|entry| entry.content == "scheduled-probe-marker")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            while f
                .server
                .processing
                .lock()
                .await
                .contains(&f.session_db.root_id().to_string())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("committed timer handoff reaches ordinary final");
    }
    let calls = f.mock.recorded_calls().len();
    let view = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    let fires = view.list_schedule_fires().await.unwrap().len();
    let session = Session::new(
        ConversationId(f.session_db.root_id().to_string()),
        f.session_db.clone(),
    )
    .await;
    let markers = session
        .entries()
        .iter()
        .filter(|e| e.entry_type == EntryType::Message && e.content == "scheduled-probe-marker")
        .count();
    assert_eq!(
        f.registry.list_sessions().await.unwrap().len(),
        1,
        "Pinned never creates a session"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), f.server.shutdown())
        .await
        .expect("join/abort all owned in-memory test tasks");
    (calls, fires, markers)
}

fn one_shot_dispatch_interval() -> Trigger {
    Trigger::Interval {
        period: std::time::Duration::from_secs(60),
    }
}

#[tokio::test]
async fn schedule_lifecycle_valid_due_one_shot_runs_exactly_once() {
    let f = one_shot_dispatch_fixture(Trigger::OneShot {
        fire_at: Utc::now() - chrono::Duration::seconds(1),
    })
    .await;
    let payload = one_shot_dispatch_park(&f).await;
    assert!(payload.one_shot);
    let observed = one_shot_dispatch_finish(&f).await;
    assert_eq!(
        observed,
        (1, 1, 1),
        "valid due one-shot must reach exactly one mock scheduled turn/audit/marker, not silently disappear"
    );
}

#[tokio::test]
async fn schedule_lifecycle_pinned_timer_refuses_read_only_history() {
    let f = one_shot_dispatch_fixture(Trigger::OneShot {
        fire_at: Utc::now() - chrono::Duration::seconds(1),
    })
    .await;
    one_shot_dispatch_park(&f).await;
    let schedule = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    let cursor = f.adb.schedule_clock_cursor(&schedule).await.unwrap();
    let boundary = f.session_db.snapshot().await.unwrap();
    let active = crate::session::activate_conversation_for_test(&f.session_db, &boundary).await;

    let error = release_timed_dispatch(&f).await.unwrap_err();
    assert!(error.contains("v1 conversation is read-only"), "{error}");
    assert!(f.adb.list_schedule_fires().await.unwrap().is_empty());
    assert_eq!(
        f.adb.schedule_clock_cursor(&schedule).await.unwrap(),
        cursor
    );
    assert_eq!(f.session_db.snapshot().await.unwrap(), active);
    assert!(f.mock.recorded_calls().is_empty());
    f.server.shutdown().await;
}

#[tokio::test]
async fn schedule_lifecycle_interval_live_positive() {
    let f = one_shot_dispatch_fixture(one_shot_dispatch_interval()).await;
    let payload = one_shot_dispatch_park(&f).await;
    assert!(!payload.one_shot);
    assert_eq!(
        f.engine.current_generation(&f.routine_id).await,
        payload.generation
    );
    assert_eq!(one_shot_dispatch_finish(&f).await, (1, 1, 1));
    let row = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    assert!(row.enabled);
    assert_eq!(row.fire_count, 0, "no per-fire Agent definition writes");
    let view = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        view.effective_schedule("probe-wake")
            .await
            .unwrap()
            .unwrap()
            .fire_count,
        1
    );
}

#[tokio::test]
async fn schedule_lifecycle_cron_live_positive() {
    let f = one_shot_dispatch_fixture(Trigger::Cron {
        expr: "0 0 9 * * *".into(),
    })
    .await;
    let payload = one_shot_dispatch_park(&f).await;
    assert!(!payload.one_shot);
    assert_eq!(
        f.engine.current_generation(&f.routine_id).await,
        payload.generation
    );
    assert_eq!(one_shot_dispatch_finish(&f).await, (1, 1, 1));
}

#[tokio::test]
async fn schedule_lifecycle_removed_stale_negative() {
    let f = one_shot_dispatch_fixture(one_shot_dispatch_interval()).await;
    one_shot_dispatch_park(&f).await;
    f.adb.remove_schedule("probe-wake").await.unwrap();
    f.engine
        .clear_agent_schedule_anchor(&f.owner_id, "probe-wake")
        .await
        .unwrap();
    f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();

    assert_eq!(one_shot_dispatch_finish(&f).await, (0, 0, 0));
    assert!(f.adb.find_schedule("probe-wake").await.unwrap().is_none());
}

#[tokio::test]
async fn schedule_lifecycle_same_id_prompt_replaced_stale_negative() {
    let f = one_shot_dispatch_fixture(one_shot_dispatch_interval()).await;
    let payload = one_shot_dispatch_park(&f).await;
    let mut replacement = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    replacement.prompt = "replacement-prompt".into();
    f.adb.upsert_schedule(replacement).await.unwrap();
    f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
    let current = f.engine.current_generation(&f.routine_id).await;
    assert!(current.is_some());
    assert_ne!(
        current, payload.generation,
        "prompt-only edit invalidates detached payload"
    );
    assert_eq!(one_shot_dispatch_finish(&f).await, (0, 0, 0));
    let row = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    assert_eq!(row.prompt, "replacement-prompt");
    assert_eq!(row.fire_count, 0);
}

#[tokio::test]
async fn schedule_lifecycle_interval_noop_reload_positive() {
    let f = one_shot_dispatch_fixture(one_shot_dispatch_interval()).await;
    let payload = one_shot_dispatch_park(&f).await;
    f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
    assert_eq!(
        f.engine.current_generation(&f.routine_id).await,
        payload.generation,
        "true no-op interval reload preserves incarnation"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        f.engine.fire_due_without_rearming_for_test(),
    )
    .await
    .expect("no-op interval reload must not redispatch an in-flight slot");
    assert_eq!(one_shot_dispatch_finish(&f).await, (1, 1, 1));
}

#[tokio::test]
async fn schedule_lifecycle_noop_reload_keeps_disarmed_calendar_slots() {
    for trigger in [
        Trigger::OneShot {
            fire_at: Utc::now() - chrono::Duration::seconds(1),
        },
        Trigger::Cron {
            expr: "0 * * * * *".into(),
        },
    ] {
        let f = one_shot_dispatch_fixture(trigger).await;
        let payload = one_shot_dispatch_park(&f).await;
        f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
        let current = f.engine.current_generation(&f.routine_id).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            f.engine.fire_due_without_rearming_for_test(),
        )
        .await
        .expect("no-op calendar reload cannot arm a second callback");
        assert_eq!(one_shot_dispatch_finish(&f).await, (1, 1, 1));
        assert_eq!(
            current, payload.generation,
            "true no-op calendar reload preserves the admission token"
        );
    }
}

#[tokio::test]
async fn schedule_lifecycle_one_shot_live_guard_positive() {
    // Discriminating control: the identical engine-produced one-shot payload
    // can run through both actual guards while its generation remains live.
    // This deliberately calls the callback directly, without handler ack.
    let f = one_shot_dispatch_fixture(Trigger::OneShot {
        fire_at: Utc::now() - chrono::Duration::seconds(1),
    })
    .await;
    let payload = one_shot_dispatch_park(&f).await;
    assert!(payload.generation.is_some());
    assert!(payload.one_shot);
    assert!(payload.clock.is_some());
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.server.fire_agent_schedule(payload),
    )
    .await
    .expect("direct guarded callback commits handoff")
    .unwrap();
    complete_timed_input(&f, &f.session_db.root_id().to_string()).await;
    let calls = f.mock.recorded_calls().len();
    let view = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    let fires = view.list_schedule_fires().await.unwrap().len();
    let session = Session::new(
        ConversationId(f.session_db.root_id().to_string()),
        f.session_db.clone(),
    )
    .await;
    let markers = session
        .entries()
        .iter()
        .filter(|e| e.entry_type == EntryType::Message && e.content == "scheduled-probe-marker")
        .count();
    assert_eq!(one_shot_dispatch_finish(&f).await, (1, 1, 1));
    assert_eq!((calls, fires, markers), (1, 1, 1));
}

#[tokio::test]
async fn schedule_lifecycle_disabled_target_and_trigger_edits_reject_detached_payload() {
    for edit in ["disable", "target", "trigger", "policy", "lifetime"] {
        let f = one_shot_dispatch_fixture(one_shot_dispatch_interval()).await;
        let payload = one_shot_dispatch_park(&f).await;
        let mut replacement = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
        match edit {
            "disable" => replacement.enabled = false,
            "target" => replacement.target = crate::agent_db::ScheduleTarget::Fresh,
            "policy" => replacement.max_fires = Some(5),
            "lifetime" => replacement.lifetime = Some(uuid::Uuid::new_v4().to_string()),
            "trigger" => {
                replacement.trigger = Trigger::OneShot {
                    fire_at: Utc::now() + chrono::Duration::hours(1),
                }
            }
            _ => unreachable!(),
        }
        f.adb.upsert_schedule(replacement.clone()).await.unwrap();
        f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
        let current = f.engine.current_generation(&f.routine_id).await;
        assert_eq!(one_shot_dispatch_finish(&f).await, (0, 0, 0), "{edit}");
        assert_ne!(current, payload.generation, "{edit}");
        assert_eq!(
            f.adb.find_schedule("probe-wake").await.unwrap(),
            Some(replacement),
            "{edit}"
        );
    }
}

async fn release_timed_dispatch(f: &OneShotDispatchFixture) -> Result<(), String> {
    f.gate.release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.gate.finished.notified(),
    )
    .await
    .unwrap();
    if let Some(task) = f.dispatch.lock().await.take() {
        task.await.unwrap();
    }
    f.gate.result.lock().unwrap().clone().unwrap()
}

async fn complete_timed_input(f: &OneShotDispatchFixture, sid: &str) {
    let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            pump.step(&f.server).await;
            let (conv, db) = f.registry.open_session(sid).await.unwrap();
            let session = Session::new(conv, db).await;
            if session
                .entries()
                .iter()
                .any(|e| e.content == "scheduled-probe-marker")
                && !f.server.processing.lock().await.contains(sid)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable timer handoff must reach the common runner");
}

#[tokio::test]
async fn schedule_lifecycle_timer_handoff_is_nonblocking_at_full_common_capacity() {
    let f = one_shot_dispatch_fixture(Trigger::OneShot {
        fire_at: Utc::now(),
    })
    .await;
    let permit = f
        .server
        .semaphore
        .clone()
        .acquire_many_owned(f.server.semaphore.available_permits() as u32)
        .await
        .unwrap();
    let payload = one_shot_dispatch_park(&f).await;
    assert!(payload.clock.is_some());
    assert!(release_timed_dispatch(&f).await.is_ok());
    let unstarted = Session::new(
        ConversationId(f.session_db.root_id().to_string()),
        f.session_db.clone(),
    )
    .await;
    assert_eq!(
        unstarted.attempts_for_test().await.len(),
        0,
        "full capacity must precede ANY Started attempt"
    );
    let agent = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    let row = agent.schedule_occurrences().await.unwrap().pop().unwrap();
    assert!(matches!(
        row.preparation,
        crate::agent_db::Preparation::HandedOff { .. }
    ));
    let cursor = agent
        .schedule_clock_cursor(&agent.find_schedule("probe-wake").await.unwrap().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cursor.position, 1);
    assert!(cursor.next.is_none());
    assert!(f.engine.current_generation(&f.routine_id).await.is_none());
    let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
    for _ in 0..12 {
        pump.step(&f.server).await;
    }
    let session = Session::new(
        ConversationId(f.session_db.root_id().to_string()),
        f.session_db.clone(),
    )
    .await;
    assert!(
        session.attempts_for_test().await.is_empty(),
        "capacity before Started"
    );
    assert!(f.mock.recorded_calls().is_empty());
    assert!(f.engine.current_generation(&f.routine_id).await.is_none());
    // A late callback has no generation authority, even though its immutable
    // definition and private input remain readable.
    f.server.dispatch_agent_schedule(payload).await.unwrap();
    assert_eq!(
        agent.schedule_occurrences().await.unwrap(),
        vec![row.clone()]
    );
    drop(permit);
    complete_timed_input(&f, &f.session_db.root_id().to_string()).await;
    let session = Session::new(
        ConversationId(f.session_db.root_id().to_string()),
        f.session_db.clone(),
    )
    .await;
    let evidence = session
        .schedule_delivery_evidence(&row.reference)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        evidence.input.state,
        crate::session::steering::JobInputState::Included { .. }
    ));
    assert_eq!(
        evidence.completion.unwrap().outcome,
        crate::session::outcome::TurnOutcome::Success
    );
    assert_eq!(f.mock.recorded_calls().len(), 1);
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    assert!(
        !serde_json::to_string(&session.entries())
            .unwrap()
            .contains("private-probe-prompt")
    );
    f.server.shutdown().await;
}

#[tokio::test]
async fn schedule_lifecycle_timer_fresh_preparation_reuses_one_root_and_lost_ack_repairs_clock() {
    let f = one_shot_dispatch_fixture(one_shot_dispatch_interval()).await;
    let mut definition = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    definition.target = crate::agent_db::ScheduleTarget::Fresh;
    f.adb.upsert_schedule(definition.clone()).await.unwrap();
    f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
    *f.server.schedule_fail_at.lock().unwrap() = Some("timed-handoff");
    let payload = one_shot_dispatch_park(&f).await;
    assert!(
        release_timed_dispatch(&f)
            .await
            .unwrap_err()
            .contains("timed-handoff")
    );
    let agent = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    let prepared = agent.schedule_occurrences().await.unwrap().pop().unwrap();
    let crate::schedule_state::lifecycle::FreshRoot::Bound { session_db_id: sid } =
        prepared.fresh_root.clone().unwrap()
    else {
        panic!("Fresh root not bound");
    };
    assert_eq!(f.registry.list_sessions().await.unwrap().len(), 2);
    assert!(f.mock.recorded_calls().is_empty());
    assert!(agent.list_schedule_fires().await.unwrap().is_empty());
    *f.server.schedule_fail_at.lock().unwrap() = Some("timed-handoff-ack");
    let error = f
        .server
        .dispatch_agent_schedule(payload.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed-handoff-ack"), "{error}");
    let handed = agent.schedule_occurrences().await.unwrap().pop().unwrap();
    assert_eq!(handed.reference, prepared.reference);
    assert_eq!(handed.fresh_root, prepared.fresh_root);
    assert_eq!(f.registry.list_sessions().await.unwrap().len(), 2);
    let cursor = agent
        .schedule_clock_cursor(&definition)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cursor.position, 1);
    let crate::agent_db::DueSlot::Interval { anchor, ordinal } = cursor.next.clone().unwrap()
    else {
        panic!("interval successor");
    };
    assert_eq!(ordinal, 1);
    // Same process, same token: read back the committed successor before
    // attempting any Fresh provisioning. This used to refuse HandedOff.
    *f.server.schedule_fail_at.lock().unwrap() = None;
    f.server
        .dispatch_agent_schedule(payload.clone())
        .await
        .unwrap();
    assert_eq!(
        agent.schedule_clock_cursor(&definition).await.unwrap(),
        Some(cursor)
    );
    // Lose/delete the disposable local projection; restarting the clock must
    // read the exact selected-ledger successor, not choose process time.
    f.engine
        .clear_agent_schedule_anchor(&f.owner_id, "probe-wake")
        .await
        .unwrap();
    let restarted = crate::routine::RoutineEngine::new(f.registry.chaz_peer().clone(), None)
        .await
        .unwrap();
    restarted.register_agent(&f.owner_id, &agent).await.unwrap();
    let txn = f.registry.chaz_peer().new_transaction().await.unwrap();
    let projected = txn
        .get_store::<eidetica::store::DocStore>(crate::routine::LAST_FIRED_STORE)
        .await
        .unwrap()
        .get_string(f.routine_id.as_str())
        .await
        .unwrap();
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&projected)
            .unwrap()
            .to_utc(),
        anchor
    );
    // Reload from the same authority is also a repair, not another occurrence.
    f.engine.reload_agent(&f.owner_id, &agent).await.unwrap();
    assert_ne!(
        f.engine.current_generation(&f.routine_id).await,
        payload.generation
    );
    *f.server.schedule_fail_at.lock().unwrap() = None;
    f.server.dispatch_agent_schedule(payload).await.unwrap();
    complete_timed_input(&f, &sid).await;
    assert_eq!(agent.schedule_occurrences().await.unwrap(), vec![handed]);
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    assert_eq!(f.mock.recorded_calls().len(), 1);
    assert_eq!(f.registry.list_sessions().await.unwrap().len(), 2);
    f.server.shutdown().await;
}

#[tokio::test]
async fn schedule_lifecycle_bound_fresh_root_is_not_reattached_after_detach() {
    let f = one_shot_dispatch_fixture(one_shot_dispatch_interval()).await;
    let mut definition = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    definition.target = crate::agent_db::ScheduleTarget::Fresh;
    f.adb.upsert_schedule(definition).await.unwrap();
    f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
    *f.server.schedule_fail_at.lock().unwrap() = Some("timed-handoff");
    let payload = one_shot_dispatch_park(&f).await;
    assert!(release_timed_dispatch(&f).await.is_err());
    let entry = f.server.agent_index.find_by_id(&f.adb.id()).unwrap();
    let agent = f.registry.open_scheduling_writer(&entry).await.unwrap();
    let row = agent.schedule_occurrences().await.unwrap().pop().unwrap();
    let Some(crate::schedule_state::lifecycle::FreshRoot::Bound { session_db_id }) = row.fresh_root
    else {
        panic!("expected bound root")
    };
    f.server
        .prepare_fresh_schedule_root(&row.reference)
        .await
        .unwrap();
    f.registry
        .detach_agent_from_session(&session_db_id, &entry)
        .await
        .unwrap();
    *f.server.schedule_fail_at.lock().unwrap() = None;
    assert!(f.server.dispatch_agent_schedule(payload).await.is_err());
    let (_, db) = f.registry.open_session(&session_db_id).await.unwrap();
    assert!(
        crate::session::try_read_meta_from_db(&db)
            .await
            .unwrap()
            .agents
            .is_empty()
    );
    assert!(agent.list_schedule_fires().await.unwrap().is_empty());
    assert!(f.mock.recorded_calls().is_empty());
    f.server.shutdown().await;
}

#[tokio::test]
async fn schedule_lifecycle_refunded_last_slot_restores_cold_interval_entry() {
    let f = one_shot_dispatch_fixture(one_shot_dispatch_interval()).await;
    let mut definition = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    definition.target = crate::agent_db::ScheduleTarget::Fresh;
    definition.max_fires = Some(1);
    f.adb.upsert_schedule(definition.clone()).await.unwrap();
    f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
    one_shot_dispatch_park(&f).await;
    assert_eq!(release_timed_dispatch(&f).await, Ok(()));
    let entry = f.server.agent_index.find_by_id(&f.adb.id()).unwrap();
    let agent = f.registry.open_scheduling_writer(&entry).await.unwrap();
    let row = agent.schedule_occurrences().await.unwrap().pop().unwrap();
    let crate::schedule_state::lifecycle::Preparation::HandedOff { input, .. } = row.preparation
    else {
        panic!("expected handoff")
    };
    let sid = &input.session_db_id;
    // Persist the terminal outcome but leave the projection missing. Stop
    // discovery in this fixture so background scheduling cannot refund it.
    f.server.mark_startup_pending();
    let (conv, db, key) = f.registry.open_session_with_signer(sid).await.unwrap();
    let mut s = Session::try_new(conv, db)
        .await
        .unwrap()
        .with_signing_key(key);
    let attempt = s
        .start_schedule_input_as(&row.reference, "failed-attempt".into(), None)
        .await
        .unwrap()
        .unwrap();
    s.complete_turn_attempt(
        &attempt,
        Some(SessionEntry {
            sender: "probe-agent".into(),
            content: "failed before projection".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Error,
            metadata: None,
            routing: None,
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        s.turn_completion(&attempt.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        crate::session::outcome::TurnOutcome::Failure
    );
    let clock = crate::routine::RoutineEngine::new(f.registry.chaz_peer().clone(), None)
        .await
        .unwrap();
    clock.register_agent(&f.owner_id, &agent).await.unwrap();
    assert!(
        clock.current_generation(&f.routine_id).await.is_none(),
        "cold load with last slot held"
    );
    let cursor = agent.schedule_clock_cursor(&definition).await.unwrap();
    crate::schedule_state::lifecycle::settle(&agent, &row.reference, &s)
        .await
        .unwrap();
    clock
        .resume_agent_preparation(&agent, &definition)
        .await
        .unwrap();
    assert!(clock.current_generation(&f.routine_id).await.is_some());
    assert_eq!(
        agent.schedule_clock_cursor(&definition).await.unwrap(),
        cursor
    );
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    f.server.shutdown().await;
}

#[tokio::test]
async fn schedule_lifecycle_timer_final_guard_keeps_stale_inert_inputs_unstarted() {
    for edit in ["remove", "replace", "disable", "detach"] {
        let f = one_shot_dispatch_fixture(Trigger::OneShot {
            fire_at: Utc::now(),
        })
        .await;
        let barrier =
            Arc::new(crate::extensions::agent_schedule::ScheduleDispatchBarrier::default());
        *f.server.timed_prepared_barrier.lock().unwrap() = Some(barrier.clone());
        one_shot_dispatch_park(&f).await;
        f.gate.release.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            barrier.parked.notified(),
        )
        .await
        .unwrap();
        let agent = f
            .registry
            .open_scheduling_agent_db(&f.adb.id(), None)
            .await
            .unwrap()
            .unwrap();
        let row = agent.schedule_occurrences().await.unwrap().pop().unwrap();
        let session = Session::new(
            ConversationId(f.session_db.root_id().to_string()),
            f.session_db.clone(),
        )
        .await;
        assert!(
            session
                .schedule_input(&row.reference)
                .await
                .unwrap()
                .is_some()
        );
        match edit {
            "remove" => {
                f.adb.remove_schedule("probe-wake").await.unwrap();
            }
            "replace" | "disable" => {
                let mut definition = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
                if edit == "replace" {
                    definition.lifetime = Some(uuid::Uuid::new_v4().to_string());
                } else {
                    definition.enabled = false;
                }
                f.adb.upsert_schedule(definition).await.unwrap();
            }
            "detach" => {
                let entry = f.server.agent_index.find_by_id(&f.adb.id()).unwrap();
                f.registry
                    .detach_agent_from_session(&f.session_db.root_id().to_string(), &entry)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        if edit != "detach" {
            f.engine.reload_agent(&f.owner_id, &agent).await.unwrap();
        }
        barrier.release.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            f.gate.finished.notified(),
        )
        .await
        .unwrap();
        f.dispatch.lock().await.take().unwrap().await.unwrap();
        assert!(f.mock.recorded_calls().is_empty(), "{edit}");
        assert!(
            agent.list_schedule_fires().await.unwrap().is_empty(),
            "{edit}"
        );
        assert!(session.attempts_for_test().await.is_empty(), "{edit}");
        assert!(
            session
                .schedule_delivery_evidence(&row.reference)
                .await
                .unwrap()
                .is_none(),
            "{edit}"
        );
        f.server.shutdown().await;
    }
}

#[tokio::test]
async fn schedule_lifecycle_timer_noop_reload_retains_its_own_final_reservation() {
    let f = one_shot_dispatch_fixture(Trigger::OneShot {
        fire_at: Utc::now(),
    })
    .await;
    let mut schedule = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    schedule.max_fires = Some(1);
    f.adb.upsert_schedule(schedule).await.unwrap();
    f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
    let barrier = Arc::new(crate::extensions::agent_schedule::ScheduleDispatchBarrier::default());
    *f.server.timed_prepared_barrier.lock().unwrap() = Some(barrier.clone());
    let payload = one_shot_dispatch_park(&f).await;
    f.gate.release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        barrier.parked.notified(),
    )
    .await
    .unwrap();
    let agent = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    f.engine.reload_agent(&f.owner_id, &agent).await.unwrap();
    assert_eq!(
        f.engine.current_generation(&f.routine_id).await,
        payload.generation,
        "its own last-slot reservation is not a stale or retired clock"
    );
    f.engine.fire_due_without_rearming_for_test().await;
    assert!(f.mock.recorded_calls().is_empty());
    barrier.release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.gate.finished.notified(),
    )
    .await
    .unwrap();
    f.dispatch.lock().await.take().unwrap().await.unwrap();
    assert_eq!(f.gate.result.lock().unwrap().as_ref(), Some(&Ok(())));
    complete_timed_input(&f, &f.session_db.root_id().to_string()).await;
    assert_eq!(f.mock.recorded_calls().len(), 1);
    assert_eq!(agent.schedule_occurrences().await.unwrap().len(), 1);
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    f.server.shutdown().await;
}

#[tokio::test]
async fn schedule_lifecycle_timer_active_pinned_input_joins_exact_containing_attempt() {
    let f = one_shot_dispatch_fixture(Trigger::OneShot {
        fire_at: Utc::now(),
    })
    .await;
    let mut runtime_agent = f
        .server
        .agents
        .default_agent()
        .expect("fixture runtime Agent");
    runtime_agent.name = "probe-agent".into();
    f.server.agents.upsert(runtime_agent);
    f.mock.push_text("common-human-final");
    let gate = f.mock.block_next_call();
    let mut session = Session::new(
        ConversationId(f.session_db.root_id().to_string()),
        f.session_db.clone(),
    )
    .await;
    let request = session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "ordinary human request".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.wait_started())
        .await
        .unwrap();
    let attempt = f
        .server
        .turns
        .lock()
        .await
        .get(&f.session_db.root_id().to_string())
        .unwrap()
        .attempt
        .clone();
    assert_eq!(attempt.request_id, request);
    assert_eq!(
        f.server
            .turns
            .lock()
            .await
            .get(&f.session_db.root_id().to_string())
            .unwrap()
            .agent_db_id
            .as_deref(),
        Some(f.owner_id.as_str())
    );
    one_shot_dispatch_park(&f).await;
    assert!(release_timed_dispatch(&f).await.is_ok());
    let agent = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    let row = agent.schedule_occurrences().await.unwrap().pop().unwrap();
    let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
    for _ in 0..8 {
        pump.step(&f.server).await;
    }
    let evidence = session
        .schedule_delivery_evidence(&row.reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(evidence.input.attempt_id, attempt.attempt_id);
    assert_eq!(evidence.input.request_id, request);
    assert_eq!(
        session.attempts_for_test().await.len(),
        1,
        "no standalone scheduled attempt"
    );
    assert_eq!(f.mock.recorded_calls().len(), 1);
    gate.release();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while session
            .turn_completion(&attempt.attempt_id)
            .await
            .unwrap()
            .is_none()
            || f.server
                .processing
                .lock()
                .await
                .contains(&f.session_db.root_id().to_string())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let evidence = session
        .schedule_delivery_evidence(&row.reference)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        evidence.input.state,
        crate::session::steering::JobInputState::Included { .. }
    ));
    assert_eq!(
        evidence.completion.unwrap().outcome,
        crate::session::outcome::TurnOutcome::Success
    );
    let calls = f.mock.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert!(!format!("{:?}", calls[0].messages).contains("private-probe-prompt"));
    assert!(format!("{:?}", calls[1].messages).contains("private-probe-prompt"));
    assert!(
        !serde_json::to_string(&session.turn_transcript(&attempt.attempt_id).await.unwrap())
            .unwrap()
            .contains("private-probe-prompt")
    );
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    f.server.shutdown().await;
}

#[tokio::test]
async fn schedule_lifecycle_direct_legacy_payloads_are_inert_without_both_clock_guards() {
    let f = one_shot_dispatch_fixture(Trigger::OneShot {
        fire_at: Utc::now(),
    })
    .await;
    let payload = one_shot_dispatch_park(&f).await;
    let before_agent = f.adb.database().snapshot().await.unwrap();
    let before_session = f.session_db.snapshot().await.unwrap();
    for missing in ["clock", "generation"] {
        let mut invalid = payload.clone();
        if missing == "clock" {
            invalid.clock = None;
        } else {
            invalid.generation = None;
        }
        assert!(f.server.fire_agent_schedule(invalid.clone()).await.is_err());
        assert!(f.server.dispatch_agent_schedule(invalid).await.is_err());
    }
    assert_eq!(f.adb.database().snapshot().await.unwrap(), before_agent);
    assert_eq!(f.session_db.snapshot().await.unwrap(), before_session);
    assert!(f.mock.recorded_calls().is_empty());
    assert!(f.adb.schedule_occurrences().await.unwrap().is_empty());
    assert_eq!(
        one_shot_dispatch_finish(&f).await,
        (1, 1, 1),
        "real clock path is a positive control"
    );
}

#[tokio::test]
async fn scheduled_required_setup_writes_stop_dispatch_and_uncertain_handoff_is_not_replayed() {
    for boundary in [
        "timed-preparation",
        "timed-input",
        "timed-handoff",
        "timed-handoff-ack",
    ] {
        let f = one_shot_dispatch_fixture(Trigger::OneShot {
            fire_at: Utc::now(),
        })
        .await;
        *f.server.schedule_fail_at.lock().unwrap() = Some(boundary);
        let _payload = one_shot_dispatch_park(&f).await;
        let error = release_timed_dispatch(&f).await.unwrap_err();
        assert!(error.contains(boundary), "{error}");
        assert!(f.mock.recorded_calls().is_empty());
        let session = Session::new(
            ConversationId(f.session_db.root_id().to_string()),
            f.session_db.clone(),
        )
        .await;
        assert!(session.attempts_for_test().await.is_empty());
        assert_eq!(
            f.adb.list_schedule_fires().await.unwrap().len(),
            usize::from(boundary == "timed-handoff-ack")
        );
        if boundary == "timed-handoff-ack" {
            // Lost timer ack is still one durable, unstarted handoff. The same
            // reference is discovered once; it is not a new legacy invocation.
            *f.server.schedule_fail_at.lock().unwrap() = None;
            complete_timed_input(&f, &f.session_db.root_id().to_string()).await;
            assert_eq!(f.mock.recorded_calls().len(), 1);
            assert_eq!(f.adb.list_schedule_fires().await.unwrap().len(), 1);
            assert_eq!(session.attempts_for_test().await.len(), 1);
        }
        f.server.shutdown().await;
    }
}

#[tokio::test]
async fn schedule_lifecycle_common_timer_preserves_host_and_home_authority() {
    for change in ["unhosted", "agent-home", "session-home", "legacy-home"] {
        let f = one_shot_dispatch_fixture(Trigger::OneShot {
            fire_at: Utc::now(),
        })
        .await;
        let foreign = f.registry.new_ephemeral_key("foreign-home").await.unwrap();
        if matches!(change, "agent-home" | "legacy-home") {
            let mut definition = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
            definition.target = crate::agent_db::ScheduleTarget::Fresh;
            f.adb.upsert_schedule(definition).await.unwrap();
            if change == "agent-home" {
                crate::db_kind::write_agent_home_pubkey(f.adb.database(), &foreign)
                    .await
                    .unwrap();
            } else {
                crate::db_kind::clear_agent_home_pubkey(f.adb.database())
                    .await
                    .unwrap();
            }
            f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
        } else if change == "session-home" {
            crate::session::update_meta_on_db(&f.session_db, |meta| {
                meta.agents[0].home_pubkey = Some(foreign.to_string());
            })
            .await
            .unwrap();
        }
        let _payload = one_shot_dispatch_park(&f).await;
        if change == "unhosted" {
            f.server.agent_index.unregister(&f.adb.id());
        }
        let result = release_timed_dispatch(&f).await;
        if change == "legacy-home" {
            result.unwrap();
            let row = f.adb.schedule_occurrences().await.unwrap().pop().unwrap();
            let crate::schedule_state::lifecycle::Preparation::HandedOff { input, .. } =
                row.preparation
            else {
                panic!("Fresh handoff missing")
            };
            complete_timed_input(&f, &input.session_db_id).await;
            assert_eq!(f.mock.recorded_calls().len(), 1);
            assert_eq!(f.adb.list_schedule_fires().await.unwrap().len(), 1);
        } else {
            assert!(result.is_err(), "{change} must refuse before admission");
            assert!(f.mock.recorded_calls().is_empty());
            assert!(f.adb.schedule_occurrences().await.unwrap().is_empty());
            assert!(f.adb.list_schedule_fires().await.unwrap().is_empty());
        }
        f.server.shutdown().await;
    }
}

/// Fire once, let the durable successor fall due while "down", restart the
/// clock boundary and fire the real due loop again through the installed
/// handler. Returns (model calls, handoffs, missed windows, payload.missed).
async fn missed_window_restart_case(
    catch_up: bool,
) -> (
    usize,
    usize,
    Vec<crate::schedule_state::lifecycle::clock::MissedWindow>,
    bool,
) {
    let f = one_shot_dispatch_fixture(Trigger::Interval {
        period: std::time::Duration::from_secs(1),
    })
    .await;
    let mut definition = f.adb.find_schedule("probe-wake").await.unwrap().unwrap();
    definition.catch_up = Some(catch_up);
    f.adb.upsert_schedule(definition.clone()).await.unwrap();
    f.engine.reload_agent(&f.owner_id, &f.adb).await.unwrap();
    f.mock.push_text("scheduled-probe-marker");
    let sid = f.session_db.root_id().to_string();
    let first = one_shot_dispatch_park(&f).await;
    assert!(
        !first.clock.as_ref().unwrap().missed,
        "live slot is not missed"
    );
    assert_eq!(release_timed_dispatch(&f).await, Ok(()));
    complete_timed_input(&f, &sid).await;
    assert_eq!(f.mock.recorded_calls().len(), 1);
    let agent = f
        .registry
        .open_scheduling_agent_db(&f.adb.id(), None)
        .await
        .unwrap()
        .unwrap();
    let due = match agent.schedule_clock_cursor(&definition).await.unwrap() {
        Some(crate::agent_db::ClockCursor {
            next: Some(crate::agent_db::DueSlot::Interval { anchor, .. }),
            ..
        }) => anchor + chrono::Duration::seconds(1),
        other => panic!("interval successor missing: {other:?}"),
    };
    // Downtime: the persisted successor becomes due before the restart.
    while Utc::now() <= due + chrono::Duration::milliseconds(200) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    f.engine.simulate_restart_for_test(Utc::now());
    f.engine.reload_agent(&f.owner_id, &agent).await.unwrap();
    let engine = f.engine.clone();
    *f.dispatch.lock().await = Some(tokio::spawn(async move {
        engine.fire_due_without_rearming_for_test().await;
    }));
    tokio::time::timeout(std::time::Duration::from_secs(10), f.gate.parked.notified())
        .await
        .expect("actual handler received the restarted slot");
    let missed = f
        .gate
        .payload
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .clock
        .unwrap()
        .missed;
    assert_eq!(release_timed_dispatch(&f).await, Ok(()));
    let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
    for _ in 0..200 {
        pump.step(&f.server).await;
        let session = Session::new(ConversationId(sid.clone()), f.session_db.clone()).await;
        let markers = session
            .entries()
            .iter()
            .filter(|e| e.content == "scheduled-probe-marker")
            .count();
        if markers == 2 && !f.server.processing.lock().await.contains(&sid) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let calls = f.mock.recorded_calls().len();
    let fires = agent.list_schedule_fires().await.unwrap().len();
    let windows = agent.schedule_missed_windows(&definition).await.unwrap();
    // Either way the clock is re-armed for the next live slot.
    assert!(f.engine.current_generation(&f.routine_id).await.is_some());
    let listing = crate::schedule_state::policy_status(&agent, &definition).await;
    assert!(listing.contains("missed: "), "{listing}");
    assert!(!listing.contains("private-probe-prompt"));
    f.server.shutdown().await;
    (calls, fires, windows, missed)
}

#[tokio::test]
async fn schedule_lifecycle_missed_window_off_records_without_model_call() {
    let (calls, fires, windows, missed) = missed_window_restart_case(false).await;
    assert!(
        missed,
        "persisted slot due before restart is a missed window"
    );
    assert_eq!((calls, fires), (1, 1), "catch-up off makes no call");
    assert_eq!(windows.len(), 1);
    assert!(matches!(
        windows[0].disposition,
        crate::schedule_state::lifecycle::clock::MissedDisposition::Skipped { next: Some(_) }
    ));
}

#[tokio::test]
async fn schedule_lifecycle_missed_window_catch_up_runs_exactly_one_turn() {
    let (calls, fires, windows, missed) = missed_window_restart_case(true).await;
    assert!(missed);
    assert_eq!((calls, fires), (2, 2), "one catch-up turn for the window");
    assert_eq!(windows.len(), 1);
    assert_eq!(
        windows[0].disposition,
        crate::schedule_state::lifecycle::clock::MissedDisposition::CaughtUp
    );
}
