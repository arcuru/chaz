//! Disposable, model-free acceptance checks for scheduled durable execution.
mod common_delivery;
use super::*;
use crate::agent_db::{AgentDb, Schedule, ScheduleTarget};
use crate::routine::Trigger;
use crate::session::{SessionRegistry, TurnAttemptStatus};
use crate::test_support::{MockBackend, MockHost};
use crate::tool::{Tool, ToolDescriptor, ToolError};
use serde_json::{Value, json};
use std::time::Duration;

const PRIVATE: &str = "PRIVATE-WAKE-SENTINEL-NOT-A-SESSION-INPUT";

struct Fixture {
    server: Arc<Server>,
    registry: Arc<SessionRegistry>,
    adb: AgentDb,
    db: eidetica::Database,
    model: Arc<MockBackend>,
}

impl Fixture {
    async fn new(one_shot: bool) -> Self {
        Self::with_hub(one_shot, Arc::new(crate::test_support::context_hub().await)).await
    }

    async fn with_hub(one_shot: bool, hub: Arc<crate::extension::ExtensionHub>) -> Self {
        let model = Arc::new(MockBackend::new());
        let backend =
            BackendManager::with_mock(model.clone(), crate::test_support::empty_secrets().await);
        let (_, server, registry) =
            tests::server_fixture_backend(Arc::new(MockHost::new()), hub, Some(backend)).await;
        let (entry, adb) = tests::seed_agent(&server, &registry, "timer").await;
        let (_, db) = registry
            .create_session(Some("scheduled-fixture"))
            .await
            .unwrap();
        let sid = db.root_id().to_string();
        registry
            .attach_agent_to_session(&sid, &entry)
            .await
            .unwrap();
        server
            .register_session(
                &db,
                server.default_backend.clone(),
                Some("timer".into()),
                None,
            )
            .await
            .unwrap();
        let target = ScheduleTarget::Pinned { session_db_id: sid };
        let trigger = if one_shot {
            Trigger::OneShot {
                fire_at: Utc::now(),
            }
        } else {
            Trigger::Interval {
                period: Duration::from_secs(60),
            }
        };
        let mut schedule = Schedule::new("wake", trigger, PRIVATE, target.clone());
        schedule.max_fires = Some(2);
        adb.upsert_schedule(schedule).await.unwrap();
        Self {
            server,
            registry,
            adb,
            db,
            model,
        }
    }

    async fn view(&self) -> AgentDb {
        self.registry
            .open_scheduling_agent_db(&self.adb.id(), None)
            .await
            .unwrap()
            .unwrap()
    }

    async fn session(&self) -> Session {
        Session::new(
            ConversationId(self.db.root_id().to_string()),
            self.db.clone(),
        )
        .await
    }

    async fn idle(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self
                .server
                .processing
                .lock()
                .await
                .contains(&self.db.root_id().to_string())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

/// This tool verifies record-before-effect at the actual effect boundary.
struct Effect(Arc<AtomicUsize>);
impl Tool for Effect {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "counted_effect".into(),
            description: "offline counted effect".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let session = ctx.session.lock().await;
            let request = ctx.turn_request_id.as_ref().expect("real timer request");
            let attempt = session
                .attempts_for_test()
                .await
                .into_iter()
                .find(|a| &a.request_id == request)
                .unwrap();
            let transcript = session.turn_transcript(&attempt.attempt_id).await.unwrap();
            assert!(transcript.iter().any(|r| matches!(&r.message,
                crate::session::TurnTranscriptMessage::ModelResponse { tool_calls, .. } if tool_calls.iter().any(|c| c.name == "counted_effect"))));
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("effect happened once".into())
        })
    }
}

// Historical wire fixtures only. Production cannot create legacy reservations
// or dispatch a turn without the clock/lifecycle protocol.
async fn legacy_reservation(
    fx: &Fixture,
) -> anyhow::Result<Option<crate::schedule_state::Reservation>> {
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    let agent = fx.registry.ensure_scheduling_agent_db(&entry).await?;
    let _lock = agent.lock_scheduling().await;
    let schedule = agent.find_schedule("wake").await?.unwrap();
    crate::schedule_state::reserve(
        agent.scheduling_database().await?.unwrap(),
        &schedule,
        Utc::now(),
    )
    .await
}
async fn legacy_refund(
    fx: &Fixture,
    reservation: &crate::schedule_state::Reservation,
) -> anyhow::Result<()> {
    let agent = fx.view().await;
    let _lock = agent.lock_scheduling().await;
    crate::schedule_state::refund(agent.scheduling_database().await?.unwrap(), reservation).await
}

