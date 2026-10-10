use super::*;
use crate::session::{Session, TurnTranscriptMessage, test_helpers::make_registry};

fn branch(attempt: &TurnAttempt, id: &str, executor: &str) -> ExecutionStart {
    ExecutionStart {
        version: VERSION,
        id: id.into(),
        executor: ExecutorIdentity {
            id: executor.into(),
            peer: "home".into(),
        },
        incarnation: format!("process-{id}"),
        attempt: attempt.clone(),
        retry_command: None,
        continuation: None,
    }
}

fn record(attempt: &TurnAttempt, text: &str) -> TurnTranscriptRecord {
    TurnTranscriptRecord {
        request_id: attempt.request_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        sequence: 0,
        timestamp: chrono::Utc::now(),
        message: TurnTranscriptMessage::ModelResponse {
            model_sequence: 0,
            content: Some(text.into()),
            tool_calls: vec![],
            provider_extra: Default::default(),
            metadata: None,
            terminal: true,
        },
    }
}

async fn finish(db: &Database, execution: &ExecutionStart, text: &str, delivered: bool) {
    let txn = db.new_transaction().await.unwrap();
    outcome_in_txn(
        &txn,
        &ExecutionOutcome {
            execution_id: execution.id.clone(),
            result: super::super::jobs::JobResult {
                attempt_id: execution.attempt.attempt_id.clone(),
                execution_id: Some(execution.id.clone()),
                terminal: super::super::jobs::JobTerminal::Success {
                    text: Some(text.into()),
                },
            },
            delivered,
        },
        Some(&record(&execution.attempt, text)),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn participation_is_independent_idempotent_and_has_no_elapsed_time_writes() {
    let (_instance, registry) = make_registry().await;
    let (_, db) = registry
        .create_session(Some("participation"))
        .await
        .unwrap();
    activate_fresh(&db).await.unwrap();
    let first = job_executor("agent-db", "home");
    assert_eq!(job_executor("agent-db", "home"), first);
    let second = ExecutorIdentity {
        id: "another-executor".into(),
        peer: "home".into(),
    };
    participate(&db, &first).await.unwrap();
    participate(&db, &second).await.unwrap();
    let tips = db.snapshot().await.unwrap();
    for _ in 0..8 {
        participate(&db, &first).await.unwrap();
        participate(&db, &second).await.unwrap();
    }
    assert_eq!(
        db.snapshot().await.unwrap(),
        tips,
        "idempotent observation must not write ownership"
    );
    assert_eq!(participants(&db).await.unwrap().len(), 2);
    assert!(legacy_owner(&db).await.unwrap().is_none());
}

#[tokio::test]
async fn same_attempt_competitors_preserve_both_completion_orders_and_scoped_transcripts() {
    for reverse in [false, true] {
        let (_instance, registry) = make_registry().await;
        let (cid, db) = registry
            .create_session(Some("competing-outcomes"))
            .await
            .unwrap();
        activate_fresh(&db).await.unwrap();
        let session = Session::new(cid, db.clone()).await;
        let attempt = session
            .start_turn_attempt_as(TurnRequestId::parse("request"), "logical-attempt".into())
            .await
            .unwrap();
        let a = branch(&attempt, "a", "executor-a");
        let b = branch(&attempt, "b", "executor-b");
        start(&db, &a).await.unwrap();
        start(&db, &b).await.unwrap();
        let yielded = observe(&db, "b").await.unwrap().unwrap();
        assert_eq!(yielded.earlier_execution, "a");
        assert!(!yielded.continuation_handover);
        assert!(observe(&db, "a").await.unwrap().is_none());
        if reverse {
            finish(&db, &b, "late loser", false).await;
            finish(&db, &a, "winner", true).await;
        } else {
            finish(&db, &a, "winner", true).await;
            finish(&db, &b, "late loser", false).await;
        }
        assert_eq!(outcomes(&db).await.unwrap().len(), 2);
        let winner_transcript = view_transcript(&db, "a", "winner", 0, 10).await.unwrap();
        assert_eq!(winner_transcript.len(), 1);
        assert_eq!(
            winner_transcript[0].message,
            record(&attempt, "winner").message
        );
        assert_eq!(
            view_transcript(&db, "a", "late loser", 0, 10)
                .await
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            view_transcript(&db, "b", "late loser", 0, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            observe(&db, "b").await.unwrap().unwrap(),
            yielded,
            "yield is irreversible after outcomes"
        );
        assert_eq!(
            take_notices(&db, "a", true).await.unwrap().len(),
            0,
            "closing does not demand another call"
        );
        let notices = take_notices(&db, "a", false).await.unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("executor-b") && notices[0].contains("job_status"));
        assert!(take_notices(&db, "a", false).await.unwrap().is_empty());
        assert!(view_transcript(&db, "absent", "", 0, 10).await.is_err());
        assert!(view_transcript(&db, "a", "", 0, 51).await.is_err());
    }
}

#[tokio::test]
async fn exact_continuation_is_not_a_competitor_but_parallel_resumes_yield() {
    let (_instance, registry) = make_registry().await;
    let (cid, db) = registry
        .create_session(Some("continuations"))
        .await
        .unwrap();
    activate_fresh(&db).await.unwrap();
    let session = Session::new(cid, db.clone()).await;
    let attempt = session
        .start_turn_attempt_as(TurnRequestId::parse("request"), "logical-attempt".into())
        .await
        .unwrap();
    let root = branch(&attempt, "root", "executor-a");
    start(&db, &root).await.unwrap();
    let mut a = branch(&attempt, "resume-a", "executor-a");
    a.continuation = Some(Continuation {
        predecessor: "root".into(),
        checkpoint: "exact-checkpoint".into(),
    });
    let mut b = branch(&attempt, "resume-b", "executor-b");
    b.continuation = a.continuation.clone();
    start(&db, &a).await.unwrap();
    start(&db, &b).await.unwrap();
    assert!(observe(&db, "resume-a").await.unwrap().is_none());
    assert_eq!(
        observe(&db, "resume-b")
            .await
            .unwrap()
            .unwrap()
            .earlier_execution,
        "resume-a"
    );
    assert!(
        observe(&db, "root")
            .await
            .unwrap()
            .unwrap()
            .continuation_handover
    );
    let mut c = branch(&attempt, "resume-yielded", "executor-b");
    c.continuation = Some(Continuation {
        predecessor: "resume-b".into(),
        checkpoint: "same".into(),
    });
    assert!(
        start(&db, &c).await.is_err(),
        "yielded branches never revive"
    );
    let mut retry = branch(&attempt, "explicit-retry", "executor-b");
    retry.retry_command = Some("retry-command".into());
    start(&db, &retry).await.unwrap();
    assert!(observe(&db, "explicit-retry").await.unwrap().is_none());
}

#[tokio::test]
async fn post_delivery_discovery_appends_correction_without_mutating_the_receipt() {
    let (_instance, registry) = make_registry().await;
    let (cid, db) = registry
        .create_session(Some("late-discovery"))
        .await
        .unwrap();
    activate_fresh(&db).await.unwrap();
    let session = Session::new(cid, db.clone()).await;
    let attempt = session
        .start_turn_attempt_as(TurnRequestId::parse("request"), "logical-attempt".into())
        .await
        .unwrap();
    let a = branch(&attempt, "early", "executor-a");
    let b = branch(&attempt, "delivered", "executor-b");
    start(&db, &a).await.unwrap();
    start(&db, &b).await.unwrap();
    // A delayed observation permits effects/delivery: this is not fencing.
    finish(&db, &b, "already delivered", true).await;
    let before = outcomes(&db).await.unwrap();
    let before_transcript = transcript(&db, "delivered").await.unwrap();
    observe(&db, "delivered").await.unwrap().unwrap();
    let correction = notices(&db)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.correction)
        .unwrap();
    assert_eq!(correction.parallel_execution, "early");
    assert!(correction.text.contains("no rollback or parent replay"));
    assert_eq!(outcomes(&db).await.unwrap(), before);
    assert_eq!(
        transcript(&db, "delivered").await.unwrap(),
        before_transcript
    );
    observe(&db, "delivered").await.unwrap();
    assert_eq!(
        notices(&db)
            .await
            .unwrap()
            .iter()
            .filter(|n| n.correction)
            .count(),
        1
    );
}

#[tokio::test]
async fn native_start_order_ties_are_not_row_or_arrival_order() {
    let (_instance, registry) = make_registry().await;
    let (cid, db) = registry.create_session(Some("native-tie")).await.unwrap();
    activate_fresh(&db).await.unwrap();
    let session = Session::new(cid, db.clone()).await;
    let attempt = session
        .start_turn_attempt_as(TurnRequestId::parse("request"), "logical-attempt".into())
        .await
        .unwrap();
    let a = branch(&attempt, "zzz", "executor-z");
    let b = branch(&attempt, "aaa", "executor-a");
    let snapshot = db.snapshot().await.unwrap();
    let ta = db.new_transaction_at(&snapshot).await.unwrap();
    let tb = db.new_transaction_at(&snapshot).await.unwrap();
    ta.get_store::<Table<ExecutionStart>>(STARTS)
        .await
        .unwrap()
        .set(&a.id, a.clone())
        .await
        .unwrap();
    tb.get_store::<Table<ExecutionStart>>(STARTS)
        .await
        .unwrap()
        .set(&b.id, b.clone())
        .await
        .unwrap();
    let id_b = tb.commit().await.unwrap();
    let id_a = ta.commit().await.unwrap();
    let native = store_entries_at(&db, STARTS, &db.snapshot().await.unwrap())
        .await
        .unwrap();
    assert_eq!(native.len(), 2);
    assert_eq!(
        native[0].subtree_height(STARTS).unwrap(),
        native[1].subtree_height(STARTS).unwrap()
    );
    assert_eq!(native[0].id(), std::cmp::min(id_a.clone(), id_b.clone()));
    let ordered = ordered_starts(&db).await.unwrap();
    assert_eq!(
        ordered
            .iter()
            .map(|s| &s.entry_id)
            .cloned()
            .collect::<Vec<_>>(),
        native
            .iter()
            .map(|e| e.id().to_string())
            .collect::<Vec<_>>()
    );
    let loser = &ordered[1].start.id;
    let winner = &ordered[0].start.id;
    assert_eq!(
        observe(&db, loser)
            .await
            .unwrap()
            .unwrap()
            .earlier_execution,
        *winner
    );
    assert!(observe(&db, winner).await.unwrap().is_none());
}

#[tokio::test]
async fn activation_requires_coordination_and_rejects_mixed_and_unknown_writers() {
    let (_instance, registry) = make_registry().await;
    let (cid, db) = registry
        .create_session(Some("legacy-boundary"))
        .await
        .unwrap();
    let session = Session::new(cid, db.clone()).await;
    session
        .start_turn_attempt_as(TurnRequestId::parse("legacy"), "legacy-attempt".into())
        .await
        .unwrap();
    assert!(activate_fresh(&db).await.is_err());
    assert!(require_active(&db).await.is_err());
    activate_after_coordinated_upgrade(&db).await.unwrap();
    require_active(&db).await.unwrap();
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<eidetica::store::DocStore>("job_owner")
        .await
        .unwrap()
        .set_string("v1", "legacy-write")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        require_active(&db)
            .await
            .unwrap_err()
            .to_string()
            .contains("legacy ownership")
    );
    assert!(
        activate_after_coordinated_upgrade(&db).await.is_err(),
        "must not acknowledge legacy writes by reactivation"
    );
    let (_, unknown) = registry
        .create_session(Some("unknown-protocol"))
        .await
        .unwrap();
    let txn = unknown.new_transaction().await.unwrap();
    txn.get_store::<Table<Activation>>(ACTIVATION)
        .await
        .unwrap()
        .set(
            "activation",
            Activation {
                version: VERSION + 1,
                legacy_owner: None,
            },
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        require_active(&unknown)
            .await
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );
}

