use super::*;
use crate::runtime::RuntimeMessage;
use crate::session::{
    jobs::JobState,
    steering::{JobInputRequest, JobInputState},
};

/// Real separate client/executor connections through a disposable Eidetica
/// service. Only the resident executor has a recording deterministic backend.
async fn service_steering(
    tool_exchange: bool,
    restart_at_call: Option<u64>,
    close_race: bool,
    wait_graph: bool,
) {
    use eidetica::service::ServiceServer;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("monitor.sock");
    let (owner, mut user) =
        Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("monitor"))
            .await
            .unwrap();
    for name in ["monitor-observer", "monitor-reader"] {
        user.admin()
            .await
            .unwrap()
            .create_user(NewUser::passwordless(name))
            .await
            .unwrap();
    }
    let (agent_db, pubkey) = create_agent_db(
        &mut user,
        "default",
        &AgentDbConfig::default(),
        &AgentMeta {
            display_name: Some("default".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let agent = DbEntry {
        db_id: agent_db.id(),
        display_name: "default".into(),
        pubkey,
    };
    let service = ServiceServer::bind(owner, &socket).await.unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(());
    let task = tokio::spawn(service.run(receiver));
    let settings = crate::config::EideticaConfig {
        connection: format!("unix://{}", socket.display()),
        login: crate::config::EideticaLoginConfig {
            username: "monitor".into(),
            password: None,
            passwordless: true,
        },
        sync: None,
    };
    let agents = Arc::new(AgentRegistry::with_default_agent());
    let connection = crate::instance::connect_with(&settings, crate::config::ExecutionRole::Client)
        .await
        .unwrap();
    let registry = Arc::new(
        crate::session::SessionRegistry::new(connection.instance, connection.user, agents.clone())
            .await
            .unwrap(),
    );
    let client = client_server_fixture_from_registry(registry.clone()).await;
    let mock = Arc::new(crate::test_support::MockBackend::new().with_supports_tools(tool_exchange));
    if tool_exchange {
        mock.push_tool_calls([(
            "calc".into(),
            "calculate".into(),
            r#"{"expression":"2+2"}"#.into(),
        )]);
    } else {
        mock.push_text("provisional");
    }
    if !wait_graph {
        mock.push_text("continued terminal");
    }
    let gate = mock.block_next_call();
    client.agent_index.register(agent.clone());
    let (executor, _) =
        published_executor_using_mock(&settings, agents.clone(), agent.clone(), mock.clone()).await;
    let wait_slot = crate::instance::ServerSlot::default();
    if wait_graph {
        wait_slot.set(executor.clone());
        executor.tools.register(crate::tools::JobWaitTool {
            server: wait_slot.clone(),
        });
    }
    // Publish the parent after the executor's User snapshot, as the real
    // client-role TUI does. The registry binds its authenticated identity.
    let (parent_id, parent_db) = registry.create_session(Some("cli")).await.unwrap();
    registry
        .attach_agent_to_session(&parent_id.0, &agent)
        .await
        .unwrap();
    let id = if wait_graph {
        assert!(
            executor
                .registry
                .user_for_tests()
                .await
                .find_key(&eidetica::entry::ID::parse(&parent_id.0).unwrap())
                .unwrap()
                .is_none()
        );
        executor
            .registry
            .open_local_client_session(&parent_id.0)
            .await
            .unwrap();
        executor
            .submit_agent_job(&parent_id.0, "default", "original task")
            .await
            .unwrap()
    } else {
        client
            .submit_agent_job(&parent_id.0, "default", "original task")
            .await
            .unwrap()
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), gate.wait_started())
        .await
        .unwrap();
    let (_, db) = registry.open_job_session(&id).await.unwrap();
    let acceptance = crate::session::jobs::read_accepted_job(&db)
        .await
        .unwrap()
        .unwrap();
    let attempt_id = match client.job_status(&id).await.unwrap().state {
        JobState::StartedUnknown { attempt_id, .. } => attempt_id,
        other => panic!("expected observable start, got {other:?}"),
    };
    assert!(!client.is_watching_session(&id).await);
    // Fixture-only provisioning: a separate login holds Write, never the
    // submitter/executor's Admin key. Production observation grants nothing.
    let mut observer_settings = settings.clone();
    observer_settings.login.username = "monitor-observer".into();
    let mut observer_connection =
        crate::instance::connect_with(&observer_settings, crate::config::ExecutionRole::Client)
            .await
            .unwrap();
    let observer_key = observer_connection.user.get_default_key().unwrap();
    for shared_db in [&db, &parent_db, agent_db.database(), registry.chaz_group()] {
        let txn = shared_db.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(
                &observer_key,
                eidetica::auth::AuthKey::active(
                    Some("write-observer"),
                    eidetica::auth::types::Permission::Write(0),
                ),
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
        observer_connection
            .user
            .track_database(
                shared_db.root_id().clone(),
                &observer_key,
                Default::default(),
            )
            .await
            .unwrap();
        let observed_db = observer_connection
            .user
            .open_database_with_key(shared_db.root_id(), &observer_key)
            .await
            .unwrap();
        assert_eq!(
            observed_db.current_permission().await.unwrap(),
            eidetica::auth::types::Permission::Write(0)
        );
    }
    let observer_registry = Arc::new(
        crate::session::SessionRegistry::new(
            observer_connection.instance,
            observer_connection.user,
            agents.clone(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        observer_registry.chaz_group().root_id(),
        registry.chaz_group().root_id()
    );
    let observer = client_server_fixture_from_registry(observer_registry.clone()).await;
    observer.agent_index.register(DbEntry {
        pubkey: observer_key,
        ..agent.clone()
    });
    let (_, observed_db) = observer_registry.open_job_session(&id).await.unwrap();
    assert_eq!(
        observed_db.current_permission().await.unwrap(),
        eidetica::auth::types::Permission::Write(0)
    );
    let entries_before = Session::new(ConversationId(id.clone()), db.clone())
        .await
        .entries()
        .len();
    let catalog_before = registry.list_sessions().await.unwrap().len();
    let monitor = observer.job_monitor().await.unwrap();
    assert_eq!(
        Session::new(ConversationId(id.clone()), db.clone())
            .await
            .entries()
            .len(),
        entries_before,
        "monitoring appended an execution turn"
    );
    assert_eq!(
        registry.list_sessions().await.unwrap().len(),
        catalog_before,
        "monitoring created work"
    );
    assert!(
        monitor.nodes.iter().any(|node| node.session_db_id == id
            && node.claimed
            && node.unavailable.is_none()
            && !node.preview.is_empty()
            && node.parent_id.as_deref() == Some(&parent_id.0)),
        "{monitor:#?}"
    );
    assert!(
        monitor
            .nodes
            .iter()
            .any(|node| node.session_db_id == parent_id.0 && !node.claimed && node.state.is_none())
    );
    assert_eq!(mock.recorded_calls().len(), 1, "monitoring never executes");
    let input = JobInputRequest {
        id: uuid::Uuid::new_v4().to_string(),
        attempt_id: attempt_id.clone(),
        text: "steer this same job".into(),
    };
    if close_race {
        // A publisher read the still-open snapshot, but commits after the
        // executor's empty closing boundary. Publication is not acceptance.
        let txn = db.new_transaction().await.unwrap();
        txn.get_store::<eidetica::store::Table<JobInputRequest>>("job_inputs")
            .await
            .unwrap()
            .set(&input.id, input.clone())
            .await
            .unwrap();
        assert!(
            crate::session::steering::input_boundary(&db, &attempt_id, 1, true)
                .await
                .unwrap()
                .is_empty()
        );
        txn.commit().await.unwrap();
        assert!(matches!(
            observer.job_inputs(&id).await.unwrap()[0].state,
            JobInputState::NotApplied { .. }
        ));
        assert!(
            observer
                .submit_job_input(&id, JobInputRequest::new(attempt_id, "too late".into()))
                .await
                .is_err()
        );
        gate.release();
        assert_eq!(
            client
                .wait_job(&id, std::time::Duration::from_secs(14))
                .await
                .unwrap()
                .state,
            JobState::Succeeded {
                text: Some("provisional".into())
            }
        );
        assert_eq!(mock.recorded_calls().len(), 1);
        executor.shutdown().await;
        drop(shutdown);
        task.await.unwrap().unwrap();
        return;
    }
    assert_eq!(
        observer
            .submit_job_input(&id, input.clone())
            .await
            .unwrap()
            .state,
        JobInputState::Queued
    );
    // Lost acknowledgement: retry the same request without a second logical input.
    observer.submit_job_input(&id, input.clone()).await.unwrap();
    assert!(
        observer
            .submit_job_input(
                &id,
                JobInputRequest {
                    text: "changed payload".into(),
                    ..input.clone()
                }
            )
            .await
            .is_err()
    );
    assert!(
        observer
            .submit_job_input(
                &id,
                JobInputRequest {
                    id: uuid::Uuid::new_v4().to_string(),
                    attempt_id: "another-attempt".into(),
                    ..input.clone()
                }
            )
            .await
            .is_err()
    );
    assert!(
        serde_json::from_value::<JobInputRequest>(serde_json::json!({
            "id": input.id, "attempt_id": attempt_id, "text": input.text, "allowed_tools": ["shell"]
        }))
        .is_err(),
        "input cannot carry a widened execution scope"
    );
    // Keep the reader's mappings in a separate login. Publishing a Read key
    // into the writer's key map makes the existing implicit session opener
    // select that key nondeterministically after restart.
    let mut reader_settings = settings.clone();
    reader_settings.login.username = "monitor-reader".into();
    let mut reader_connection =
        crate::instance::connect_with(&reader_settings, crate::config::ExecutionRole::Client)
            .await
            .unwrap();
    // Same service, independent Read-only and unentitled keys remain refused.
    // The deferred raw Read cache reproducer below retains the upstream bug.
    let read_db = {
        let user = &mut reader_connection.user;
        let read_key = user
            .add_private_key(Some("monitor-read-only"))
            .await
            .unwrap();
        let bad_key = user
            .add_private_key(Some("monitor-unentitled"))
            .await
            .unwrap();
        let txn = db.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(
                &read_key,
                eidetica::auth::AuthKey::active(
                    Some("reader"),
                    eidetica::auth::types::Permission::Read,
                ),
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
        user.map_key(
            &read_key,
            db.root_id(),
            eidetica::auth::SigKey::from_pubkey(&read_key),
        )
        .await
        .unwrap();
        user.map_key(
            &bad_key,
            db.root_id(),
            eidetica::auth::SigKey::from_pubkey(&bad_key),
        )
        .await
        .unwrap();
        assert!(
            user.open_database_with_key(db.root_id(), &bad_key)
                .await
                .is_err(),
            "unentitled key cannot even open the job"
        );
        user.open_database_with_key(db.root_id(), &read_key)
            .await
            .unwrap()
    };
    assert_eq!(
        read_db.current_permission().await.unwrap(),
        eidetica::auth::types::Permission::Read
    );
    // DEFERRED READ-ONLY CACHE REPRODUCER: bypass only Chaz's new observation
    // preflight, not Eidetica auth. Retain the original native query failure;
    // success here requires revisiting this assertion and true Read support.
    let raw_read = async {
        read_db
            .new_transaction()
            .await?
            .get_store::<eidetica::store::Table<JobInputRequest>>("job_inputs")
            .await?
            .search(|_: &JobInputRequest| true)
            .await
    }
    .await;
    let deferred_error = raw_read.unwrap_err().to_string();
    eprintln!("DEFERRED Read-only cache materialization (not repaired): {deferred_error}");
    assert!(deferred_error.contains("PermissionDenied") && deferred_error.contains("Write(0)"));
    let refusal = crate::session::jobs::observer_status(&read_db)
        .await
        .unwrap_err()
        .to_string();
    assert!(refusal.contains("insufficient permission") && refusal.contains("Write authority"));
    let reader = Session::new(ConversationId(id.clone()), read_db).await;
    assert!(
        reader
            .job_inputs()
            .await
            .unwrap_err()
            .to_string()
            .contains("insufficient permission")
    );
    assert!(
        reader
            .submit_job_input(JobInputRequest::new(
                attempt_id.clone(),
                "not authorized".into()
            ))
            .await
            .unwrap_err()
            .to_string()
            .contains("insufficient permission")
    );

    assert!(
        crate::session::jobs::read_job_result(&db)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        mock.recorded_calls().len(),
        1,
        "observation/input must not run another executor or interrupt the current call"
    );
    assert_eq!(observer.job_inputs(&id).await.unwrap().len(), 1);
    if let Some(sequence) = restart_at_call {
        let gate = if sequence == 1 {
            let continuation = mock.block_next_call();
            gate.release();
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                continuation.wait_started(),
            )
            .await
            .unwrap();
            assert!(
                crate::session::jobs::read_job_result(&db)
                    .await
                    .unwrap()
                    .is_none(),
                "parent cannot receive the provisional answer while accepted input is in flight"
            );
            assert_eq!(
                observer.job_inputs(&id).await.unwrap()[0].state,
                JobInputState::Dispatching { model_sequence: 1 }
            );
            continuation
        } else {
            gate
        };
        executor.shutdown().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.wait_stopped())
            .await
            .unwrap();
        let (restarted, next_mock) = published_executor_with_mock(&settings, agents, agent).await;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !restarted.is_watching_session(&id).await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            restarted.job_status(&id).await.unwrap().state,
            JobState::Interrupted { .. }
        ));
        assert!(
            next_mock.recorded_calls().is_empty(),
            "restart cannot replay a queued input or model effect"
        );
        restarted
            .retry_interrupted_turn(
                &id,
                &crate::session::TurnRequestId::parse(acceptance.directive_id),
            )
            .await
            .unwrap();
        assert!(
            client
                .wait_job(&id, std::time::Duration::from_secs(14))
                .await
                .unwrap()
                .state
                .is_terminal()
        );
        assert_eq!(next_mock.recorded_calls().len(), 1);
        assert!(
            !next_mock.recorded_calls()[0]
                .messages
                .iter()
                .any(|m| matches!(m, RuntimeMessage::User(text) if text == &input.text))
        );
        let input_state = observer.job_inputs(&id).await.unwrap()[0].state.clone();
        if sequence == 1 {
            assert!(
                matches!(input_state, JobInputState::Uncertain { .. }),
                "dispatch without a response must never claim inclusion after restart"
            );
        } else {
            assert!(matches!(
                input_state,
                JobInputState::NotApplied { .. } | JobInputState::Uncertain { .. }
            ));
        }
        restarted.shutdown().await;
        drop(shutdown);
        task.await.unwrap().unwrap();
        return;
    }
    let mut child_wait = None;
    if wait_graph {
        let permits = executor
            .semaphore
            .clone()
            .try_acquire_many_owned(9)
            .unwrap();
        let child = client
            .submit_agent_job(&id, "default", "referenced queued child")
            .await
            .unwrap();
        // Provision this newly published fixture job explicitly too; no
        // observer key acquisition or delegated session discovery is implied.
        let (_, child_db) = registry.open_job_session(&child).await.unwrap();
        let txn = child_db.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(
                &observer_registry
                    .user_for_tests()
                    .await
                    .get_default_key()
                    .unwrap(),
                eidetica::auth::AuthKey::active(
                    Some("write-observer"),
                    eidetica::auth::types::Permission::Write(0),
                ),
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
        mock.push_tool_calls([(
            "wait".into(),
            "job_wait".into(),
            serde_json::json!({"session_db_id": child, "timeout_seconds": 2}).to_string(),
        )]);
        // Upstream yields the parent's slot inside job_wait: the child calls
        // before the parent can reacquire and request its terminal answer.
        mock.push_text("child terminal");
        mock.push_text("continued terminal");
        assert!(
            matches!(
                observer.job_status(&child).await.unwrap().state,
                JobState::Pending | JobState::Queued
            ),
            "child must be pending/queued before the parent yields"
        );
        // Before job_wait records its reference, an unclaimed child need not
        // be discovered. If already projected, it still must not be started.
        let graph = observer.job_monitor().await.unwrap();
        for node in graph
            .nodes
            .iter()
            .filter(|node| node.session_db_id == child)
        {
            assert!(
                (node.state == Some(JobState::Pending) && !node.claimed)
                    || node.state == Some(JobState::Queued),
                "pending work is not a running job: {graph:#?}"
            );
        }
        assert_eq!(
            mock.recorded_calls().len(),
            1,
            "child cannot run before the parent yields"
        );
        child_wait = Some((child, permits));
    }
    let continuation = mock.block_next_call();
    let child_gate = child_wait.as_ref().map(|_| mock.block_next_call());
    gate.release();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        continuation.wait_started(),
    )
    .await
    .unwrap();
    assert!(
        crate::session::jobs::read_job_result(&db)
            .await
            .unwrap()
            .is_none(),
        "parent cannot receive a provisional result before the continuation finishes"
    );
    assert_eq!(
        observer.job_inputs(&id).await.unwrap()[0].state,
        JobInputState::Dispatching { model_sequence: 1 }
    );
    assert_eq!(
        mock.recorded_calls()[1]
            .messages
            .iter()
            .filter(|m| matches!(m, RuntimeMessage::User(text) if text == &input.text))
            .count(),
        1
    );
    continuation.release();
    if let Some((child, _)) = &child_wait {
        let child_gate = child_gate.as_ref().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), child_gate.wait_started())
            .await
            .expect("child must acquire the parent's yielded slot");
        assert!(mock.recorded_calls()[2].messages.iter().any(|message|
            matches!(message, RuntimeMessage::User(text) if text == "referenced queued child")));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let graph = observer.job_monitor().await.unwrap();
                let parent = graph
                    .nodes
                    .iter()
                    .find(|node| node.session_db_id == id)
                    .unwrap();
                if parent.waits.iter().any(|wait| {
                    wait.child_id == *child && !wait.finished && wait.attempt_id == attempt_id
                }) {
                    assert!(
                        graph.nodes.iter().any(|node| node.session_db_id == *child
                            && node.claimed
                            && matches!(node.state, Some(JobState::StartedUnknown { .. }))),
                        "child with an actual blocked model call must be claimed/started: {graph:#?}"
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actual bounded child wait must be observable before it finishes");
        child_gate.release();
    }
    let terminal = client
        .wait_job(&id, std::time::Duration::from_secs(14))
        .await
        .unwrap();
    assert_eq!(
        terminal.state,
        JobState::Succeeded {
            text: Some("continued terminal".into())
        }
    );
    let calls: Vec<_> =
        mock.recorded_calls()
            .into_iter()
            .filter(|call| {
                call.messages.iter().any(|message|
            matches!(message, RuntimeMessage::User(text) if text == "original task"))
            })
            .collect();
    assert_eq!(calls.len(), if wait_graph { 3 } else { 2 });
    assert!(
        !calls[0]
            .messages
            .iter()
            .any(|m| matches!(m, RuntimeMessage::User(text) if text == &input.text))
    );
    assert_eq!(
        calls[1]
            .messages
            .iter()
            .filter(|m| matches!(m, RuntimeMessage::User(text) if text == &input.text))
            .count(),
        1
    );
    if tool_exchange {
        let steering_index = calls[1]
            .messages
            .iter()
            .position(|m| matches!(m, RuntimeMessage::User(text) if text == &input.text))
            .unwrap();
        assert!(
            matches!(calls[1].messages[steering_index - 1], RuntimeMessage::ToolResult { ref call_id, .. } if call_id == "calc"),
            "steering must follow the complete native exchange"
        );
        assert_eq!(
            calls[0].tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
            calls[1].tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    } else {
        assert!(calls.iter().all(|c| c.tools.is_empty()));
    }
    assert_eq!(calls[0].model, calls[1].model);
    assert_eq!(
        crate::session::jobs::read_accepted_job(&db).await.unwrap(),
        Some(acceptance)
    );
    let receipt = crate::session::jobs::read_job_result(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.attempt_id, attempt_id);
    assert_eq!(
        observer.job_inputs(&id).await.unwrap()[0].state,
        JobInputState::Included { model_sequence: 1 }
    );
    observer.submit_job_input(&id, input.clone()).await.unwrap();
    assert!(
        observer
            .submit_job_input(
                &id,
                JobInputRequest {
                    id: uuid::Uuid::new_v4().to_string(),
                    ..input
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        crate::session::jobs::read_job_result(&db).await.unwrap(),
        Some(receipt)
    );
    let session = Session::new(ConversationId(id.clone()), db.clone()).await;
    assert_eq!(
        session
            .entries()
            .iter()
            .filter(|e| e.entry_type == EntryType::Directive)
            .count(),
        1
    );
    assert!(
        !session
            .entries()
            .iter()
            .any(|e| e.sender == "submitter" && e.entry_type == EntryType::Message)
    );
    assert_eq!(session.attempts_for_test().await.len(), 1);
    assert!(
        executor
            .validate_accepted_job(session.database())
            .await
            .unwrap()
            .is_some()
    );
    assert!(!client.is_watching_session(&id).await);
    // A signed reference alone cannot authorize reading another database.
    // Keep the failed source explicit alongside the readable local job.
    agent_db
        .register_executor(crate::agent_db::ExecutorRef {
            peer_pubkey: "unavailable-reference".into(),
            db_id: parent_id.0.clone(),
        })
        .await
        .unwrap();
    let graph = observer.job_monitor().await.unwrap();
    assert!(
        graph
            .sources
            .iter()
            .any(|source| source.contains("unavailable-reference")
                && source.contains("unavailable")
                && source.contains("remote fetch is not performed"))
    );
    assert!(
        graph
            .nodes
            .iter()
            .any(|node| node.session_db_id == id && node.claimed)
    );
    assert_eq!(
        mock.recorded_calls()
            .iter()
            .filter(|call| call.messages.iter().any(
                |message| matches!(message, RuntimeMessage::User(text) if text == "original task")
            ))
            .count(),
        calls.len(),
        "refresh must not execute another parent turn; the real child may run after parent settlement"
    );
    assert!(!observer.is_watching_session(&id).await);
    assert!(
        graph
            .nodes
            .iter()
            .any(|node| node.session_db_id == parent_id.0
                && node
                    .unavailable
                    .as_deref()
                    .is_some_and(|error| error.contains("cannot verify existing Write authority"))),
        "an unsupported delegated service session query must be an explicit gap, not a panic"
    );
    // A warm cache cannot hide an Executor delegation narrowed to Read.
    let (executor_db, _) = registry
        .ensure_executor_db(&agent.db_id, &agent.pubkey)
        .await
        .unwrap();
    let original_delegation = executor_db
        .get_settings()
        .await
        .unwrap()
        .auth_snapshot()
        .await
        .unwrap()
        .get_delegated_tree(agent_db.database().root_id())
        .unwrap();
    let mut narrowed = original_delegation.clone();
    narrowed.permission_bounds.max = eidetica::auth::types::Permission::Read;
    let txn = executor_db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .add_delegated_tree(narrowed)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let graph = observer.job_monitor().await.unwrap();
    assert!(
        graph
            .sources
            .iter()
            .any(|source| source.contains("insufficient permission")
                && source.contains("Executor DB"))
    );
    let txn = executor_db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .add_delegated_tree(original_delegation)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    // Revocation/reduced authority on refresh stays an explicit gap even for
    // a completed job, never an empty successful transcript or cached grant.
    let txn = db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_auth_key(
            &observer_registry
                .user_for_tests()
                .await
                .get_default_key()
                .unwrap(),
            eidetica::auth::AuthKey::active(
                Some("reduced-observer"),
                eidetica::auth::types::Permission::Read,
            ),
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let refused = observer_registry
        .open_job_session(&id)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("insufficient permission"));
    let graph = observer.job_monitor().await.unwrap();
    let gap = graph
        .nodes
        .iter()
        .find(|node| node.session_db_id == id)
        .unwrap();
    assert!(gap.claimed && gap.preview.is_empty());
    assert!(
        gap.unavailable
            .as_deref()
            .unwrap()
            .contains("insufficient permission")
    );
    let txn = agent_db.database().new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_auth_key(
            &observer_registry
                .user_for_tests()
                .await
                .get_default_key()
                .unwrap(),
            eidetica::auth::AuthKey::active(
                Some("reduced-observer"),
                eidetica::auth::types::Permission::Read,
            ),
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let graph = observer.job_monitor().await.unwrap();
    assert!(
        graph
            .sources
            .iter()
            .any(|source| source.contains("insufficient permission"))
    );
    wait_slot.clear();
    executor.shutdown().await;
    drop(child_wait);
    drop(shutdown);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn service_job_steering_tool_free_drains_before_terminal() {
    Box::pin(service_steering(false, None, false, false)).await;
}

#[tokio::test]
async fn service_job_steering_follows_complete_native_tool_exchange() {
    Box::pin(service_steering(true, None, false, false)).await;
}

#[tokio::test]
async fn service_job_steering_restart_never_replays_input() {
    Box::pin(service_steering(false, Some(0), false, false)).await;
}

#[tokio::test]
async fn service_job_steering_restart_preserves_dispatch_uncertainty() {
    Box::pin(service_steering(false, Some(1), false, false)).await;
}

#[tokio::test]
async fn service_job_steering_close_race_refuses_late_publication() {
    Box::pin(service_steering(false, None, true, false)).await;
}

#[tokio::test]
async fn service_job_steering_monitor_records_actual_child_wait() {
    Box::pin(service_steering(true, None, false, true)).await;
}