#[tokio::test]
async fn scheduled_owned_reference_is_stable_across_home_and_other_executor_changes() {
    let fx = Fixture::new(false).await;
    let Some(_) = legacy_reservation(&fx).await.unwrap() else {
        panic!("first reservation refused");
    };
    let selected = fx.adb.scheduling_reference().await.unwrap().unwrap();
    let db = fx
        .view()
        .await
        .scheduling_database()
        .await
        .unwrap()
        .unwrap()
        .clone();
    assert_eq!(selected.db_id, db.root_id().to_string());
    assert!(
        fx.registry
            .user_for_tests()
            .await
            .databases()
            .await
            .unwrap()
            .iter()
            .any(|r| r.database_id.to_string() == selected.db_id),
        "ledger is an account-owned DB"
    );
    assert!(
        crate::executor_db::list_jobs(&db).await.unwrap().is_empty(),
        "not a job enrollment"
    );
    let (other, _) = tests::seed_agent(&fx.server, &fx.registry, "other").await;
    let (other_db, _) = fx
        .registry
        .ensure_executor_db(&other.db_id, &other.pubkey)
        .await
        .unwrap();
    fx.adb
        .register_executor(crate::agent_db::ExecutorRef {
            peer_pubkey: "different-peer".into(),
            db_id: other_db.root_id().to_string(),
        })
        .await
        .unwrap();
    let home = fx.registry.new_ephemeral_key("new-home").await.unwrap();
    crate::db_kind::write_agent_home_pubkey(fx.adb.database(), &home)
        .await
        .unwrap();
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    let reopened = fx
        .registry
        .ensure_scheduling_agent_db(&entry)
        .await
        .unwrap();
    assert_eq!(
        reopened.scheduling_reference().await.unwrap().unwrap(),
        selected
    );
    assert_eq!(
        reopened.effective_schedules().await.unwrap()[0].fire_count,
        1
    );
    assert!(legacy_reservation(&fx).await.unwrap().is_some());
    assert!(legacy_reservation(&fx).await.unwrap().is_none());
    assert_eq!(
        fx.view().await.effective_schedules().await.unwrap()[0].fire_count,
        2
    );
}

#[tokio::test]
async fn scheduled_unavailable_wrong_and_conflicting_references_never_reset_state() {
    for invalid in ["malformed", "wrong-kind", "conflicting"] {
        let fx = Box::pin(Fixture::new(false)).await;
        // Even the first read is side-effect-free when no reference exists.
        let before = fx.adb.database().snapshot().await.unwrap();
        assert_eq!(
            fx.view().await.effective_schedules().await.unwrap().len(),
            1
        );
        assert_eq!(fx.adb.database().snapshot().await.unwrap(), before);
        let bad = crate::agent_db::ExecutorRef {
            peer_pubkey: "peer".into(),
            db_id: if invalid == "malformed" {
                "not-a-db-id".into()
            } else {
                fx.db.root_id().to_string()
            },
        };
        fx.adb.pin_scheduling_reference(bad.clone()).await.unwrap();
        if invalid == "conflicting" {
            let txn = fx.adb.database().new_transaction().await.unwrap();
            let refs = txn
                .get_store::<eidetica::store::Table<crate::agent_db::ExecutorRef>>(
                    "scheduling_executor",
                )
                .await
                .unwrap();
            refs.insert(bad.clone()).await.unwrap(); // duplicate-identical is benign
            txn.commit().await.unwrap();
            assert_eq!(
                fx.adb.scheduling_reference().await.unwrap(),
                Some(bad.clone())
            );
            let txn = fx.adb.database().new_transaction().await.unwrap();
            txn.get_store::<eidetica::store::Table<crate::agent_db::ExecutorRef>>(
                "scheduling_executor",
            )
            .await
            .unwrap()
            .insert(crate::agent_db::ExecutorRef {
                db_id: fx.adb.id().to_string(),
                ..bad
            })
            .await
            .unwrap();
            txn.commit().await.unwrap();
        }
        let count = fx
            .registry
            .user_for_tests()
            .await
            .databases()
            .await
            .unwrap()
            .len();
        assert!(
            fx.registry
                .open_scheduling_agent_db(&fx.adb.id(), None)
                .await
                .is_err()
        );
        let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
        assert!(
            fx.registry
                .ensure_scheduling_agent_db(&entry)
                .await
                .is_err()
        );
        assert_eq!(
            fx.registry
                .user_for_tests()
                .await
                .databases()
                .await
                .unwrap()
                .len(),
            count,
            "no empty replacement ledger"
        );
        assert!(fx.model.recorded_calls().is_empty());
    }
}

