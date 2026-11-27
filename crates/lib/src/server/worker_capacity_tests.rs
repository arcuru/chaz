//! Model-free Worker submission/observation tests against a disposable executor.
use super::*;
use crate::test_support::{MockBackend, MockCallGate, tool_context};
use crate::tool::{ToolPolicy, ToolPolicyRegistry};
use std::time::Duration;

async fn worker_fixture() -> (
    Instance,
    Arc<Server>,
    crate::instance::ServerSlot,
    Arc<MockBackend>,
) {
    let (owner, server, registry) = server_fixture().await;
    let (slot, child) = install_worker_fixture(&server, &registry).await;
    (owner, server, slot, child)
}

async fn install_worker_fixture(
    server: &Arc<Server>,
    registry: &crate::session::SessionRegistry,
) -> (crate::instance::ServerSlot, Arc<MockBackend>) {
    let (_, adb) = seed_agent(server, registry, "default").await;
    adb.write_config(&AgentDbConfig {
        workers: vec![crate::agent_db::WorkerDbConfig {
            name: "helper".into(),
            tools: Some(vec![]),
            max_spawn_depth: Some(2),
            ..Default::default()
        }],
        ..Default::default()
    })
    .await
    .unwrap();
    server
        .hydrate_agent_from_db(server.agents.default_agent().unwrap())
        .await;
    let slot = crate::instance::ServerSlot::default();
    slot.set(server.clone());
    let child = Arc::new(MockBackend::new());
    server.tools.register(crate::tools::SpawnWorker {
        server: slot.clone(),
        backend: BackendManager::with_mock(
            child.clone(),
            crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
        ),
        security: crate::test_support::permissive_security(),
    });
    server.tools.register(crate::tools::WorkerWait {
        server: slot.clone(),
    });
    (slot, child)
}

struct Parent {
    task: tokio::task::JoinHandle<Result<runtime::RuntimeOutcome, String>>,
    mock: Arc<MockBackend>,
    first: MockCallGate,
    final_call: MockCallGate,
    db: eidetica::Database,
}

