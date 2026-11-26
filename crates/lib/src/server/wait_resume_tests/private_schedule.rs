//! Kill/relaunch the real libtest process, retaining only the native service and
//! disposable files. MockBackend is the only model transport in either process.
use super::*;
use crate::agent_db::{Schedule, ScheduleReference, ScheduleTarget};
use crate::routine::{RoutineEngine, RoutineId, Trigger};
use crate::session::steering::JobInputState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const PRIVATE: &str = "PRIVATE-PROCESS-WAIT-SCHEDULE";
const WORKER: &str = "CHAZ_TEST_PRIVATE_WAIT_WORKER";
const TEST: &str = "server::tests::wait_resume::private_schedule::scheduled_private_wait_survives_process_death_without_effect_replay";

#[derive(Serialize, Deserialize)]
struct Setup {
    connection: String,
    username: String,
    agent: DbEntry,
    phase: String,
    fault: String,
}

#[derive(Serialize, Deserialize)]
struct Evidence {
    sid: String,
    child: String,
    reference: ScheduleReference,
    attempt: String,
    checkpoint: Option<JobWaitCheckpoint>,
}

struct FileEffect {
    path: PathBuf,
    session: Arc<std::sync::Mutex<Option<std::sync::Weak<Mutex<Session>>>>>,
}
impl crate::tool::Tool for FileEffect {
    fn descriptor(&self) -> crate::tool::ToolDescriptor {
        crate::tool::ToolDescriptor {
            name: "effect".into(),
            description: "Count a disposable external effect".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }
    fn execute<'a>(
        &'a self,
        _: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>,
    > {
        Box::pin(async move {
            *self.session.lock().unwrap() = Some(Arc::downgrade(&ctx.session));
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .unwrap(),
                "effect"
            )
            .unwrap();
            Ok("effect ran".into())
        })
    }
}

struct ChildProcess(std::process::Child);
impl Drop for ChildProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn launch(dir: &Path, setup: &Setup) -> ChildProcess {
    std::fs::write(dir.join("setup.json"), serde_json::to_vec(setup).unwrap()).unwrap();
    let log = std::fs::File::create(dir.join(format!("{}.log", setup.phase))).unwrap();
    ChildProcess(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(WORKER, dir)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    )
}

async fn marker(dir: &Path, child: &mut ChildProcess, name: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !dir.join(name).exists() {
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!(
                    "worker exited {status} before {name}: {}{}",
                    std::fs::read_to_string(dir.join("first.log")).unwrap_or_default(),
                    std::fs::read_to_string(dir.join("resume.log")).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "worker did not reach {name}: {}{}",
            std::fs::read_to_string(dir.join("first.log")).unwrap_or_default(),
            std::fs::read_to_string(dir.join("resume.log")).unwrap_or_default()
        )
    });
}

