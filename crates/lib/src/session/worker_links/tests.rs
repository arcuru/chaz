use super::*;
use crate::session::{TurnAttemptStatus, TurnCallIdentity};
use crate::types::ConversationId;
use serde_json::{Value, json};

fn entry(kind: EntryType, body: &str) -> SessionEntry {
    SessionEntry {
        sender: "chaz".into(),
        content: body.into(),
        timestamp: chrono::Utc::now(),
        entry_type: kind,
        metadata: None,
        routing: None,
    }
}

fn call() -> WorkerCallSite {
    WorkerCallSite {
        request_id: TurnRequestId::parse("parent-request"),
        attempt_id: "parent-attempt".into(),
        execution_id: None,
        agent: "chaz".into(),
        model_sequence: 0,
        call_index: 0,
        call_id: "fixture-call".into(),
    }
}

async fn fixture() -> (eidetica::Instance, Database, Database, WorkerSpawn) {
    let (instance, mut user, parent) = crate::session::test_helpers::test_session_db().await;
    let key = user.get_default_key().unwrap();
    let child = user
        .create_database(eidetica::crdt::Doc::new(), &key)
        .await
        .unwrap();
    Session::initialize_turn_schema(&child).await.unwrap();
    let directive_id = Session::prepare_worker_directive(
        &child,
        &parent,
        "fixture-spawn",
        entry(EntryType::Directive, "submitted assignment"),
        true,
    )
    .await
    .unwrap();
    let session = Session::new(ConversationId(parent.root_id().to_string()), parent.clone()).await;
    session
        .append_turn_transcript(
            TurnTranscriptRecord {
                request_id: call().request_id,
                attempt_id: call().attempt_id,
                sequence: 0,
                timestamp: chrono::Utc::now(),
                identity: Some(TurnCallIdentity {
                    agent: "chaz".into(),
                    requested_model: "stub".into(),
                    execution_id: None,
                }),
                message: TurnTranscriptMessage::ModelResponse {
                    model_sequence: 0,
                    content: Some("parent prose".into()),
                    tool_calls: vec![crate::runtime::ToolCallRequest {
                        id: "fixture-call".into(),
                        name: "spawn_worker".into(),
                        arguments: json!({"name":"helper","task":"requested assignment"})
                            .to_string(),
                    }],
                    provider_extra: Default::default(),
                    metadata: None,
                    terminal: false,
                },
            },
            vec![],
        )
        .await
        .unwrap();
    let spawn = WorkerSpawn {
        version: 1,
        spawn_id: "fixture-spawn".into(),
        call: call(),
        child_session_id: child.root_id().to_string(),
        directive_id,
    };
    (instance, parent, child, spawn)
}

#[test]
fn worker_reference_wire_fixture_roundtrips_exactly() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../fixtures/worker-links.snap")).unwrap();
    for row in rows {
        let value = row["value"].clone();
        let actual = match row["store"].as_str().unwrap() {
            SPAWNS => {
                serde_json::to_value(serde_json::from_value::<WorkerSpawn>(value.clone()).unwrap())
                    .unwrap()
            }
            ORIGIN => {
                serde_json::to_value(serde_json::from_value::<WorkerOrigin>(value.clone()).unwrap())
                    .unwrap()
            }
            OUTCOMES => serde_json::to_value(
                serde_json::from_value::<WorkerOutcome>(value.clone()).unwrap(),
            )
            .unwrap(),
            _ => panic!("unknown fixture store"),
        };
        assert_eq!(actual, value);
    }
}