async fn start_parent(
    server: &Server,
    permit: tokio::sync::OwnedSemaphorePermit,
    is_async: bool,
    timeout: u64,
    observed_yield: bool,
) -> Parent {
    let (sid, db) = server
        .registry
        .create_session(Some("worker-parent"))
        .await
        .unwrap();
    let session = Arc::new(Mutex::new(Session::new(sid, db.clone()).await));
    let mut ctx = tool_context(session, server.tools.clone());
    ctx.agent_name = "default".into();
    let job_execution = if observed_yield {
        crate::session::execution::test_branch(&db, "owner")
            .await
            .unwrap();
        Some((db.clone(), "owner".into()))
    } else {
        None
    };
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls([
        (
            "spawn".into(),
            "spawn_worker".into(),
            serde_json::json!({"name":"helper", "task":"one child", "async": is_async}).to_string(),
        ),
        ("effect".into(), "permit_probe".into(), "{}".into()),
    ]);
    mock.push_text("parent finished");
    let first = mock.block_next_call();
    let final_call = mock.block_next_call();
    let backend = BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(server.registry.chaz_peer().clone()).await,
    );
    let policies = ToolPolicyRegistry::new(HashMap::from([(
        "spawn_worker".into(),
        ToolPolicy {
            timeout: Some(timeout),
            ..Default::default()
        }
        .into(),
    )]));
    let capacity = runtime::ExecutionCapacity::new(server.semaphore.clone(), permit, job_execution);
    let task = tokio::spawn(async move {
        runtime::execute_with_recorder(
            None,
            vec![runtime::RuntimeMessage::User("delegate".into())],
            &backend,
            &crate::test_support::permissive_security(),
            &ctx,
            &policies,
            None,
            None,
            Some(capacity),
            runtime::ModelCallScope::default(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), first.wait_started())
        .await
        .unwrap();
    Parent {
        task,
        mock,
        first,
        final_call,
        db,
    }
}

async fn children(server: &Server, expected: usize) -> Vec<String> {
    let rows = server.registry.list_sessions().await.unwrap();
    let children: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            row.source
                .as_deref()
                .is_some_and(|s| s.starts_with("spawn:"))
        })
        .collect();
    assert_eq!(
        children.len(),
        expected,
        "each call must register exactly one child"
    );
    let mut result = Vec::new();
    for child in children {
        let (_, db) = server
            .registry
            .open_session(&child.session_db_id)
            .await
            .unwrap();
        let session = Session::new(ConversationId(child.session_db_id.clone()), db.clone()).await;
        assert_eq!(
            session
                .entries()
                .iter()
                .filter(|e| e.entry_type == EntryType::Directive)
                .count(),
            1,
            "submit exactly once"
        );
        assert_eq!(
            session.attempts_for_test().await.len(),
            1,
            "run exactly once"
        );
        result.push(child.session_db_id);
    }
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn synchronous_worker_yields_at_capacity_one_and_ten_saturated_parents() {
    for count in [1, MAX_CONCURRENT_LLM_CALLS] {
        let (_owner, server, slot, child) = worker_fixture().await;
        // Reserve the other slots to exercise an effective capacity of one.
        let reserved = server
            .semaphore
            .clone()
            .acquire_many_owned((MAX_CONCURRENT_LLM_CALLS - count) as u32)
            .await
            .unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        server.tools.register(PermitProbe {
            semaphore: server.semaphore.clone(),
            calls: effects.clone(),
        });
        let mut parents = Vec::new();
        let mut child_gates = Vec::new();
        for _ in 0..count {
            child.push_text("child finished");
            child_gates.push(child.block_next_call());
            let permit = server.semaphore.clone().acquire_owned().await.unwrap();
            parents.push(start_parent(&server, permit, false, 300, false).await);
        }
        assert_eq!(server.semaphore.available_permits(), 0);
        assert!(
            child.recorded_calls().is_empty(),
            "submission has not started"
        );
        for parent in &parents {
            parent.first.release();
        }
        tokio::time::timeout(Duration::from_secs(20), async {
            for gate in &child_gates {
                gate.wait_started().await;
            }
        })
        .await
        .expect("saturated parents must yield so their Workers can run");
        assert_eq!(child.recorded_calls().len(), count);
        assert_eq!(
            server.semaphore.available_permits(),
            0,
            "children own all slots"
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert!(
            child
                .recorded_calls()
                .iter()
                .all(|call| call.tools.is_empty()),
            "Worker tool narrowing stays enforced"
        );
        let child_dbs = children(&server, count).await;
        // Queue ahead of parent reacquisition. Child settlement can complete,
        // but no parent effect or model call may run until these permits return.
        let mut blocker = Box::pin(server.semaphore.clone().acquire_many_owned(count as u32));
        assert!(futures::poll!(blocker.as_mut()).is_pending());
        for gate in &child_gates {
            gate.release();
        }
        let blocker = tokio::time::timeout(Duration::from_secs(10), blocker)
            .await
            .unwrap()
            .unwrap();
        for sid in &child_dbs {
            await_no_processing(&server, sid).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            effects.load(Ordering::SeqCst),
            0,
            "no post-wait effect without reacquisition"
        );
        assert!(parents.iter().all(|p| p.mock.recorded_calls().len() == 1));
        drop(blocker);
        tokio::time::timeout(Duration::from_secs(10), async {
            for parent in &parents {
                parent.final_call.wait_started().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(effects.load(Ordering::SeqCst), count);
        assert_eq!(
            server.semaphore.available_permits(),
            0,
            "resumed models own all slots"
        );
        for parent in parents {
            assert!(parent.mock.recorded_calls()[1].messages.iter().any(|msg| matches!(msg,
                runtime::RuntimeMessage::ToolResult { call_id, content } if call_id == "spawn" && content.contains("child finished"))));
            parent.final_call.release();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), parent.task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .body,
                "parent finished"
            );
        }
        children(&server, count).await;
        drop(reserved);
        assert_eq!(
            server.semaphore.available_permits(),
            MAX_CONCURRENT_LLM_CALLS
        );
        server.shutdown().await;
        slot.clear();
    }
}

#[tokio::test]
async fn worker_wait_balances_capacity_after_error_timeout_cancellation_and_observed_yield() {
    for mode in [
        "error",
        "timeout",
        "cancel-observation",
        "cancel-reacquire",
        "observed-yield",
        "closed",
    ] {
        let (_owner, server, slot, child) = worker_fixture().await;
        let reserved = server
            .semaphore
            .clone()
            .acquire_many_owned((MAX_CONCURRENT_LLM_CALLS - 1) as u32)
            .await
            .unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        server.tools.register(PermitProbe {
            semaphore: server.semaphore.clone(),
            calls: effects.clone(),
        });
        if mode == "error" {
            child.push_err(crate::error::LlmError::Configuration {
                message: "child failed".into(),
            });
        } else {
            child.push_text("child finished");
        }
        let gate = child.block_next_call();
        let permit = server.semaphore.clone().acquire_owned().await.unwrap();
        let parent = start_parent(&server, permit, false, 1, mode == "observed-yield").await;
        parent.first.release();
        tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
            .await
            .unwrap();
        let child_dbs = children(&server, 1).await;
        if mode == "cancel-observation" {
            parent.task.abort();
            assert!(matches!(parent.task.await, Err(error) if error.is_cancelled()));
            gate.release();
            await_no_processing(&server, &child_dbs[0]).await;
        } else {
            let mut blocker = Box::pin(server.semaphore.clone().acquire_owned());
            assert!(futures::poll!(blocker.as_mut()).is_pending());
            if mode == "timeout" {
                // The child holds the only slot beyond the unchanged outer
                // policy timeout. Capacity acquisition itself must not time out.
                tokio::time::sleep(Duration::from_millis(1100)).await;
            }
            gate.release();
            let blocker = tokio::time::timeout(Duration::from_secs(10), blocker)
                .await
                .unwrap()
                .unwrap();
            await_no_processing(&server, &child_dbs[0]).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(effects.load(Ordering::SeqCst), 0);
            assert_eq!(parent.mock.recorded_calls().len(), 1);
            if mode == "cancel-reacquire" {
                parent.task.abort();
                assert!(matches!(parent.task.await, Err(error) if error.is_cancelled()));
                drop(blocker);
            } else {
                if mode == "observed-yield" {
                    crate::session::execution::test_supersede(&parent.db)
                        .await
                        .unwrap();
                }
                if mode == "closed" {
                    server.semaphore.close();
                }
                drop(blocker);
                if mode == "observed-yield" || mode == "closed" {
                    let error = tokio::time::timeout(Duration::from_secs(10), parent.task)
                        .await
                        .unwrap()
                        .unwrap()
                        .expect_err("parent must stop before further effects");
                    let expected = if mode == "observed-yield" {
                        "job execution yielded while observing"
                    } else {
                        "executor capacity closed"
                    };
                    assert!(error.contains(expected), "{mode}: {error}");
                    assert_eq!(effects.load(Ordering::SeqCst), 0);
                    assert_eq!(parent.mock.recorded_calls().len(), 1);
                } else {
                    tokio::time::timeout(Duration::from_secs(10), parent.final_call.wait_started())
                        .await
                        .unwrap();
                    let expected = if mode == "timeout" {
                        "Tool timed out after 1 seconds"
                    } else {
                        "Tool error: Error: Configuration error: child failed"
                    };
                    assert!(parent.mock.recorded_calls()[1].messages.iter().any(|msg| matches!(msg,
                        runtime::RuntimeMessage::ToolResult { call_id, content } if call_id == "spawn" && content.contains(expected))));
                    parent.final_call.release();
                    tokio::time::timeout(Duration::from_secs(10), parent.task)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                    assert_eq!(effects.load(Ordering::SeqCst), 1);
                }
            }
        }
        assert_eq!(
            child.recorded_calls().len(),
            1,
            "timeout/cancel/error never resubmit"
        );
        children(&server, 1).await;
        drop(reserved);
        assert_eq!(
            server.semaphore.available_permits(),
            MAX_CONCURRENT_LLM_CALLS
        );
        server.shutdown().await;
        slot.clear();
    }
}

#[tokio::test]
async fn asynchronous_worker_keeps_parent_capacity_and_submits_once() {
    let (_owner, server, slot, child) = worker_fixture().await;
    let reserved = server
        .semaphore
        .clone()
        .acquire_many_owned((MAX_CONCURRENT_LLM_CALLS - 1) as u32)
        .await
        .unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    server.tools.register(PermitProbe {
        semaphore: server.semaphore.clone(),
        calls: effects.clone(),
    });
    child.push_text("child finished");
    let gate = child.block_next_call();
    let permit = server.semaphore.clone().acquire_owned().await.unwrap();
    let parent = start_parent(&server, permit, true, 300, false).await;
    parent.first.release();
    tokio::time::timeout(Duration::from_secs(10), parent.final_call.wait_started())
        .await
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert!(parent.mock.recorded_calls()[1].messages.iter().any(|msg| matches!(msg,
        runtime::RuntimeMessage::ToolResult { call_id, content } if call_id == "spawn" && content.contains("Worker 'helper' spawned asynchronously in session "))));
    assert!(
        child.recorded_calls().is_empty(),
        "async submission must retain parent capacity"
    );
    assert_eq!(server.semaphore.available_permits(), 0);
    parent.final_call.release();
    tokio::time::timeout(Duration::from_secs(10), parent.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
        .await
        .unwrap();
    let child_dbs = children(&server, 1).await;
    gate.release();
    await_no_processing(&server, &child_dbs[0]).await;
    drop(reserved);
    assert_eq!(
        server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
    server.shutdown().await;
    slot.clear();
}

#[tokio::test]
async fn worker_executor_shutdown_stops_observation_and_reacquisition() {
    worker_stop_observation_and_reacquisition(false).await;
}

#[tokio::test]
async fn worker_user_stop_pairs_calls_during_observation_and_reacquisition() {
    worker_stop_observation_and_reacquisition(true).await;
}

async fn worker_stop_observation_and_reacquisition(user_stop: bool) {
    for reacquiring in [false, true] {
        let (_owner, server, slot, child) = worker_fixture().await;
        let reserved = server
            .semaphore
            .clone()
            .acquire_many_owned((MAX_CONCURRENT_LLM_CALLS - 1) as u32)
            .await
            .unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        server.tools.register(PermitProbe {
            semaphore: server.semaphore.clone(),
            calls: effects.clone(),
        });
        child.push_text("child finished");
        let gate = child.block_next_call();
        let mock = Arc::new(MockBackend::new());
        mock.push_tool_calls([
            (
                "spawn".into(),
                "spawn_worker".into(),
                serde_json::json!({"name":"helper", "task":"one child"}).to_string(),
            ),
            ("effect".into(), "permit_probe".into(), "{}".into()),
        ]);
        mock.push_text("must not continue");
        let (sid, db) = server
            .registry
            .create_session(Some("worker-parent"))
            .await
            .unwrap();
        let (security, approval_task) =
            crate::test_support::security_with_decision(crate::bridge::ApprovalDecision::Approve);
        server
            .register_session(
                &db,
                BackendManager::with_mock(
                    mock.clone(),
                    crate::security::SecretStore::new(server.registry.chaz_peer().clone()).await,
                ),
                Some("default".into()),
                security.approval_callback,
            )
            .await
            .unwrap();
        Session::new(sid.clone(), db.clone())
            .await
            .add_entry(SessionEntry {
                sender: "user".into(),
                content: "delegate".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Directive,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
            .await
            .expect("approved parent must reach the Worker");
        let child_dbs = children(&server, 1).await;
        let blocker = if reacquiring {
            let mut blocker = Box::pin(server.semaphore.clone().acquire_owned());
            assert!(futures::poll!(blocker.as_mut()).is_pending());
            gate.release();
            let blocker = tokio::time::timeout(Duration::from_secs(10), blocker)
                .await
                .unwrap()
                .unwrap();
            await_no_processing(&server, &child_dbs[0]).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            Some(blocker)
        } else {
            None
        };
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(mock.recorded_calls().len(), 1);
        if user_stop {
            let mut frontend = Session::new(sid.clone(), db.clone()).await;
            assert!(frontend.request_stop("user").await.unwrap());
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let session = Session::new(sid.clone(), db.clone()).await;
                    if session.entries().iter().any(|entry| {
                        entry.entry_type == EntryType::Error
                            && entry.content == TURN_STOPPED_MESSAGE
                    }) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("user stop must interrupt Worker observation or reacquisition promptly");
            await_no_processing(&server, &sid.0).await;
            gate.release();
            await_no_processing(&server, &child_dbs[0]).await;
            server.shutdown().await;
        } else {
            tokio::time::timeout(Duration::from_secs(10), server.shutdown())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), gate.wait_stopped())
                .await
                .unwrap();
        }
        approval_task.abort();
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(mock.recorded_calls().len(), 1);
        assert_eq!(child.recorded_calls().len(), 1);
        let session = Session::new(sid, db).await;
        assert_eq!(session.attempts_for_test().await.len(), 1);
        let attempt = &session.attempts_for_test().await[0];
        assert_eq!(
            attempt.status,
            if user_stop {
                crate::session::TurnAttemptStatus::Completed
            } else {
                crate::session::TurnAttemptStatus::Started
            }
        );
        if user_stop {
            let records = session.turn_transcript(&attempt.attempt_id).await.unwrap();
            let cancelled: Vec<_> = records
                .iter()
                .filter_map(|record| match &record.message {
                    TurnTranscriptMessage::ToolResult {
                        call_id,
                        outcome: runtime::ToolResultOutcome::Cancelled,
                        ..
                    } => Some(call_id.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(cancelled, vec!["spawn", "effect"]);
            assert_eq!(
                records
                    .iter()
                    .filter(|record| matches!(record.message, TurnTranscriptMessage::Cancelled))
                    .count(),
                1,
                "persist exactly one cancellation terminal"
            );
            children(&server, 1).await;
        }
        drop(blocker);
        drop(reserved);
        assert_eq!(
            server.semaphore.available_permits(),
            MAX_CONCURRENT_LLM_CALLS
        );
        slot.clear();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_worker_links_precede_execution_and_resolve_each_exact_result() {
    use crate::session::worker_links::*;
    let (_owner, server, slot, child) = worker_fixture().await;
    child.push_text("first child result");
    child.push_text("second child result");
    let gate = child.block_next_call();
    let mock = Arc::new(MockBackend::new());
    for task in ["first assignment", "second assignment"] {
        // Providers may reuse a call ID in another model response. The full
        // native call site must keep these two creations distinct.
        mock.push_tool_calls([(
            "reused-call".into(),
            "spawn_worker".into(),
            serde_json::json!({"name":"helper","task":task}).to_string(),
        )]);
    }
    mock.push_text("parent complete");
    let (sid, db) = server
        .registry
        .create_session(Some("worker-parent"))
        .await
        .unwrap();
    let (security, approval_task) =
        crate::test_support::security_with_decision(crate::bridge::ApprovalDecision::Approve);
    server
        .register_session(
            &db,
            BackendManager::with_mock(
                mock.clone(),
                crate::security::SecretStore::new(server.registry.chaz_peer().clone()).await,
            ),
            Some("default".into()),
            security.approval_callback,
        )
        .await
        .unwrap();
    let mut session = Session::new(sid.clone(), db.clone()).await;
    let request = session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "delegate".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Directive,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
        .await
        .unwrap();
    let first = worker_spawns(&db).await.unwrap();
    assert_eq!(
        first.len(),
        1,
        "parent reference must already exist before child calls its model"
    );
    assert_eq!(first[0].call.request_id, request);
    assert_eq!(first[0].call.agent, "default");
    assert_eq!(first[0].call.execution_id, None);
    let (_, child_db) = server
        .registry
        .open_session(&first[0].child_session_id)
        .await
        .unwrap();
    assert_eq!(
        verify_worker_spawn(&db, &child_db, &first[0])
            .await
            .unwrap()
            .content,
        "first assignment"
    );
    assert!(
        latest_worker_outcome(&child_db, &first[0].directive_id)
            .await
            .unwrap()
            .is_none()
    );
    gate.release();
    await_no_processing(&server, &sid.0).await;
    let mut links = worker_spawns(&db).await.unwrap();
    links.sort_by_key(|link| link.call.model_sequence);
    assert_eq!(links.len(), 2);
    assert_ne!(links[0].spawn_id, links[1].spawn_id);
    assert_ne!(links[0].child_session_id, links[1].child_session_id);
    for (index, link) in links.iter().enumerate() {
        assert_eq!(link.call.call_id, "reused-call");
        assert_eq!(link.call.call_index, 0);
        assert_eq!(link.call.model_sequence, index as u64);
        assert_eq!(
            link.call.attempt_id,
            session.attempts_for_test().await[0].attempt_id
        );
        let (_, child_db) = server
            .registry
            .open_session(&link.child_session_id)
            .await
            .unwrap();
        verify_worker_spawn(&db, &child_db, link).await.unwrap();
        let (outcome, result) = latest_worker_outcome(&child_db, &link.directive_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.terminal, WorkerTerminal::Reply);
        let reopened = Session::new(
            ConversationId(link.child_session_id.clone()),
            child_db.clone(),
        )
        .await;
        let attempt = reopened
            .read_turn_attempt(&outcome.attempt_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attempt.request_id, link.directive_id);
        assert_eq!(outcome.execution_id, None);
        let completion = crate::session::execution::store_entries_at(
            &child_db,
            "worker_outcomes_v1",
            &child_db.snapshot().await.unwrap(),
        )
        .await
        .unwrap();
        let [commit] = completion.as_slice() else {
            panic!("expected one Worker outcome commit");
        };
        for store in ["entries", "turn_attempts", "turn_transcript"] {
            assert!(
                commit.in_subtree(store),
                "Worker completion omitted {store}"
            );
        }
        assert_eq!(
            result.unwrap().content,
            ["first child result", "second child result"][index]
        );
    }
    assert_eq!(mock.recorded_calls().len(), 3);
    assert_eq!(child.recorded_calls().len(), 2);
    let tool_results: Vec<_> = mock
        .recorded_calls()
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| match m {
            runtime::RuntimeMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert!(tool_results[0].contains("first child result"));
    assert!(tool_results[1].contains("second child result"));
    approval_task.abort();
    server.shutdown().await;
    slot.clear();
}

#[tokio::test]
async fn prepared_worker_reference_and_catalog_are_not_execution_requests() {
    use crate::session::worker_links::*;
    let (_owner, server, slot, model) = worker_fixture().await;
    let (_, parent) = server
        .registry
        .create_session(Some("parent"))
        .await
        .unwrap();
    let child = server
        .registry
        .prepare_worker_session(&parent, "spawn:prepared-only")
        .await
        .unwrap();
    let directive = Session::prepare_worker_directive(
        &child,
        &parent,
        "prepared-only",
        SessionEntry {
            sender: "default".into(),
            content: "do not execute".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Directive,
            metadata: None,
            routing: None,
        },
        true,
    )
    .await
    .unwrap();
    assert!(
        !server
            .registry
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|row| row.session_db_id == child.root_id().to_string())
    );
    assert!(
        server
            .sessions
            .lock()
            .await
            .get(&child.root_id().to_string())
            .is_none()
    );
    // Failure to find the recorded invocation cannot publish a parent link.
    let bad_call = WorkerCallSite {
        request_id: TurnRequestId::parse("missing"),
        attempt_id: "missing".into(),
        execution_id: None,
        agent: "default".into(),
        model_sequence: 0,
        call_index: 0,
        call_id: "missing".into(),
    };
    assert!(
        publish_spawn(&parent, &child, "prepared-only", directive, bad_call)
            .await
            .is_err()
    );
    assert!(worker_spawns(&parent).await.unwrap().is_empty());
    server
        .registry
        .publish_prepared_session(&child, "spawn:prepared-only")
        .await
        .unwrap();
    // Exercise the resident's actual adoption decision after publication. A
    // catalog entry or a backlink must not recreate template/tool authority.
    let row = server
        .registry
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.session_db_id == child.root_id().to_string())
        .unwrap();
    let observer_registry = Arc::new(
        crate::session::SessionRegistry::new(
            server.registry.instance().clone(),
            server
                .registry
                .instance()
                .login_account("test", "test-password")
                .await
                .unwrap(),
            server.agents.clone(),
        )
        .await
        .unwrap(),
    );
    let (_second_owner, observer, _) = server_fixture_from_registry(observer_registry).await;
    assert!(!super::resident_sessions::is_resident_candidate(&row));
    assert!(
        observer
            .sessions
            .lock()
            .await
            .get(&child.root_id().to_string())
            .is_none()
    );
    observer.shutdown().await;
    let session = Session::new(ConversationId(child.root_id().to_string()), child.clone()).await;
    assert_eq!(session.entries().len(), 1);
    assert!(session.attempts_for_test().await.is_empty());
    assert!(model.recorded_calls().is_empty());
    assert!(
        server
            .sessions
            .lock()
            .await
            .get(&child.root_id().to_string())
            .is_none()
    );
    server.shutdown().await;
    slot.clear();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recorded_and_scheduled_turns_join_only_their_exact_background_workers() {
    use crate::session::worker_links::{latest_worker_outcome, worker_spawns};

    async fn entered(gate: &MockCallGate, phase: &str) {
        tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
            .await
            .unwrap_or_else(|_| panic!("scripted phase did not start: {phase}"));
    }
    fn spawn(task: &str) -> (String, String, String) {
        (
            "reused-spawn-id".into(),
            "spawn_worker".into(),
            serde_json::json!({"name": "helper", "task": task, "async": true}).to_string(),
        )
    }
    fn wait(call: &str, child: &str, seconds: u64) -> (String, String, String) {
        (
            call.into(),
            "worker_wait".into(),
            serde_json::json!({"session_db_id": child, "timeout_seconds": seconds}).to_string(),
        )
    }
    fn tool_result(model: &MockBackend, id: &str) -> String {
        model
            .recorded_calls()
            .last()
            .unwrap()
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                runtime::RuntimeMessage::ToolResult { call_id, content } if call_id == id => {
                    Some(content.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing model-visible result for {id}"))
    }

    let interactive = Arc::new(MockBackend::new());
    let scheduled = Arc::new(MockBackend::new());
    let secrets = crate::test_support::empty_secrets().await;
    let (_owner, server, registry) = server_fixture_backend_with_approvals(
        Arc::new(crate::tool_host::NativeToolHost::new()),
        Arc::new(crate::test_support::context_hub().await),
        Some(BackendManager::with_mock(
            scheduled.clone(),
            secrets.clone(),
        )),
        &["spawn_worker"],
    )
    .await;
    let (slot, children_mock) = install_worker_fixture(&server, &registry).await;
    let owner = server.agent_index.find_by_name("default").unwrap();
    let adb = server.open_agent_db_by_name("default").await.unwrap();
    let (sid, db) = registry.create_session(Some("tui")).await.unwrap();
    registry
        .attach_agent_to_session(&sid.0, &owner)
        .await
        .unwrap();
    server
        .register_session(
            &db,
            BackendManager::with_mock(interactive.clone(), secrets),
            Some("default".into()),
            None,
        )
        .await
        .unwrap();
    let wake = "private wake prompt; not a queued user turn";
    let target = crate::agent_db::ScheduleTarget::Pinned {
        session_db_id: sid.0.clone(),
    };
    adb.upsert_schedule(Schedule::new(
        "worker-fire",
        Trigger::OneShot {
            fire_at: Utc::now(),
        },
        wake,
        target,
    ))
    .await
    .unwrap();
    let payload = pinned_schedule_payload(&owner.db_id.to_string(), "worker-fire", wake, &sid.0);
    let reserved = server
        .semaphore
        .clone()
        .acquire_many_owned((MAX_CONCURRENT_LLM_CALLS - 2) as u32)
        .await
        .unwrap();
    let mut joined = Vec::new();

    for (is_schedule, model) in [(false, &interactive), (true, &scheduled)] {
        let start = model.block_next_call();
        let after_one = model.block_next_call();
        let after_two = model.block_next_call();
        let after_second_join = model.block_next_call();
        let after_pending = model.block_next_call();
        let after_first_join = model.block_next_call();
        let first_child = children_mock.block_next_call();
        let second_child = children_mock.block_next_call();
        let first_reply = if is_schedule {
            "scheduled first result"
        } else {
            "interactive first result"
        };
        let final_reply = if is_schedule {
            "scheduled parent final"
        } else {
            "interactive parent final"
        };
        children_mock.push_text(first_reply);
        children_mock.push_err(crate::error::LlmError::Configuration {
            message: "exact second failure".into(),
        });
        model.push_tool_calls([spawn("first instruction")]);
        model.push_tool_calls([spawn("second instruction")]);
        let mut fire = None;
        if is_schedule {
            let server = server.clone();
            let payload = payload.clone();
            fire = Some(tokio::spawn(async move {
                server.fire_agent_schedule(payload).await
            }));
        } else {
            write_user_message_with_content(&db, &sid.0, "interactive parent request").await;
        }
        entered(&start, "start").await;
        if !is_schedule {
            // A real competing fire must skip while the interactive runtime owns the target.
            tokio::time::timeout(
                Duration::from_secs(5),
                server.fire_agent_schedule(payload.clone()),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(scheduled.recorded_calls().is_empty());
            assert!(adb.list_schedule_fires().await.unwrap().is_empty());
            assert!(adb.find_schedule("worker-fire").await.unwrap().is_some());
        }
        start.release();
        entered(&after_one, "after_one").await;
        entered(&first_child, "first_child").await;
        after_one.release();
        entered(&after_two, "after_two").await;

        let mut links = worker_spawns(&db).await.unwrap();
        links.retain(|link| !joined.contains(&link.child_session_id));
        links.sort_by_key(|link| link.call.model_sequence);
        let [first, second] = links.as_slice() else {
            panic!("both Workers must be published before either completes: {links:?}")
        };
        assert_eq!(first.call.attempt_id, second.call.attempt_id);
        assert_eq!(
            first.call.request_id.as_str().starts_with("schedule:"),
            is_schedule
        );
        assert_eq!(first.call.agent, "default");
        assert_eq!(first.call.call_id, second.call.call_id);
        let (_, first_db) = registry
            .open_worker_session(&first.child_session_id)
            .await
            .unwrap();
        assert!(
            latest_worker_outcome(&first_db, &first.directive_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            model.recorded_calls()[0]
                .tools
                .iter()
                .any(|tool| tool.name == "worker_wait")
        );
        assert!(tool_result(model, "reused-spawn-id").contains(&second.child_session_id));

        let (id, name, arguments) = wait("second", &second.child_session_id, 30);
        model.push_tool_calls([wait("short", &first.child_session_id, 1)]);
        after_two.release_with(runtime::LLMResponse::ToolCalls {
            content: None,
            tool_calls: vec![runtime::ToolCallRequest {
                id,
                name,
                arguments,
            }],
            provider_extra: Default::default(),
            metadata: None,
        });
        // In the interactive case both slots were held by parent and first
        // Worker. The second cannot start without the native wait yielding.
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(10), second_child.wait_started()) => result.expect("second Worker requires a yielded slot"),
            _ = after_second_join.wait_started() => panic!("join did not observe the second Worker: {}", tool_result(model, "second")),
        }
        second_child.release();
        entered(&after_second_join, "after_second_join").await;
        let second_result = tool_result(model, "second");
        assert!(
            second_result.contains("\"state\":\"failed\"")
                && second_result.contains("exact second failure"),
            "{second_result}"
        );
        assert!(!second_result.contains(first_reply));
        model.push_tool_calls([
            wait("first", &first.child_session_id, 30),
            wait("again", &first.child_session_id, 30),
        ]);
        after_second_join.release();
        entered(&after_pending, "after_pending").await;
        assert!(tool_result(model, "short").contains("\"state\":\"pending\""));
        assert!(
            latest_worker_outcome(&first_db, &first.directive_id)
                .await
                .unwrap()
                .is_none()
        );
        first_child.release();
        await_no_processing(&server, &first.child_session_id).await;
        let mut worker =
            Session::new(ConversationId(first.child_session_id.clone()), first_db).await;
        worker
            .add_entry(SessionEntry {
                sender: "default".into(),
                content: "unrelated newer child message".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        assert_eq!(
            worker.latest_entry().unwrap().content,
            "unrelated newer child message"
        );
        model.push_text(final_reply);
        after_pending.release();
        entered(&after_first_join, "after_first_join").await;
        let first_result = tool_result(model, "first");
        assert!(
            first_result.contains("\"state\":\"completed\"") && first_result.contains(first_reply),
            "{first_result}"
        );
        assert!(!first_result.contains("unrelated newer child message"));
        assert_eq!(tool_result(model, "again"), first_result);
        after_first_join.release();
        if let Some(fire) = fire {
            tokio::time::timeout(Duration::from_secs(10), fire)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(adb.list_schedule_fires().await.unwrap().len(), 1);
        }
        await_no_processing(&server, &sid.0).await;
        joined.extend(links.into_iter().map(|link| link.child_session_id));
    }

    // Same Agent and same session, but a different actual invocation. Both
    // previously successful handles remain accessible to the operator API.
    interactive.push_tool_calls([
        wait("old-interactive", &joined[0], 1),
        wait("old-schedule", &joined[2], 1),
    ]);
    interactive.push_text("interactive still routed to its original backend");
    let denied = interactive.block_next_call();
    let checked = interactive.block_next_call();
    write_user_message_with_content(&db, &sid.0, "later interactive request").await;
    entered(&denied, "denied").await;
    denied.release();
    entered(&checked, "checked").await;
    for id in ["old-interactive", "old-schedule"] {
        let refusal = tool_result(&interactive, id);
        assert!(
            refusal.contains("Worker result unavailable for this invocation"),
            "{refusal}"
        );
        assert!(!refusal.contains("first result"));
    }
    let replayed = format!(
        "{:?}",
        interactive.recorded_calls().last().unwrap().messages
    );
    assert!(!replayed.contains(wake));
    assert!(
        !replayed.contains("scheduled first result"),
        "a scheduled ToolResult is audit, not an interactive replay exchange"
    );
    checked.release();
    await_no_processing(&server, &sid.0).await;
    let parent = Session::new(sid, db).await;
    assert_eq!(
        parent.attempts_for_test().await.len(),
        2,
        "no scheduled request was queued"
    );
    assert_eq!(
        scheduled.recorded_calls().len(),
        6,
        "never route the interactive request to the schedule backend"
    );
    assert!(
        parent
            .entries()
            .iter()
            .all(|entry| !entry.content.contains(wake))
    );
    assert_eq!(children_mock.recorded_calls().len(), 4);
    children(&server, 4).await;
    drop(reserved);
    assert_eq!(
        server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
    server.shutdown().await;
    slot.clear();
}