async fn accept_job(db: &Database, directive: &TurnRequestId) {
    let accepted = super::super::jobs::AcceptedAgentJob {
        parent_id: "parent".into(),
        definition: super::super::jobs::StagedAgentDefinition {
            target: "default".into(),
            task: "task".into(),
            executor_pubkey: "peer".into(),
            call_depth: 1,
            max_call_depth: 4,
            allowed_tools: vec![],
            tool_ceilings: Default::default(),
            capability_ceiling: Default::default(),
        },
        directive_id: directive.as_str().into(),
    };
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<eidetica::store::DocStore>("job_acceptance")
        .await
        .unwrap()
        .set_string("v1", serde_json::to_string(&accepted).unwrap())
        .await
        .unwrap();
    txn.commit().await.unwrap();
}

fn reply(text: &str) -> crate::session::SessionEntry {
    crate::session::SessionEntry {
        sender: "default".into(),
        content: text.into(),
        timestamp: chrono::Utc::now(),
        entry_type: crate::session::EntryType::Message,
        metadata: None,
        routing: None,
    }
}

/// The receipt names the delivering branch and is never replaced. A branch
/// arriving second, in either completion order, keeps an undelivered outcome.
#[tokio::test]
async fn branch_receipt_is_never_replaced_and_late_results_stay_evidence() {
    for yielded_first in [false, true] {
        let (_instance, registry) = make_registry().await;
        let (cid, db) = registry
            .create_session(Some("branch-receipt"))
            .await
            .unwrap();
        activate_fresh(&db).await.unwrap();
        let mut session = Session::new(cid, db.clone()).await;
        let request = session
            .add_entry(crate::session::SessionEntry {
                entry_type: crate::session::EntryType::Directive,
                sender: "parent".into(),
                ..reply("task")
            })
            .await
            .unwrap();
        accept_job(&db, &request).await;
        let attempt = session
            .start_turn_attempt_as(request.clone(), "logical-attempt".into())
            .await
            .unwrap();
        let a = branch(&attempt, "a", "executor");
        let b = branch(&attempt, "b", "executor");
        start(&db, &a).await.unwrap();
        start(&db, &b).await.unwrap();
        assert_eq!(
            observe(&db, "b").await.unwrap().unwrap().earlier_execution,
            "a"
        );
        let late = ExecutionOutcome {
            execution_id: "b".into(),
            delivered: false,
            result: super::super::jobs::JobResult {
                attempt_id: attempt.attempt_id.clone(),
                execution_id: Some("b".into()),
                terminal: super::super::jobs::JobTerminal::Success {
                    text: Some("late".into()),
                },
            },
        };
        if yielded_first {
            record_undelivered(&db, &late).await.unwrap();
        }
        session
            .complete_turn_attempt_for_branch(
                &attempt,
                Some(reply("winner")),
                None,
                Some(record(&attempt, "winner")),
                Some("a"),
            )
            .await
            .unwrap();
        // A second completion (a racing branch that missed the yield) fails
        // closed instead of replacing the receipt.
        let err = session
            .complete_turn_attempt_for_branch(&attempt, Some(reply("late")), None, None, Some("b"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already settled"), "{err}");
        if !yielded_first {
            record_undelivered(&db, &late).await.unwrap();
        }
        let receipt = super::super::jobs::read_job_result(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.execution_id.as_deref(), Some("a"));
        assert_eq!(
            receipt.terminal,
            super::super::jobs::JobTerminal::Success {
                text: Some("winner".into())
            }
        );
        let mut all = outcomes(&db).await.unwrap();
        all.sort_by(|x, y| x.execution_id.cmp(&y.execution_id));
        assert_eq!(
            all.iter()
                .map(|o| (o.execution_id.as_str(), o.delivered))
                .collect::<Vec<_>>(),
            [("a", true), ("b", false)]
        );
        assert!(
            record_undelivered(
                &db,
                &ExecutionOutcome {
                    delivered: true,
                    ..late.clone()
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            view_transcript(&db, "a", "winner", 0, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            super::super::jobs::status_from_db(&db, &Default::default(), true)
                .await
                .unwrap()
                .state,
            super::super::jobs::JobState::Succeeded {
                text: Some("winner".into())
            }
        );
    }
}

/// A restart that resumes an exact checkpoint is lineage: the predecessor
/// (an older incarnation still alive) hands over instead of competing.
#[tokio::test]
async fn stale_incarnation_hands_over_to_its_exact_continuation() {
    let (_instance, registry) = make_registry().await;
    let (cid, db) = registry.create_session(Some("handover")).await.unwrap();
    activate_fresh(&db).await.unwrap();
    let session = Session::new(cid, db.clone()).await;
    let attempt = session
        .start_turn_attempt_as(TurnRequestId::parse("request"), "logical-attempt".into())
        .await
        .unwrap();
    start(&db, &branch(&attempt, "old-process", "executor"))
        .await
        .unwrap();
    assert_eq!(
        latest_branch(&db, &attempt.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        "old-process"
    );
    let mut resumed = branch(&attempt, "new-process", "executor");
    resumed.continuation = Some(Continuation {
        predecessor: "old-process".into(),
        checkpoint: "exact".into(),
    });
    start(&db, &resumed).await.unwrap();
    assert!(observe(&db, "new-process").await.unwrap().is_none());
    let handover = observe(&db, "old-process").await.unwrap().unwrap();
    assert!(handover.continuation_handover);
    assert_eq!(handover.earlier_execution, "new-process");
    assert!(
        notices(&db).await.unwrap().is_empty(),
        "lineage is not parallel work"
    );
}

/// A competitor committed between a branch and its exact continuation does
/// not outrank that lineage: the continuation inherits its root's order, so
/// exactly one lineage keeps running instead of every branch yielding.
#[tokio::test]
async fn continuation_keeps_its_lineage_order_over_an_interleaved_competitor() {
    for competitor_observes_first in [true, false] {
        let (_instance, registry) = make_registry().await;
        let (cid, db) = registry.create_session(Some("interleaved")).await.unwrap();
        activate_fresh(&db).await.unwrap();
        let session = Session::new(cid, db.clone()).await;
        let attempt = session
            .start_turn_attempt_as(TurnRequestId::parse("request"), "logical-attempt".into())
            .await
            .unwrap();
        start(&db, &branch(&attempt, "root", "executor-a"))
            .await
            .unwrap();
        start(&db, &branch(&attempt, "rival", "executor-b"))
            .await
            .unwrap();
        if competitor_observes_first {
            assert_eq!(
                observe(&db, "rival")
                    .await
                    .unwrap()
                    .unwrap()
                    .earlier_execution,
                "root"
            );
        }
        let mut resumed = branch(&attempt, "resumed", "executor-a");
        resumed.continuation = Some(Continuation {
            predecessor: "root".into(),
            checkpoint: "exact".into(),
        });
        start(&db, &resumed).await.unwrap();
        assert!(
            observe(&db, "resumed").await.unwrap().is_none(),
            "the earlier lineage continues"
        );
        assert!(
            observe(&db, "root")
                .await
                .unwrap()
                .unwrap()
                .continuation_handover
        );
        let rival = observe(&db, "rival").await.unwrap().unwrap();
        assert!(!rival.continuation_handover);
        assert_eq!(rival.earlier_execution, "root");
    }
}