async fn worker(dir: &Path) {
    let setup: Setup =
        serde_json::from_slice(&std::fs::read(dir.join("setup.json")).unwrap()).unwrap();
    let settings = crate::config::EideticaConfig {
        connection: setup.connection.clone(),
        login: crate::config::EideticaLoginConfig {
            username: setup.username.clone(),
            password: Some("test-password".into()),
        },
        sync: None,
    };
    let slot = crate::instance::ServerSlot::default();
    let mut hub = crate::test_support::context_hub().await;
    hub.install_all(vec![Arc::new(
        crate::extensions::agent_schedule::AgentScheduleExtension::new(slot.clone()),
    )])
    .await
    .unwrap();
    let hub = Arc::new(hub);
    let mock = Arc::new(MockBackend::new());
    let (server, _) = published_executor_using_mock_and_hub(
        &settings,
        Arc::new(AgentRegistry::with_default_agent()),
        setup.agent.clone(),
        mock.clone(),
        hub.clone(),
        false,
    )
    .await;
    slot.set(server.clone());
    assert!(
        server.default_backend.list_known_backends().is_empty(),
        "fixture has no real model clients"
    );
    server.tools.register(crate::tools::JobWaitTool {
        server: slot.clone(),
    });
    let effect_session = Arc::new(std::sync::Mutex::new(None));
    server.tools.register(FileEffect {
        path: dir.join("effects"),
        session: effect_session.clone(),
    });
    if setup.phase == "first" {
        let registry = server.registry.clone();
        let (sid, db) = registry
            .create_session(Some("private-process-wait"))
            .await
            .unwrap();
        registry
            .attach_agent_to_session(&sid.0, &setup.agent)
            .await
            .unwrap();
        let prepared = registry
            .prepare_agent_job(&sid.0, "default", "unpublished child")
            .await
            .unwrap();
        let child = prepared.db.root_id().to_string();
        let gate = mock.block_next_call();
        mock.push_tool_calls([("effect-call".into(), "effect".into(), "{}".into())]);
        mock.push_tool_calls([wait_call(&child, 240)]);
        server
            .register_session(&db, server.default_backend.clone(), None, None)
            .await
            .unwrap();
        let mut session = Session::new(sid.clone(), db.clone()).await;
        session
            .add_entry(SessionEntry {
                sender: "human".into(),
                content: "ordinary containing turn".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Directive,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(ASYNC_TEST_TIMEOUT, gate.wait_started())
            .await
            .unwrap();
        let attempt = Session::steering_attempts(&db)
            .await
            .unwrap()
            .pop()
            .unwrap();
        session
            .submit_turn_input(crate::session::steering::JobInputRequest::new(
                attempt.clone(),
                "ordinary steering before private input".into(),
            ))
            .await
            .unwrap();
        let agent = registry
            .open_agent_db(&setup.agent.db_id, Some(&setup.agent.pubkey))
            .await
            .unwrap()
            .unwrap();
        let mut definition = Schedule::new(
            "process-wake",
            Trigger::OneShot {
                fire_at: Utc::now(),
            },
            PRIVATE,
            ScheduleTarget::Pinned {
                session_db_id: sid.0.clone(),
            },
        );
        definition.max_fires = Some(1);
        agent.upsert_schedule(definition).await.unwrap();
        let agent = registry
            .ensure_scheduling_agent_db(&setup.agent)
            .await
            .unwrap();
        let definition = agent.find_schedule("process-wake").await.unwrap().unwrap();
        for _ in 0..100 {
            if agent
                .reconcile_schedule_accounting(&definition, 32)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
        let engine = RoutineEngine::new(registry.chaz_peer().clone(), Some(hub))
            .await
            .unwrap();
        engine
            .register_agent(&setup.agent.db_id.to_string(), &agent)
            .await
            .unwrap();
        server.set_routine_engine(engine.clone());
        let routine_id = RoutineId::new(format!("agent:{}:process-wake", setup.agent.db_id));
        // Production polls timer admission in its own Tokio task. Don't nest
        // the large debug service futures under the libtest scenario's stack.
        let clock = engine.clone();
        let due = routine_id.clone();
        tokio::spawn(async move { clock.fire_due_for_test(&due).await })
            .await
            .unwrap();
        let timer_result = engine.get(&routine_id).await;
        let reference = agent
            .schedule_occurrences()
            .await
            .unwrap()
            .pop()
            .unwrap_or_else(|| panic!("timer produced no occurrence: {timer_result:?}"))
            .reference;
        tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
            while session
                .schedule_delivery_evidence(&reference)
                .await
                .unwrap()
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let second_gate = (setup.fault == "unpaired").then(|| mock.block_next_call());
        gate.release();
        let checkpoint = if let Some(gate) = second_gate {
            tokio::time::timeout(ASYNC_TEST_TIMEOUT, gate.wait_started())
                .await
                .unwrap();
            assert!(checkpoints(&db).await.is_empty());
            assert!(matches!(
                session
                    .schedule_delivery_evidence(&reference)
                    .await
                    .unwrap()
                    .unwrap()
                    .input
                    .state,
                JobInputState::Dispatching { .. }
            ));
            None
        } else {
            Some(
                tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
                    loop {
                        if let Some(checkpoint) = checkpoints(&db).await.pop() {
                            break checkpoint;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap(),
            )
        };
        assert_eq!(mock.recorded_calls().len(), 2);
        assert_eq!(
            std::fs::read_to_string(dir.join("effects")).unwrap(),
            "effect\n"
        );
        let calls = mock.recorded_calls();
        let messages = &calls[1].messages;
        let ordinary = messages.iter().position(|m| matches!(m, runtime::RuntimeMessage::User(text) if text == "ordinary steering before private input")).unwrap();
        let private = messages
            .iter()
            .position(|m| matches!(m, runtime::RuntimeMessage::System(text) if text == PRIVATE))
            .unwrap();
        assert!(ordinary < private);
        let evidence = Evidence {
            sid: sid.0.clone(),
            child,
            reference,
            attempt,
            checkpoint,
        };
        std::fs::write(
            dir.join("first-ready"),
            serde_json::to_vec(&evidence).unwrap(),
        )
        .unwrap();
        if setup.fault == "in-process" {
            let checkpoint = evidence.checkpoint.unwrap();
            tokio::time::timeout(Duration::from_secs(15), async {
                while !server.suspended_waits.lock().await.contains_key(&sid.0) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            await_pending_reconciliation(&server).await;
            assert!(
                effect_session
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .is_none()
            );
            assert_eq!(
                server.semaphore.available_permits(),
                MAX_CONCURRENT_LLM_CALLS
            );
            // Pause processing-loop accounting, not the unloaded observer.
            // Lose only its disposable certificate, never durable evidence.
            server.mark_startup_pending();
            *agent.lock_scheduling().await = Default::default();
            assert!(
                server
                    .resumable_scheduled_wait(&session, &checkpoint.request_id, checkpoint.clone())
                    .await
                    .unwrap_err()
                    .is::<crate::schedule_state::lifecycle::ReconciliationRequired>()
            );
            server
                .suspended_wait_reconciliations
                .store(0, Ordering::SeqCst);
            crate::session::jobs::reject_job(&prepared.db, "child finished during cold accounting")
                .await
                .unwrap();
            await_pending_reconciliation(&server).await;
            assert!(server.suspended_waits.lock().await.contains_key(&sid.0));
            assert!(server.processing.lock().await.contains(&sid.0));
            assert!(
                effect_session
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .is_none()
            );
            assert_eq!(mock.recorded_calls().len(), 2);
            assert_eq!(
                server.semaphore.available_permits(),
                MAX_CONCURRENT_LLM_CALLS
            );
            assert!(
                session
                    .turn_completion(&evidence.attempt)
                    .await
                    .unwrap()
                    .is_none()
            );
            mock.push_text("same in-process attempt resumed privately");
            server.mark_startup_ready();
            let continued = tokio::time::timeout(Duration::from_secs(15), async {
                while mock.recorded_calls().len() < 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            continued.expect("the same unloaded attempt must continue after accounting is ready");
            await_no_processing(&server, &sid.0).await;
            assert_eq!(mock.recorded_calls().len(), 3);
            let calls = mock.recorded_calls();
            assert!(wait_result(&calls[2]).contains("Rejected"));
            assert!(
                calls[2]
                    .messages
                    .iter()
                    .any(|m| matches!(m, runtime::RuntimeMessage::System(text) if text == PRIVATE))
            );
            assert_eq!(session.attempts_for_test().await.len(), 1);
            assert_eq!(checkpoints(&db).await, [finished_checkpoint(checkpoint)]);
            assert_eq!(
                std::fs::read_to_string(dir.join("effects")).unwrap(),
                "effect\n"
            );
            assert_eq!(agent.schedule_occurrences().await.unwrap().len(), 1);
            assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
            server.shutdown().await;
        } else {
            std::future::pending::<()>().await; // parent kills this process, without executor cleanup
        }
    } else {
        let evidence: Evidence =
            serde_json::from_slice(&std::fs::read(dir.join("first-ready")).unwrap()).unwrap();
        mock.push_text("same attempt resumed privately");
        if setup.fault == "ok" {
            mock.push_text("queued independent turn");
        }
        let engine = RoutineEngine::new(server.registry.chaz_peer().clone(), Some(hub))
            .await
            .unwrap();
        server.set_routine_engine(engine);
        server
            .adopt_indexed_local_client_sessions(&mut Default::default())
            .await;
        let (_, db) = server.registry.open_session(&evidence.sid).await.unwrap();
        if setup.fault == "ok" {
            await_calls(&mock, 2).await;
            await_no_processing(&server, &evidence.sid).await;
            assert_eq!(mock.recorded_calls().len(), 2);
            let call = &mock.recorded_calls()[0];
            assert!(wait_result(call).contains("Rejected"));
            let ordinary = call.messages.iter().position(|m| matches!(m, runtime::RuntimeMessage::User(text) if text == "ordinary steering before private input")).unwrap();
            let private = call
                .messages
                .iter()
                .position(|m| matches!(m, runtime::RuntimeMessage::System(text) if text == PRIVATE))
                .unwrap();
            assert!(ordinary < private);
            let session = Session::new(ConversationId(evidence.sid.clone()), db.clone()).await;
            assert_eq!(session.attempts_for_test().await.len(), 2);
            assert!(
                session
                    .turn_completion(&evidence.attempt)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                session
                    .schedule_delivery_evidence(&evidence.reference)
                    .await
                    .unwrap()
                    .unwrap()
                    .input
                    .attempt_id,
                evidence.attempt
            );
            assert_eq!(
                checkpoints(&db).await,
                [finished_checkpoint(evidence.checkpoint.unwrap())]
            );
            assert!(
                !format!(
                    "{:?}",
                    session.turn_transcript(&evidence.attempt).await.unwrap()
                )
                .contains(PRIVATE)
            );
            assert!(!format!("{:?}", session.entries()).contains(PRIVATE));
            // The later turn was already queued at restart. It must wait for
            // the exact resumed call, and must not inherit its private text.
            assert!(
                !mock.recorded_calls()[1]
                    .messages
                    .iter()
                    .any(|m| matches!(m, runtime::RuntimeMessage::System(text) if text == PRIVATE))
            );
        } else {
            // Positive processing opportunities, not just an empty provider log.
            server
                .register_session(&db, server.default_backend.clone(), None, None)
                .await
                .unwrap();
            for _ in 0..3 {
                server.process_session(&evidence.sid).await.unwrap();
                await_no_processing(&server, &evidence.sid).await;
            }
            let session = Session::new(ConversationId(evidence.sid.clone()), db).await;
            assert_eq!(session.attempts_for_test().await.len(), 1);
            assert!(
                session
                    .turn_completion(&evidence.attempt)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                mock.recorded_calls().is_empty(),
                "uncertain or unauthorized work must remain held"
            );
        }
        let ledger = server
            .registry
            .open_scheduling_agent_db(&setup.agent.db_id, Some(&setup.agent.pubkey))
            .await
            .unwrap()
            .unwrap();
        let rows = ledger.schedule_occurrences().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reference, evidence.reference);
        assert!(
            rows[0].clock.is_some(),
            "actual timer handoff, not legacy standalone execution"
        );
        assert_eq!(ledger.list_schedule_fires().await.unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("effects")).unwrap(),
            "effect\n",
            "the earlier effect was not replayed"
        );
        std::fs::write(dir.join("resume-ready"), "verified").unwrap();
        server.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduled_private_wait_survives_process_death_without_effect_replay() {
    if let Some(dir) = std::env::var_os(WORKER) {
        // The child is the executor, not the crash/restart coordinator. Poll
        // it independently rather than embedding both large debug futures in
        // the libtest caller's stack.
        tokio::spawn(async move { worker(Path::new(&dir)).await })
            .await
            .unwrap();
        return;
    }
    for fault in ["ok", "unpaired", "revoked"] {
        let fx = Fixture::new(&format!("private-process-{fault}")).await;
        let dir = fx._dir.path();
        let mut setup = Setup {
            connection: fx.settings.connection.clone(),
            username: fx.settings.login.username.clone(),
            agent: fx.agent.clone(),
            phase: "first".into(),
            fault: fault.into(),
        };
        let mut first = launch(dir, &setup);
        marker(dir, &mut first, "first-ready").await;
        first.0.kill().unwrap();
        let status = first.0.wait().unwrap();
        assert!(!status.success(), "the first executor really died");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                status.signal(),
                Some(9),
                "actual SIGKILL, not graceful shutdown"
            );
        }
        let evidence: Evidence =
            serde_json::from_slice(&std::fs::read(dir.join("first-ready")).unwrap()).unwrap();
        let instance = Instance::connect(&setup.connection).await.unwrap();
        let user = instance
            .login_account(&setup.username, "test-password")
            .await
            .unwrap();
        let db = user
            .open_database(&eidetica::entry::ID::parse(&evidence.sid).unwrap())
            .await
            .unwrap();
        let history = wait_history(&db).await;
        if fault != "unpaired" {
            let child = user
                .open_database(&eidetica::entry::ID::parse(&evidence.child).unwrap())
                .await
                .unwrap();
            crate::session::jobs::reject_job(&child, "completed while executor was down")
                .await
                .unwrap();
        }
        if fault == "ok" {
            // Included input survives removal of the later editable definition,
            // but only for its exact authorized containing attempt.
            let agent = crate::agent_db::AgentDb::from_database(
                user.open_database(&setup.agent.db_id).await.unwrap(),
            );
            agent.remove_schedule("process-wake").await.unwrap();
            let mut session = Session::try_new(ConversationId(evidence.sid.clone()), db.clone())
                .await
                .unwrap();
            session
                .add_entry(SessionEntry {
                    sender: "human".into(),
                    content: "unrelated later question".into(),
                    timestamp: Utc::now(),
                    entry_type: EntryType::Directive,
                    metadata: None,
                    routing: None,
                })
                .await
                .unwrap();
        } else if fault == "revoked" {
            let registry = crate::session::SessionRegistry::new(
                instance.clone(),
                user,
                Arc::new(AgentRegistry::with_default_agent()),
            )
            .await
            .unwrap();
            registry
                .detach_agent_from_session(&evidence.sid, &setup.agent)
                .await
                .unwrap();
        }
        setup.phase = "resume".into();
        let mut resumed = launch(dir, &setup);
        marker(dir, &mut resumed, "resume-ready").await;
        let status = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(status) = resumed.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(dir.join("resume.log")).unwrap()
        );
        let after = wait_history(&db).await;
        assert!(
            history
                .iter()
                .all(|(id, bytes)| after.get(id) == Some(bytes)),
            "original signed attempt/transcript/checkpoint history is preserved"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("effects")).unwrap(),
            "effect\n"
        );
        println!(
            "private scheduled process restart {fault}: SIGKILL observed; effect count 1; signed history retained; exact continuation/hold verified"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduled_unloaded_wait_recovers_cold_accounting_without_effect_replay() {
    let fx = Fixture::new("private-unloaded-accounting").await;
    let setup = Setup {
        connection: fx.settings.connection.clone(),
        username: fx.settings.login.username.clone(),
        agent: fx.agent.clone(),
        phase: "first".into(),
        fault: "in-process".into(),
    };
    std::fs::write(
        fx._dir.path().join("setup.json"),
        serde_json::to_vec(&setup).unwrap(),
    )
    .unwrap();
    worker(fx._dir.path()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduled_service_fresh_root_and_handoff_use_the_selected_owner() {
    let fx = Fixture::new("service-fresh-owner").await;
    let slot = crate::instance::ServerSlot::default();
    let mut hub = crate::test_support::context_hub().await;
    hub.install_all(vec![Arc::new(
        crate::extensions::agent_schedule::AgentScheduleExtension::new(slot.clone()),
    )])
    .await
    .unwrap();
    let hub = Arc::new(hub);
    let mock = Arc::new(MockBackend::new());
    mock.push_text("Fresh private final");
    let process = fx.start_with_hub(mock.clone(), hub.clone()).await;
    slot.set(process.server.clone());
    let registry = &process.server.registry;
    let agent = registry
        .open_agent_db(&fx.agent.db_id, Some(&fx.agent.pubkey))
        .await
        .unwrap()
        .unwrap();
    let mut definition = Schedule::new(
        "fresh",
        Trigger::OneShot {
            fire_at: Utc::now(),
        },
        PRIVATE,
        ScheduleTarget::Fresh,
    );
    definition.max_fires = Some(1);
    agent.upsert_schedule(definition).await.unwrap();
    let agent = registry
        .ensure_scheduling_agent_db(&fx.agent)
        .await
        .unwrap();
    let definition = agent.find_schedule("fresh").await.unwrap().unwrap();
    for _ in 0..100 {
        if agent
            .reconcile_schedule_accounting(&definition, 32)
            .await
            .unwrap()
            .complete
        {
            break;
        }
    }
    let engine = RoutineEngine::new(registry.chaz_peer().clone(), Some(hub))
        .await
        .unwrap();
    engine
        .register_agent(&fx.agent.db_id.to_string(), &agent)
        .await
        .unwrap();
    process.server.set_routine_engine(engine.clone());
    let id = RoutineId::new(format!("agent:{}:fresh", fx.agent.db_id));
    engine.fire_due_for_test(&id).await;
    let row = agent.schedule_occurrences().await.unwrap().pop().unwrap();
    let crate::schedule_state::lifecycle::Preparation::HandedOff { input, .. } = &row.preparation
    else {
        panic!("Fresh timer did not hand off: {:?}", engine.get(&id).await);
    };
    let sid = &input.session_db_id;
    await_calls(&mock, 1).await;
    await_no_processing(&process.server, sid).await;
    let (conv, db) = registry.open_session(sid).await.unwrap();
    let session = Session::new(conv, db).await;
    let evidence = session
        .schedule_delivery_evidence(&row.reference)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        evidence.input.state,
        JobInputState::Included { .. }
    ));
    assert_eq!(
        evidence.completion.unwrap().outcome,
        crate::session::outcome::TurnOutcome::Success
    );
    assert_eq!(evidence.input.request_id, input.request_id);
    assert_eq!(session.attempts_for_test().await.len(), 1);
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    assert_eq!(agent.schedule_occurrences().await.unwrap().len(), 1);
    assert_eq!(
        registry
            .find_sessions_by_source(
                &crate::schedule_state::lifecycle::fresh::source(&row.reference).unwrap()
            )
            .await
            .unwrap(),
        std::slice::from_ref(sid)
    );
    assert_eq!(mock.recorded_calls().len(), 1);
    assert!(mock.recorded_calls()[0].messages.iter().any(
        |message| matches!(message, runtime::RuntimeMessage::System(text) if text == PRIVATE)
    ));
    assert!(!format!("{:?}", session.entries()).contains(PRIVATE));
    process.server.shutdown().await;
}