#[tokio::test]
async fn scheduled_legacy_baseline_and_exact_refunds_do_not_mutate_replacements() {
    let fx = Fixture::new(false).await;
    let mut legacy: Schedule = serde_json::from_value(json!({
        "id":"wake", "trigger":{"kind":"cron", "expr":"0 0 * * * *"},
        "prompt":PRIVATE, "target":{"kind":"fresh"}, "enabled":true,
        "fire_count":1, "max_fires":2, "consecutive_failures":2, "last_error":"legacy failure"
    }))
    .unwrap();
    assert_eq!(legacy.lifetime_key(), "legacy:wake");
    fx.adb.upsert_schedule(legacy.clone()).await.unwrap();
    let old_fire: crate::agent_db::ScheduleFire = serde_json::from_value(json!({
        "schedule_id":"wake", "fired_at":"2026-01-01T00:00:00Z", "session_db_id":"legacy-session", "fresh":true
    })).unwrap();
    assert!(old_fire.attempt_id.is_none());
    fx.adb.record_schedule_fire(old_fire).await.unwrap();
    let Some(reservation) = legacy_reservation(&fx).await.unwrap() else {
        panic!("legacy final slot refused");
    };
    let db = fx
        .view()
        .await
        .scheduling_database()
        .await
        .unwrap()
        .unwrap()
        .clone();
    assert_eq!(
        fx.view().await.effective_schedules().await.unwrap()[0].fire_count,
        2
    );
    assert_eq!(
        fx.adb.find_schedule("wake").await.unwrap().unwrap(),
        legacy,
        "baseline remains frozen on Agent"
    );
    legacy.enabled = false;
    fx.adb.upsert_schedule(legacy).await.unwrap();
    legacy_refund(&fx, &reservation).await.unwrap();
    assert!(
        !fx.view().await.effective_schedules().await.unwrap()[0].enabled,
        "refund never re-enables operator disable"
    );
    fx.adb.remove_schedule("wake").await.unwrap();
    let replacement = Schedule::new(
        "wake",
        Trigger::Interval {
            period: Duration::from_secs(60),
        },
        PRIVATE,
        ScheduleTarget::Fresh,
    );
    assert_ne!(replacement.lifetime_key(), reservation.lifetime);
    fx.adb.upsert_schedule(replacement.clone()).await.unwrap();
    let Some(new) = legacy_reservation(&fx).await.unwrap() else {
        panic!("replacement refused");
    };
    legacy_refund(&fx, &reservation).await.unwrap();
    assert_eq!(
        crate::schedule_state::state(&db, &replacement)
            .await
            .unwrap()
            .fire_count,
        1
    );
    legacy_refund(&fx, &new).await.unwrap();
    legacy_refund(&fx, &new).await.unwrap();
    assert_eq!(
        crate::schedule_state::state(&db, &replacement)
            .await
            .unwrap()
            .fire_count,
        0
    );
    assert_eq!(
        fx.view().await.list_schedule_fires().await.unwrap()[0].session_db_id,
        "legacy-session"
    );
    let wire = serde_json::to_value(replacement).unwrap();
    assert!(wire.get("fire_count").is_none());
    assert!(wire.get("consecutive_failures").is_none());
}

