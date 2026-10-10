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
    let (_, adb) = seed_agent(&server, &registry, "default").await;
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
        .hydrate_agent_from_db(server.agents.default_agent().clone())
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
    (owner, server, slot, child)
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
    claim: bool,
) -> Parent {
    let (sid, db) = server
        .registry
        .create_session(Some("worker-parent"))
        .await
        .unwrap();
    let session = Arc::new(Mutex::new(Session::new(sid, db.clone()).await));
    let mut ctx = tool_context(session, server.tools.clone());
    ctx.agent_name = "default".into();
    let job_claim = if claim {
        assert!(
            crate::session::jobs::claim_job(&db, "peer", "default", "owner")
                .await
                .unwrap()
        );
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
            timeout,
            ..Default::default()
        },
    )]));
    let capacity = runtime::ExecutionCapacity::new(server.semaphore.clone(), permit, job_claim);
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
async fn worker_wait_balances_capacity_after_error_timeout_cancellation_and_claim_loss() {
    for mode in [
        "error",
        "timeout",
        "cancel-observation",
        "cancel-reacquire",
        "claim-loss",
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
        let parent = start_parent(&server, permit, false, 1, mode == "claim-loss").await;
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
                if mode == "claim-loss" {
                    use eidetica::store::DocStore;
                    let txn = parent.db.new_transaction().await.unwrap();
                    let mut owner = crate::session::jobs::read_job_owner(&parent.db)
                        .await
                        .unwrap()
                        .unwrap();
                    owner.incarnation = "rival".into();
                    txn.get_store::<DocStore>("job_owner")
                        .await
                        .unwrap()
                        .set_string("v1", serde_json::to_string(&owner).unwrap())
                        .await
                        .unwrap();
                    txn.commit().await.unwrap();
                }
                if mode == "closed" {
                    server.semaphore.close();
                }
                drop(blocker);
                if mode == "claim-loss" || mode == "closed" {
                    let error = tokio::time::timeout(Duration::from_secs(10), parent.task)
                        .await
                        .unwrap()
                        .unwrap()
                        .err()
                        .expect("parent must stop before further effects");
                    let expected = if mode == "claim-loss" {
                        "job lost claim while observing"
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
        tokio::time::timeout(Duration::from_secs(10), server.shutdown())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), gate.wait_stopped())
            .await
            .unwrap();
        approval_task.abort();
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(mock.recorded_calls().len(), 1);
        assert_eq!(child.recorded_calls().len(), 1);
        let session = Session::new(sid, db).await;
        assert_eq!(session.attempts_for_test().await.len(), 1);
        assert_eq!(
            session.attempts_for_test().await[0].status,
            crate::session::TurnAttemptStatus::Started
        );
        drop(blocker);
        drop(reserved);
        assert_eq!(
            server.semaphore.available_permits(),
            MAX_CONCURRENT_LLM_CALLS
        );
        slot.clear();
    }
}