#[tokio::test]
async fn worker_links_validate_exact_call_and_backlink_without_copying_payloads() {
    let (_instance, parent, child, spawn) = fixture().await;
    assert!(worker_spawns(&parent).await.unwrap().is_empty());
    assert!(
        worker_origin(&parent).await.unwrap().is_none(),
        "legacy/root is unknown, not inferred"
    );
    publish_spawn(
        &parent,
        &child,
        &spawn.spawn_id,
        spawn.directive_id.clone(),
        spawn.call.clone(),
    )
    .await
    .unwrap();
    publish_spawn(
        &parent,
        &child,
        &spawn.spawn_id,
        spawn.directive_id.clone(),
        spawn.call.clone(),
    )
    .await
    .unwrap();
    let rows = worker_spawns(&parent).await.unwrap();
    assert_eq!(rows.as_slice(), std::slice::from_ref(&spawn));
    let mut user = _instance
        .login_account("test", "test-password")
        .await
        .unwrap();
    let key = user.get_default_key().unwrap();
    let repeated = user
        .create_database(eidetica::crdt::Doc::new(), &key)
        .await
        .unwrap();
    let repeated_directive = Session::prepare_worker_directive(
        &repeated,
        &parent,
        "another-creation",
        entry(EntryType::Directive, "replayed assignment"),
        true,
    )
    .await
    .unwrap();
    assert!(
        publish_spawn(
            &parent,
            &repeated,
            "another-creation",
            repeated_directive,
            call()
        )
        .await
        .is_err(),
        "the same exact invocation cannot create a second linked child"
    );
    assert_eq!(
        verify_worker_spawn(&parent, &child, &rows[0])
            .await
            .unwrap()
            .content,
        "submitted assignment"
    );
    for bad_call in [
        WorkerCallSite {
            call_index: 1,
            ..call()
        },
        WorkerCallSite {
            model_sequence: 1,
            ..call()
        },
        WorkerCallSite {
            attempt_id: "other".into(),
            ..call()
        },
        WorkerCallSite {
            execution_id: Some("other".into()),
            ..call()
        },
        WorkerCallSite {
            agent: "other".into(),
            ..call()
        },
    ] {
        let bad = WorkerSpawn {
            call: bad_call,
            ..spawn.clone()
        };
        // Corrupt the persisted relation too: the exact-call check must reject
        // this, not merely notice that the caller supplied an unrecorded row.
        let txn = parent.new_transaction().await.unwrap();
        txn.get_store::<Table<WorkerSpawn>>(SPAWNS)
            .await
            .unwrap()
            .set(&spawn.spawn_id, bad.clone())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert!(verify_worker_spawn(&parent, &child, &bad).await.is_err());
    }
    let txn = parent.new_transaction().await.unwrap();
    txn.get_store::<Table<WorkerSpawn>>(SPAWNS)
        .await
        .unwrap()
        .set(&spawn.spawn_id, spawn.clone())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let bad = WorkerSpawn {
        spawn_id: "forged".into(),
        ..spawn.clone()
    };
    assert!(verify_worker_spawn(&parent, &child, &bad).await.is_err());
    let bad = WorkerSpawn {
        version: 2,
        ..spawn.clone()
    };
    assert!(verify_worker_spawn(&parent, &child, &bad).await.is_err());
    // A different invocation cannot reuse an already published creation ID.
    let conflict = publish_spawn(
        &parent,
        &child,
        &spawn.spawn_id,
        TurnRequestId::parse("absent"),
        call(),
    )
    .await;
    assert!(conflict.is_err());
    assert_eq!(worker_spawns(&parent).await.unwrap(), [spawn]);
}

#[tokio::test]
async fn worker_completion_references_exact_final_and_rolls_back_duplicate_settlement() {
    let (_instance, _parent, child, spawn) = fixture().await;
    let mut session =
        Session::new(ConversationId(child.root_id().to_string()), child.clone()).await;
    let evidence = worker_evidence(&child, &_parent.root_id().to_string(), &spawn)
        .await
        .unwrap();
    assert_eq!(
        evidence.status(chrono::Utc::now()),
        "not started; no attempt recorded"
    );
    let attempt = session
        .start_turn_attempt(spawn.directive_id.clone())
        .await
        .unwrap();
    let evidence = worker_evidence(&child, &_parent.root_id().to_string(), &spawn)
        .await
        .unwrap();
    assert_eq!(
        evidence.status(attempt.started_at),
        "activity recorded; no terminal result"
    );
    assert_eq!(
        evidence.status(attempt.started_at + chrono::Duration::minutes(2)),
        "outcome unknown"
    );
    assert!(
        worker_outcome(&child, &attempt.attempt_id, None)
            .await
            .unwrap()
            .is_none()
    );
    session
        .complete_turn_attempt(&attempt, Some(entry(EntryType::Message, "actual reply")))
        .await
        .unwrap();
    // A later unrelated row must never become this Worker's result.
    session
        .add_entry(entry(EntryType::Message, "unrelated newer message"))
        .await
        .unwrap();
    let (outcome, final_entry) = worker_outcome(&child, &attempt.attempt_id, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.directive_id, spawn.directive_id);
    assert_eq!(outcome.terminal, WorkerTerminal::Reply);
    assert_eq!(final_entry.unwrap().content, "actual reply");
    let evidence = worker_evidence(&child, &_parent.root_id().to_string(), &spawn)
        .await
        .unwrap();
    assert_eq!(evidence.status(chrono::Utc::now()), "completed");
    assert_eq!(evidence.result.unwrap().content, "actual reply");
    assert!(
        worker_outcome(&child, &attempt.attempt_id, Some("wrong-branch"))
            .await
            .unwrap()
            .is_none()
    );
    let before = session.entries().len();
    assert!(
        session
            .complete_turn_attempt(&attempt, Some(entry(EntryType::Message, "duplicate")))
            .await
            .is_err()
    );
    let fresh = Session::try_new(ConversationId(child.root_id().to_string()), child.clone())
        .await
        .unwrap();
    assert_eq!(
        fresh.entries().len(),
        before,
        "failed completion must not leak its final row"
    );
    assert_eq!(
        fresh.attempts_for_test().await[0].status,
        TurnAttemptStatus::Completed
    );
    assert_eq!(
        worker_outcome(&child, &attempt.attempt_id, None)
            .await
            .unwrap()
            .unwrap()
            .1
            .unwrap()
            .content,
        "actual reply"
    );
    let retry = session
        .start_turn_attempt(spawn.directive_id.clone())
        .await
        .unwrap();
    assert!(
        latest_worker_outcome(&child, &spawn.directive_id)
            .await
            .unwrap()
            .is_none(),
        "a newer unfinished attempt must not inherit the older reply"
    );
    session
        .complete_turn_attempt(&retry, Some(entry(EntryType::Message, "retry reply")))
        .await
        .unwrap();
    assert_eq!(
        latest_worker_outcome(&child, &spawn.directive_id)
            .await
            .unwrap()
            .unwrap()
            .1
            .unwrap()
            .content,
        "retry reply"
    );
    assert_eq!(
        worker_outcome(&child, &attempt.attempt_id, None)
            .await
            .unwrap()
            .unwrap()
            .1
            .unwrap()
            .content,
        "actual reply"
    );
}