#[tokio::test]
async fn scheduled_tools_edit_raw_definitions_and_recreate_consumed_lifetimes() {
    let fx = Fixture::new(false).await;
    fx.model.push_text("first dispatch");
    let (reference, sid) = common_delivery::prepared(&fx, false, true).await;
    fx.server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    fx.idle().await;
    let cap = Arc::new(crate::extension::agent_state::ScopedAgentStateAdmin::new(
        fx.registry.clone(),
        fx.server.agent_index.clone(),
        Some(vec!["timer".into()]),
    ));
    let mut ctx = crate::test_support::tool_context_with_host(
        Arc::new(Mutex::new(fx.session().await)),
        fx.server.tools.clone(),
        Arc::new(MockHost::new()),
    );
    ctx.agent_name = "timer".into();
    let engine = crate::routine::RoutineEngine::new(fx.registry.chaz_peer().clone(), None)
        .await
        .unwrap();
    engine
        .register_agent(&fx.adb.id().to_string(), &fx.view().await)
        .await
        .unwrap();
    ctx.routine_engine = Some(engine.clone());
    crate::tools::ScheduleModify::new(cap.clone())
        .execute(json!({"id":"wake", "max_fires":3}), &ctx)
        .await
        .unwrap();
    let raw = fx.adb.find_schedule("wake").await.unwrap().unwrap();
    assert_eq!(
        raw.fire_count, 0,
        "editing must not write the projected count into the Agent"
    );
    assert_eq!(
        fx.view()
            .await
            .effective_schedule("wake")
            .await
            .unwrap()
            .unwrap()
            .fire_count,
        1
    );
    assert_eq!(engine.list_routines().await.len(), 1);
    // Give the definition a new one-shot lifetime, then consume it normally.
    let one_shot = Schedule::new(
        "wake",
        Trigger::OneShot {
            fire_at: Utc::now(),
        },
        PRIVATE,
        raw.target,
    );
    fx.adb.upsert_schedule(one_shot.clone()).await.unwrap();
    fx.model.push_text("one-shot");
    let (reference, sid) = common_delivery::prepared(&fx, false, true).await;
    fx.server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    fx.idle().await;
    assert!(
        fx.view()
            .await
            .effective_schedule("wake")
            .await
            .unwrap()
            .is_none()
    );
    let before = fx.adb.find_schedule("wake").await.unwrap().unwrap();
    assert!(
        crate::tools::ScheduleModify::new(cap.clone())
            .execute(json!({"id":"wake", "interval_seconds":60}), &ctx)
            .await
            .is_err(),
        "consumed one-shots must be recreated, not silently edited while hidden"
    );
    assert_eq!(fx.adb.find_schedule("wake").await.unwrap().unwrap(), before);
    crate::tools::ScheduleAdd::new(cap)
        .execute(
            json!({
                "id":"wake", "interval_seconds":60, "task":PRIVATE, "target":"pinned"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let recreated = fx.adb.find_schedule("wake").await.unwrap().unwrap();
    assert_ne!(recreated.lifetime, one_shot.lifetime);
    assert_eq!(
        fx.view()
            .await
            .effective_schedule("wake")
            .await
            .unwrap()
            .unwrap()
            .fire_count,
        0
    );
}

#[tokio::test]
async fn scheduled_accounting_processing_loop_progresses_without_capacity_or_timer_dispatch() {
    let fx = Fixture::new(false).await;
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    let agent = fx
        .registry
        .ensure_scheduling_agent_db(&entry)
        .await
        .unwrap();
    let mut schedule = agent.find_schedule("wake").await.unwrap().unwrap();
    schedule.max_fires = Some(80);
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let revision = schedule.revision_key().unwrap();
    let now = Utc::now();
    let slot = |ordinal| crate::agent_db::DueSlot::Interval {
        anchor: now - chrono::Duration::minutes(1),
        ordinal,
    };
    while !agent
        .reconcile_schedule_accounting(&schedule, 32)
        .await
        .unwrap()
        .complete
    {}
    for ordinal in 0..65 {
        agent
            .prepare_schedule_occurrence("wake", &revision, slot(ordinal), now)
            .await
            .unwrap();
    }
    *agent.lock_scheduling().await = crate::schedule_state::lifecycle::AccountingCache::default();
    let operations = agent.scheduling_database().await.unwrap().unwrap();
    let before = operations.snapshot().await.unwrap();
    let _all_capacity = fx
        .server
        .semaphore
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
        .await
        .unwrap();
    let engine = crate::routine::RoutineEngine::new(fx.registry.chaz_peer().clone(), None)
        .await
        .unwrap();
    fx.server.set_routine_engine(engine);
    // The actual ordinary processing loop drives bounded pages. No engine.run,
    // model call, target-idleness or execution capacity is needed for observation.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match agent
                .prepare_schedule_occurrence("wake", &revision, slot(66), now)
                .await
            {
                Ok(_) => break,
                Err(error) => {
                    assert!(
                        error.to_string().contains("reconciliation required"),
                        "{error:#}"
                    );
                    assert_eq!(before, operations.snapshot().await.unwrap());
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    })
    .await
    .expect("resident processing loop must finish read-only accounting");
    assert_eq!(agent.schedule_occurrences().await.unwrap().len(), 66);
    assert!(agent.list_schedule_fires().await.unwrap().is_empty());
    assert!(fx.model.recorded_calls().is_empty());
    assert!(
        fx.session()
            .await
            .turn_requests(|_| false, &Default::default())
            .await
            .unwrap()
            .is_empty()
    );
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_accounting_discovery_initializes_only_authorized_timer_storage() {
    let fx = Fixture::new(false).await;
    let engine = crate::routine::RoutineEngine::new(fx.registry.chaz_peer().clone(), None)
        .await
        .unwrap();
    fx.server.set_routine_engine(engine);
    fx.server
        .deregister_session(&fx.db.root_id().to_string())
        .await;
    let before = fx.adb.database().snapshot().await.unwrap();
    let mut pump = super::schedule::AccountingPump::default();
    for _ in 0..10 {
        pump.step(&fx.server).await;
    }
    assert!(fx.adb.scheduling_reference().await.unwrap().is_none());
    assert_eq!(before, fx.adb.database().snapshot().await.unwrap());
    fx.server
        .register_session(
            &fx.db,
            fx.server.default_backend.clone(),
            Some("timer".into()),
            None,
        )
        .await
        .unwrap();
    for _ in 0..10 {
        pump.step(&fx.server).await;
    }
    let selected = fx.adb.scheduling_reference().await.unwrap().unwrap();
    for _ in 0..10 {
        pump.step(&fx.server).await;
    }
    assert_eq!(fx.adb.scheduling_reference().await.unwrap(), Some(selected));
    assert!(fx.model.recorded_calls().is_empty());
    assert!(fx.session().await.attempts_for_test().await.is_empty());
    fx.server.shutdown().await;
}

impl Fixture {
    async fn prepared_fresh(&self) -> (AgentDb, crate::agent_db::Occurrence) {
        let entry = self.server.agent_index.find_by_id(&self.adb.id()).unwrap();
        let agent = self
            .registry
            .ensure_scheduling_agent_db(&entry)
            .await
            .unwrap();
        let mut schedule = agent.find_schedule("wake").await.unwrap().unwrap();
        schedule.target = ScheduleTarget::Fresh;
        agent.upsert_schedule(schedule.clone()).await.unwrap();
        while !agent
            .reconcile_schedule_accounting(&schedule, 32)
            .await
            .unwrap()
            .complete
        {}
        let now = Utc::now();
        let occurrence = agent
            .prepare_schedule_occurrence(
                "wake",
                &schedule.revision_key().unwrap(),
                crate::agent_db::DueSlot::Interval {
                    anchor: now - chrono::Duration::minutes(1),
                    ordinal: 1,
                },
                now,
            )
            .await
            .unwrap();
        (agent, occurrence)
    }
}

#[tokio::test]
async fn scheduled_fresh_root_reconciles_creation_without_a_second_session_or_turn() {
    use crate::schedule_state::lifecycle::{FreshRoot, fresh};
    let fx = Fixture::new(false).await;
    let (agent, occurrence) = fx.prepared_fresh().await;
    let reference = &occurrence.reference;
    assert!(
        fresh::request(&agent, reference, Utc::now())
            .await
            .unwrap()
            .1
    );
    let source = fresh::source(reference).unwrap();
    // Interruption after native creation, before operational binding: lose the
    // returned ID. Even mutable display-name edits must not hide that root.
    let (_, created) = fx.registry.create_session(Some(&source)).await.unwrap();
    let txn = created.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_name("renamed after creation")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    // A different DB renamed to look like the marker is not a creation match.
    let (_, impostor) = fx
        .registry
        .create_session(Some("not-scheduled"))
        .await
        .unwrap();
    let txn = impostor.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_name(&format!("session:{source}"))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let (left, right) = tokio::join!(
        fx.server.prepare_fresh_schedule_root(reference),
        fx.server.prepare_fresh_schedule_root(reference)
    );
    assert_eq!(left.unwrap().1.root_id(), created.root_id());
    assert_eq!(right.unwrap().1.root_id(), created.root_id());
    assert_eq!(
        fx.registry.find_sessions_by_source(&source).await.unwrap(),
        vec![created.root_id().to_string()]
    );
    let row = agent.schedule_occurrences().await.unwrap().pop().unwrap();
    assert_eq!(
        row.fresh_root,
        Some(FreshRoot::Bound {
            session_db_id: created.root_id().to_string()
        })
    );
    assert_eq!(row.preparation, crate::agent_db::Preparation::Prepared);
    let mut session = Session::try_new(
        ConversationId(created.root_id().to_string()),
        created.clone(),
    )
    .await
    .unwrap();
    assert!(session.attempts_for_test().await.is_empty());
    assert!(agent.list_schedule_fires().await.unwrap().is_empty());
    assert!(fx.model.recorded_calls().is_empty());
    assert!(
        !fx.server
            .is_session_open(&created.root_id().to_string())
            .await
    );
    // Only a real inert Ack can finish handoff, still without Started/success.
    session.prepare_schedule_input(reference).await.unwrap();
    // Attachment advances the inherited identity boundary. Reopen the selected
    // Agent signer before the next operational write, as admission must do.
    let current_agent = fx.view().await;
    current_agent
        .handoff_schedule_occurrence(reference, &session, Utc::now())
        .await
        .unwrap();
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    assert!(session.attempts_for_test().await.is_empty());
    let error = fresh::bind(
        &agent,
        reference,
        &impostor.root_id().to_string(),
        Utc::now(),
    )
    .await
    .unwrap_err();
    assert!(!error.to_string().is_empty());
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_fresh_unknown_creation_and_duplicate_markers_hold_without_replacement() {
    use crate::schedule_state::lifecycle::fresh;
    let fx = Fixture::new(false).await;
    let (agent, row) = fx.prepared_fresh().await;
    let source = fresh::source(&row.reference).unwrap();
    fresh::request(&agent, &row.reference, Utc::now())
        .await
        .unwrap();
    let before = agent
        .scheduling_database()
        .await
        .unwrap()
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    let error = fx
        .server
        .prepare_fresh_schedule_root(&row.reference)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown"), "{error:#}");
    assert!(
        fx.registry
            .find_sessions_by_source(&source)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        before,
        agent
            .scheduling_database()
            .await
            .unwrap()
            .unwrap()
            .snapshot()
            .await
            .unwrap()
    );
    // Two actual original markers are ambiguity, never first-match selection.
    fx.registry.create_session(Some(&source)).await.unwrap();
    fx.registry.create_session(Some(&source)).await.unwrap();
    let error = fx
        .server
        .prepare_fresh_schedule_root(&row.reference)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("multiple Fresh roots"),
        "{error:#}"
    );
    assert_eq!(
        fx.registry
            .find_sessions_by_source(&source)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        before,
        agent
            .scheduling_database()
            .await
            .unwrap()
            .unwrap()
            .snapshot()
            .await
            .unwrap()
    );
    assert!(fx.model.recorded_calls().is_empty());
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_fresh_new_root_is_bound_once_and_revocation_prevents_provisioning() {
    use crate::schedule_state::lifecycle::{FreshRoot, fresh};
    let fx = Fixture::new(false).await;
    let (agent, row) = fx.prepared_fresh().await;
    let (_, root) = fx
        .server
        .prepare_fresh_schedule_root(&row.reference)
        .await
        .unwrap();
    let (_, again) = fx
        .server
        .prepare_fresh_schedule_root(&row.reference)
        .await
        .unwrap();
    assert_eq!(root.root_id(), again.root_id());
    assert_eq!(
        agent.schedule_occurrences().await.unwrap()[0].fresh_root,
        Some(FreshRoot::Bound {
            session_db_id: root.root_id().to_string()
        })
    );
    assert!(
        fresh::bind(
            &agent,
            &row.reference,
            &fx.db.root_id().to_string(),
            Utc::now()
        )
        .await
        .is_err()
    );
    let mut definition = agent.find_schedule("wake").await.unwrap().unwrap();
    definition.enabled = false;
    agent.upsert_schedule(definition).await.unwrap();
    assert!(
        fx.server
            .prepare_fresh_schedule_root(&row.reference)
            .await
            .is_err()
    );
    assert_eq!(
        fx.registry
            .find_sessions_by_source(&fresh::source(&row.reference).unwrap())
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(fx.model.recorded_calls().is_empty());
    fx.server.shutdown().await;
}

mod common_input;