#[tokio::test]
async fn worker_terminal_evidence_distinguishes_stop_failure_no_reply_and_broken_reference() {
    for (terminal, final_entry, cancelled) in [
        (
            WorkerTerminal::Failed,
            Some(entry(EntryType::Error, "failure")),
            false,
        ),
        (
            WorkerTerminal::Cancelled,
            Some(entry(EntryType::Error, "stopped")),
            true,
        ),
        (WorkerTerminal::NoReply, None, false),
    ] {
        let (_instance, _parent, child, spawn) = fixture().await;
        let mut session =
            Session::new(ConversationId(child.root_id().to_string()), child.clone()).await;
        let attempt = session
            .start_turn_attempt(spawn.directive_id.clone())
            .await
            .unwrap();
        // A stop request is not cancellation acknowledgement or terminal evidence.
        session.request_stop("user").await.unwrap();
        assert_eq!(
            worker_evidence(&child, &_parent.root_id().to_string(), &spawn)
                .await
                .unwrap()
                .status(chrono::Utc::now()),
            "stop request recorded; outcome not recorded"
        );
        assert!(
            worker_outcome(&child, &attempt.attempt_id, None)
                .await
                .unwrap()
                .is_none()
        );
        let transcript = cancelled.then(|| TurnTranscriptRecord {
            request_id: attempt.request_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            sequence: 0,
            timestamp: chrono::Utc::now(),
            identity: None,
            message: TurnTranscriptMessage::Cancelled,
        });
        if cancelled {
            assert!(
                session
                    .complete_turn_attempt_with_transcript(&attempt, None, transcript.clone())
                    .await
                    .is_err(),
                "cancellation needs the actual acknowledged final, not only a marker"
            );
            assert!(
                worker_outcome(&child, &attempt.attempt_id, None)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        session
            .complete_turn_attempt_with_transcript(&attempt, final_entry, transcript)
            .await
            .unwrap();
        let (mut outcome, _) = worker_outcome(&child, &attempt.attempt_id, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.terminal, terminal);
        let evidence = worker_evidence(&child, &_parent.root_id().to_string(), &spawn)
            .await
            .unwrap();
        assert_eq!(
            evidence.state,
            match terminal {
                WorkerTerminal::Failed => "failed",
                WorkerTerminal::Cancelled => "cancelled (acknowledged)",
                WorkerTerminal::NoReply => "completed without reply",
                _ => unreachable!(),
            }
        );
        outcome.final_entry_id = Some(TurnRequestId::parse("missing-final"));
        let txn = child.new_transaction().await.unwrap();
        txn.get_store::<Table<WorkerOutcome>>(OUTCOMES)
            .await
            .unwrap()
            .set(outcome_key(&attempt.attempt_id, None), outcome)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert!(
            worker_outcome(&child, &attempt.attempt_id, None)
                .await
                .is_err(),
            "missing referenced payload is not an empty success"
        );
        assert!(
            worker_evidence(&child, &_parent.root_id().to_string(), &spawn)
                .await
                .is_err()
        );
        let txn = child.new_transaction().await.unwrap();
        txn.get_store::<Table<WorkerOutcome>>(OUTCOMES)
            .await
            .unwrap()
            .delete(&outcome_key(&attempt.attempt_id, None))
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(
            worker_evidence(&child, &_parent.root_id().to_string(), &spawn)
                .await
                .unwrap()
                .state,
            "completed attempt; result unavailable"
        );
    }
}

#[tokio::test]
async fn worker_unknown_version_is_unavailable_not_absent_and_legacy_meta_write_preserves_links() {
    let (_instance, parent, child, spawn) = fixture().await;
    publish_spawn(
        &parent,
        &child,
        &spawn.spawn_id,
        spawn.directive_id,
        spawn.call,
    )
    .await
    .unwrap();
    let session = Session::new(ConversationId(parent.root_id().to_string()), parent.clone()).await;
    session
        .update_meta(|meta| meta.name = Some("legacy-style full meta writer".into()))
        .await
        .unwrap();
    assert_eq!(worker_spawns(&parent).await.unwrap().len(), 1);
    let txn = child.new_transaction().await.unwrap();
    txn.get_store::<Table<Value>>(ORIGIN).await.unwrap().set("origin", json!({"version":2,"parent_session_id":parent.root_id().to_string(),"spawn_id":"fixture-spawn"})).await.unwrap();
    txn.commit().await.unwrap();
    assert!(worker_origin(&child).await.is_err());
    assert!(
        worker_spawns(&parent).await.is_ok(),
        "the reference survives unavailable target metadata"
    );
}

#[tokio::test]
async fn worker_parent_sync_preserves_reference_without_acquiring_child() {
    use eidetica::{
        Instance, NewAccount,
        auth::{Permission, types::AuthKey},
        backend::database::InMemory,
        sync::{Address, transports::http::HttpTransport},
    };
    let (owner, parent, child, spawn) = fixture().await;
    publish_spawn(
        &parent,
        &child,
        &spawn.spawn_id,
        spawn.directive_id.clone(),
        spawn.call.clone(),
    )
    .await
    .unwrap();
    owner.enable_sync().await.unwrap();
    let sync = owner.sync().unwrap();
    sync.register_transport("http", HttpTransport::builder().bind("127.0.0.1:0"))
        .await
        .unwrap();
    sync.accept_connections().await.unwrap();
    let address = Address::http(sync.get_server_address_for("http").await.unwrap());
    let (peer, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("peer", "test-password"),
    )
    .await
    .unwrap();
    peer.enable_sync().await.unwrap();
    let key = user.get_default_key().unwrap();
    let signing = user.get_signing_key(&key).unwrap();
    let ps = peer.sync().unwrap();
    ps.register_transport("http", HttpTransport::builder())
        .await
        .unwrap();
    // Explicit sharing in a disposable test only. A relation itself never
    // causes these grants, bootstraps or sync requests in the product.
    for source in [&parent, &child] {
        let txn = source.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_global_auth_key(AuthKey::active(None, Permission::Write(0)))
            .await
            .unwrap();
        txn.commit().await.unwrap();
        source.share().await.unwrap();
        ps.sync_with_peer_for_bootstrap_with_key(
            &address,
            source.root_id(),
            &signing,
            &key.to_string(),
            Permission::Write(10),
        )
        .await
        .unwrap();
        ps.flush().await.unwrap();
        let (identity, _) = Database::find_sigkeys(&peer, source.root_id(), &key)
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        user.map_key(&key, source.root_id(), identity)
            .await
            .unwrap();
        if source.root_id() == parent.root_id() {
            let available_parent = user.open_database(parent.root_id()).await.unwrap();
            assert_eq!(
                worker_spawns(&available_parent).await.unwrap().as_slice(),
                std::slice::from_ref(&spawn)
            );
            assert!(
                user.open_database(child.root_id()).await.is_err(),
                "learning a reference cannot acquire an unshared/unfetched child"
            );
        }
    }
    let parent_peer = user.open_database(parent.root_id()).await.unwrap();
    let child_peer = user.open_database(child.root_id()).await.unwrap();
    assert_eq!(
        verify_worker_spawn(&parent_peer, &child_peer, &spawn)
            .await
            .unwrap()
            .content,
        "submitted assignment"
    );
    assert!(
        worker_outcome(&child_peer, "unknown-attempt", None)
            .await
            .unwrap()
            .is_none()
    );
}
