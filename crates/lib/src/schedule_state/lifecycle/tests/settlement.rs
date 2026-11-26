use super::*;
use crate::session::outcome::TurnOutcome;
use crate::session::scheduled::ScheduleExecutor;
use crate::session::steering::JobInputState;
use crate::session::{
    EntryType, SessionEntry, TurnAttempt, TurnAttemptStatus, TurnTranscriptMessage,
    TurnTranscriptRecord,
};

pub(super) async fn admitted(
    agent: &AgentDb,
    session: &mut Session,
    pinned: bool,
) -> (Occurrence, TurnAttempt) {
    admitted_with_clock(agent, session, pinned, false).await
}

async fn admitted_with_clock(
    agent: &AgentDb,
    session: &mut Session,
    pinned: bool,
    timed: bool,
) -> (Occurrence, TurnAttempt) {
    let mut schedule = definition();
    if pinned {
        schedule.target = ScheduleTarget::Pinned {
            session_db_id: session.database().root_id().to_string(),
        };
    }
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let row = if timed {
        reconcile_all(agent, &schedule).await.unwrap();
        agent
            .prepare_schedule_clock("wake", &schedule.revision_key().unwrap(), slot(1), now())
            .await
            .unwrap()
    } else {
        prepare(agent, "wake", slot(1), now()).await.unwrap()
    };
    let input = session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    handoff(agent, &row.reference, session, now())
        .await
        .unwrap();
    let request = if pinned {
        session
            .add_entry(SessionEntry {
                sender: "human".into(),
                content: "ordinary turn".into(),
                timestamp: now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap()
    } else {
        input.request_id
    };
    let attempt = session.start_turn_attempt(request).await.unwrap();
    session
        .bind_schedule_input(&row.reference, &attempt.attempt_id)
        .await
        .unwrap();
    (row, attempt)
}

pub(super) async fn included(session: &Session, row: &Occurrence, attempt: &TurnAttempt) {
    for state in [
        JobInputState::Accepted,
        JobInputState::Dispatching { model_sequence: 3 },
        JobInputState::Included { model_sequence: 3 },
    ] {
        session
            .record_schedule_input(&row.reference, &attempt.attempt_id, state)
            .await
            .unwrap();
    }
}

pub(super) async fn finish(session: &mut Session, attempt: &TurnAttempt, outcome: TurnOutcome) {
    let final_entry = (outcome != TurnOutcome::Success).then(|| SessionEntry {
        sender: "timer".into(),
        content: "deliberate failure or stop".into(),
        timestamp: now(),
        entry_type: EntryType::Error,
        metadata: None,
        routing: None,
    });
    let transcript = (outcome == TurnOutcome::Cancelled).then(|| TurnTranscriptRecord {
        request_id: attempt.request_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        sequence: 1,
        timestamp: now(),
        identity: None,
        message: TurnTranscriptMessage::Cancelled,
    });
    session
        .complete_turn_attempt_with_transcript(attempt, final_entry, transcript)
        .await
        .unwrap();
    assert_eq!(
        session
            .turn_completion(&attempt.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        outcome
    );
}

#[tokio::test]
async fn interruption_retry_ceiling_and_predecessor_survive_reopen() {
    let (_instance, user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.interruption_retries = std::num::NonZeroU32::new(1);
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let original = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    session
        .prepare_schedule_input(&original.reference)
        .await
        .unwrap();
    let original = handoff(&agent, &original.reference, &session, now())
        .await
        .unwrap();
    let first = ScheduleExecutor {
        home_key: "same home".into(),
        incarnation: uuid::Uuid::new_v4().to_string(),
    };
    let started = session
        .start_schedule_input_as(
            &original.reference,
            "attempt-one".into(),
            Some(first.clone()),
        )
        .await
        .unwrap()
        .unwrap();
    let evidence = settle(&agent, &original.reference, &session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retry::recover(&agent, &original, &evidence, &mut session, &first)
            .await
            .unwrap(),
        None
    );
    let second = ScheduleExecutor {
        incarnation: uuid::Uuid::new_v4().to_string(),
        ..first
    };
    let new = retry::recover(&agent, &original, &evidence, &mut session, &second)
        .await
        .unwrap()
        .expect("positive restart path");
    assert_eq!(
        (
            accounting(&agent).await.handoff_count,
            accounting(&agent).await.reservations
        ),
        (2, 1)
    );
    assert_eq!(
        retry::verify_interrupted_predecessor(&agent, &new, &session, &second)
            .await
            .unwrap(),
        Some(started.attempt_id)
    );
    session
        .start_schedule_input_as(&new, "attempt-two".into(), Some(second.clone()))
        .await
        .unwrap()
        .unwrap();
    let evidence = settle(&agent, &new, &session).await.unwrap().unwrap();

    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(user.open_database(operations.root_id()).await.unwrap());
    let mut session = Session::try_new(
        ConversationId("reopened".into()),
        user.open_database(session.database().root_id())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    *reopened.lock_scheduling().await = AccountingCache::default();
    reconcile_all(&reopened, &schedule).await.unwrap();
    let new_row = retry::delivery(&reopened, &new.occurrence_id, &new.delivery_id)
        .await
        .unwrap();
    let third = ScheduleExecutor {
        incarnation: uuid::Uuid::new_v4().to_string(),
        ..second
    };
    assert!(session.schedule_interruption(&evidence).await.unwrap());
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        retry::recover(&reopened, &new_row, &evidence, &mut session, &third)
            .await
            .unwrap(),
        None
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert_eq!(reopened.list_schedule_fires().await.unwrap().len(), 2);
}

#[tokio::test]
async fn explicit_retry_of_failed_fresh_reserves_once_and_reuses_bound_session() {
    let (_instance, user, agent, operations, mut session) = fixture().await;
    let (root, failed) = admitted_with_clock(&agent, &mut session, false, true).await;
    let definition = agent.find_schedule("wake").await.unwrap().unwrap();
    let cursor = agent.schedule_clock_cursor(&definition).await.unwrap();
    let reserved = operations.snapshot().await.unwrap();
    assert!(
        retry::explicit(
            &agent,
            &root.reference,
            &failed.attempt_id,
            &mut session,
            now()
        )
        .await
        .is_err()
    );
    assert_eq!(reserved, operations.snapshot().await.unwrap());

    included(&session, &root, &failed).await;
    finish(&mut session, &failed, TurnOutcome::Failure).await;
    settle(&agent, &root.reference, &session).await.unwrap();
    assert_eq!(accounting(&agent).await.reservations, 0);
    assert!(
        retry::explicit(
            &agent,
            &root.reference,
            "stale-attempt",
            &mut session,
            now()
        )
        .await
        .is_err()
    );
    let reference = retry::explicit(
        &agent,
        &root.reference,
        &failed.attempt_id,
        &mut session,
        now(),
    )
    .await
    .unwrap();
    assert_ne!(reference.delivery_id, root.reference.delivery_id);
    assert_eq!(reference.occurrence_id, root.reference.occurrence_id);
    assert_eq!(
        agent.schedule_clock_cursor(&definition).await.unwrap(),
        cursor
    );
    assert_eq!(
        (
            accounting(&agent).await.reservations,
            accounting(&agent).await.handoff_count
        ),
        (1, 2)
    );
    assert!(prepare(&agent, "wake", slot(2), now()).await.is_err());
    assert!(
        authorized_input(&agent, &root.reference, &session, now())
            .await
            .is_err()
    );

    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(user.open_database(operations.root_id()).await.unwrap());
    let mut session = Session::try_new(
        ConversationId("reopened".into()),
        user.open_database(session.database().root_id())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    // Force actual reconciliation even while the original test handle lives.
    let mut audit = accounting::AccountingCache::default();
    std::mem::swap(&mut audit, &mut *reopened.lock_scheduling().await);
    reconcile_all(&reopened, &definition).await.unwrap();
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        retry::explicit(
            &reopened,
            &root.reference,
            &failed.attempt_id,
            &mut session,
            now()
        )
        .await
        .unwrap(),
        reference
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let mut scan = HandoffScan::open(&reopened).await.unwrap().unwrap();
    let rows = scan.next_page(&reopened, 2).await.unwrap().unwrap();
    assert_eq!(rows[0].reference, root.reference);
    assert_eq!(rows[1].reference, reference);
    let retry_attempt = session
        .start_schedule_input_as(&reference, "retry-attempt".into(), None)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(retry_attempt.request_id, failed.request_id);
    included(&session, &rows[1], &retry_attempt).await;
    finish(&mut session, &retry_attempt, TurnOutcome::Success).await;
    settle(&reopened, &reference, &session).await.unwrap();
    settle(&reopened, &root.reference, &session).await.unwrap();
    let state = accounting(&reopened).await;
    assert_eq!(
        (
            state.successful_fires,
            state.reservations,
            state.handoff_count,
            state.fire_count
        ),
        (1, 0, 2, 8)
    );
    assert_eq!(
        session
            .turn_completion(&failed.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        TurnOutcome::Failure
    );
}

#[tokio::test]
async fn fresh_counts_only_durable_terminal_success_once_across_reopen() {
    let (_instance, user, agent, operations, mut session) = fixture().await;
    let (row, attempt) = admitted(&agent, &mut session, false).await;
    let definitions = agent.database().snapshot().await.unwrap();
    included(&session, &row, &attempt).await;
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    assert_eq!(
        (
            accounting(&agent).await.successful_fires,
            accounting(&agent).await.reservations
        ),
        (0, 1),
        "Included is not Fresh execution success"
    );
    assert!(
        prepare(&agent, "wake", slot(2), now()).await.is_err(),
        "uncertain/started last slot remains reserved"
    );
    finish(&mut session, &attempt, TurnOutcome::Success).await;
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    let state = accounting(&agent).await;
    assert_eq!(
        (
            state.legacy_baseline,
            state.successful_fires,
            state.reservations,
            state.handoff_count,
            state.fire_count
        ),
        (7, 1, 0, 1, 8)
    );
    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(user.open_database(operations.root_id()).await.unwrap());
    let reopened_session = Session::try_new(
        ConversationId("reopened".into()),
        user.open_database(session.database().root_id())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let before = operations.snapshot().await.unwrap();
    reopened
        .settle_reconciled_for_test(&row.reference, &reopened_session)
        .await
        .unwrap();
    assert_eq!(
        before,
        operations.snapshot().await.unwrap(),
        "lost projection reply cannot count twice"
    );
    assert!(
        prepare(&reopened, "wake", slot(2), now()).await.is_err(),
        "successful last slot is permanently spent"
    );
    assert_eq!(
        definitions,
        agent.database().snapshot().await.unwrap(),
        "no per-fire definition write"
    );
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
}

#[tokio::test]
async fn pinned_included_survives_failed_containing_turn_without_refund() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let (row, attempt) = admitted(&agent, &mut session, true).await;
    included(&session, &row, &attempt).await;
    let first = agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap()
        .unwrap();
    assert!(first.completion.is_none());
    assert_ne!(
        first.input.request_id, first.input.anchor.request_id,
        "busy input belongs to containing human request"
    );
    assert_eq!(
        (
            accounting(&agent).await.successful_fires,
            accounting(&agent).await.reservations
        ),
        (1, 0)
    );
    finish(&mut session, &attempt, TurnOutcome::Failure).await;
    let final_evidence = agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_evidence.completion.unwrap().outcome,
        TurnOutcome::Failure
    );
    assert_eq!(
        (
            accounting(&agent).await.successful_fires,
            accounting(&agent).await.reservations,
            accounting(&agent).await.fire_count
        ),
        (1, 0, 8)
    );
    assert!(
        session
            .record_schedule_input(
                &row.reference,
                &attempt.attempt_id,
                JobInputState::NotApplied {
                    reason: "late cleanup".into()
                }
            )
            .await
            .is_err()
    );
    let before = operations.snapshot().await.unwrap();
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert!(prepare(&agent, "wake", slot(2), now()).await.is_err());
}

#[tokio::test]
async fn failed_fresh_releases_only_its_reservation_and_never_rearms_one_shot() {
    for outcome in [TurnOutcome::Failure, TurnOutcome::Cancelled] {
        let (_instance, _user, agent, operations, mut session) = fixture().await;
        let (row, attempt) = admitted(&agent, &mut session, false).await;
        included(&session, &row, &attempt).await;
        finish(&mut session, &attempt, outcome).await;
        agent
            .settle_reconciled_for_test(&row.reference, &session)
            .await
            .unwrap();
        let state = accounting(&agent).await;
        assert_eq!(
            (
                state.successful_fires,
                state.reservations,
                state.handoff_count,
                state.fire_count
            ),
            (0, 0, 1, 7)
        );
        let next = prepare(&agent, "wake", slot(2), now()).await.unwrap();
        assert_ne!(row.reference, next.reference);
        let before = operations.snapshot().await.unwrap();
        agent
            .settle_reconciled_for_test(&row.reference, &session)
            .await
            .unwrap();
        assert_eq!(before, operations.snapshot().await.unwrap());
        assert_eq!(
            accounting(&agent).await.reservations,
            1,
            "old failure cannot refund another occurrence"
        );
    }
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.trigger = crate::routine::Trigger::OneShot { fire_at: now() };
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let row = prepare(&agent, "wake", DueSlot::Calendar { due_at: now() }, now())
        .await
        .unwrap();
    let input = session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    handoff(&agent, &row.reference, &session, now())
        .await
        .unwrap();
    let attempt = session.start_turn_attempt(input.request_id).await.unwrap();
    session
        .bind_schedule_input(&row.reference, &attempt.attempt_id)
        .await
        .unwrap();
    finish(&mut session, &attempt, TurnOutcome::Failure).await;
    // Explicit operator disable is independent from derived capacity release.
    schedule.enabled = false;
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    assert!(accounting(&agent).await.consumed);
    assert_eq!(accounting(&agent).await.fire_count, 7);
    assert_eq!(
        agent.find_schedule("wake").await.unwrap().unwrap(),
        schedule
    );
    assert!(release_prepared(&agent, &row.reference).await.is_err());
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    let before = operations.snapshot().await.unwrap();
    assert!(
        prepare(&agent, "wake", DueSlot::Calendar { due_at: now() }, now())
            .await
            .is_err(),
        "disable changed the revision; a consumed one-shot cannot rearm"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
}

#[tokio::test]
async fn missing_or_ambiguous_terminal_evidence_never_frees_capacity() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let row = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    let anchor = session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    handoff(&agent, &row.reference, &session, now())
        .await
        .unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(
        agent
            .settle_reconciled_for_test(&row.reference, &session)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let attempt = session.start_turn_attempt(anchor.request_id).await.unwrap();
    session
        .bind_schedule_input(&row.reference, &attempt.attempt_id)
        .await
        .unwrap();
    included(&session, &row, &attempt).await;
    // Older readers/writers can retain Completed without any typed outcome.
    let mut legacy_completed = attempt.clone();
    legacy_completed.status = TurnAttemptStatus::Completed;
    legacy_completed.completed_at = Some(now());
    put(
        session.database(),
        "turn_attempts",
        &attempt.attempt_id,
        serde_json::to_value(legacy_completed).unwrap(),
    )
    .await;
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    assert_eq!(
        (
            accounting(&agent).await.successful_fires,
            accounting(&agent).await.reservations
        ),
        (0, 1)
    );
    assert!(prepare(&agent, "wake", slot(2), now()).await.is_err());
    // A projection can arrive after replacement, but touches only the old lifetime.
    agent.remove_schedule("wake").await.unwrap();
    schedule.lifetime = Some("replacement".into());
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let replacement = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    finish(&mut session, &attempt, TurnOutcome::Failure).await;
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    assert_eq!(accounting(&agent).await.reservations, 1);
    assert_eq!(accounting(&agent).await.fire_count, 8);
    assert_eq!(
        agent.find_schedule("wake").await.unwrap().unwrap(),
        schedule
    );
    assert_ne!(replacement.reference.lifetime, row.reference.lifetime);
}

#[tokio::test]
async fn definite_unused_pinned_input_releases_but_dispatch_uncertainty_does_not() {
    let (_instance, _user, agent, _operations, mut session) = fixture().await;
    let (row, attempt) = admitted(&agent, &mut session, true).await;
    session
        .record_schedule_input(
            &row.reference,
            &attempt.attempt_id,
            JobInputState::NotApplied {
                reason: "boundary closed before dispatch".into(),
            },
        )
        .await
        .unwrap();
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    assert_eq!(accounting(&agent).await.reservations, 0);
    assert!(
        session
            .record_schedule_input(
                &row.reference,
                &attempt.attempt_id,
                JobInputState::Included { model_sequence: 3 }
            )
            .await
            .is_err()
    );
    let next = prepare(&agent, "wake", slot(2), now()).await.unwrap();
    session
        .prepare_schedule_input(&next.reference)
        .await
        .unwrap();
    handoff(&agent, &next.reference, &session, now())
        .await
        .unwrap();
    session
        .bind_schedule_input(&next.reference, &attempt.attempt_id)
        .await
        .unwrap();
    for state in [
        JobInputState::Accepted,
        JobInputState::Dispatching { model_sequence: 4 },
        JobInputState::Uncertain {
            reason: "response acknowledgement unavailable".into(),
        },
    ] {
        session
            .record_schedule_input(&next.reference, &attempt.attempt_id, state)
            .await
            .unwrap();
    }
    finish(&mut session, &attempt, TurnOutcome::Failure).await;
    agent
        .settle_reconciled_for_test(&next.reference, &session)
        .await
        .unwrap();
    assert_eq!(
        (
            accounting(&agent).await.successful_fires,
            accounting(&agent).await.reservations
        ),
        (0, 1)
    );
    assert!(prepare(&agent, "wake", slot(3), now()).await.is_err());
}

#[tokio::test]
async fn identity_write_failure_and_missing_projection_hold_instead_of_resetting() {
    let (instance, _user, agent, operations, mut session) = fixture().await;
    let (row, attempt) = admitted(&agent, &mut session, false).await;
    included(&session, &row, &attempt).await;
    // Real native commit failure: final entry, completion and outcome are atomic.
    let read_only_session = Database::open(&instance, session.database().root_id())
        .await
        .unwrap();
    let mut no_writer = Session::try_new(ConversationId("read-only".into()), read_only_session)
        .await
        .unwrap();
    assert!(
        no_writer
            .complete_turn_attempt(&attempt, None)
            .await
            .is_err()
    );
    assert!(
        session
            .turn_completion(&attempt.attempt_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        session
            .read_turn_attempt(&attempt.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TurnAttemptStatus::Started
    );
    finish(&mut session, &attempt, TurnOutcome::Success).await;
    let no_writer = agent.clone().with_scheduling_database(
        Database::open(&instance, operations.root_id())
            .await
            .unwrap(),
    );
    let before = operations.snapshot().await.unwrap();
    assert!(
        no_writer
            .settle_reconciled_for_test(&row.reference, &session)
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert_eq!(accounting(&agent).await.reservations, 1);
    // A completed unrelated request must not supply Fresh success proof.
    let mut evidence = session
        .schedule_delivery_evidence(&row.reference)
        .await
        .unwrap()
        .unwrap();
    let original = serde_json::to_value(&evidence.input).unwrap();
    evidence.input.request_id = crate::session::TurnRequestId::parse("another-request");
    put(
        session.database(),
        "scheduled_input_receipts",
        &row.reference.delivery_id,
        serde_json::to_value(&evidence.input).unwrap(),
    )
    .await;
    assert!(
        agent
            .settle_reconciled_for_test(&row.reference, &session)
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    put(
        session.database(),
        "scheduled_input_receipts",
        &row.reference.delivery_id,
        original,
    )
    .await;
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    // Lost projection data cannot replenish the successful last slot.
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<serde_json::Value>>("schedule_settlements")
        .await
        .unwrap()
        .delete(&row.reference.occurrence_id)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(prepare(&agent, "wake", slot(2), now()).await.is_err());
    assert!(
        agent
            .settle_reconciled_for_test(&row.reference, &session)
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
}

#[tokio::test]
async fn sqlite_restart_projects_terminal_success_without_reexecution_or_count_reset() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("scheduling.db").display()
    );
    let (agent_id, operations_id, session_id, reference, attempt_id) = {
        let (instance, mut user) = eidetica::Instance::create_backend(
            Box::new(
                eidetica::backend::database::Sqlite::connect(&url)
                    .await
                    .unwrap(),
            ),
            eidetica::NewAccount::with_password("scheduling", "test-password"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let (_instance, _user, agent, operations, mut session) =
            fixture_on(instance, user, db).await;
        let (row, attempt) = admitted_with_clock(&agent, &mut session, false, true).await;
        included(&session, &row, &attempt).await;
        finish(&mut session, &attempt, TurnOutcome::Success).await;
        // Crash window: Session completion durable, operational projection absent.
        assert_eq!(
            (
                accounting(&agent).await.successful_fires,
                accounting(&agent).await.reservations
            ),
            (0, 1)
        );
        (
            agent.id().clone(),
            operations.root_id().clone(),
            session.database().root_id().clone(),
            row.reference,
            attempt.attempt_id,
        )
    };
    let instance = eidetica::Instance::connect(&url).await.unwrap();
    let user = instance
        .login_account("scheduling", "test-password")
        .await
        .unwrap();
    let operations = user.open_database(&operations_id).await.unwrap();
    let agent = AgentDb::from_database(user.open_database(&agent_id).await.unwrap())
        .with_scheduling_database(operations.clone());
    let session = Session::try_new(
        ConversationId("restarted".into()),
        user.open_database(&session_id).await.unwrap(),
    )
    .await
    .unwrap();
    let mut publications = HandoffScan::open(&agent).await.unwrap().unwrap();
    let page = publications.next_page(&agent, 1).await.unwrap().unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].reference, reference);
    assert_eq!(page[0].publication_sequence, Some(1));
    let definition = agent.find_schedule("wake").await.unwrap().unwrap();
    let clock = agent
        .schedule_clock_cursor(&definition)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(clock.position, 1);
    assert_eq!(clock.last.as_ref(), Some(&reference));
    assert_eq!(
        clock.next,
        Some(DueSlot::Interval {
            anchor: now(),
            ordinal: 2
        })
    );
    assert_eq!(page[0].version, 4);
    assert!(publications.next_page(&agent, 1).await.unwrap().is_none());
    agent
        .settle_reconciled_for_test(&reference, &session)
        .await
        .unwrap();
    assert_eq!(
        (
            accounting(&agent).await.successful_fires,
            accounting(&agent).await.reservations,
            accounting(&agent).await.fire_count
        ),
        (1, 0, 8)
    );
    let before = operations.snapshot().await.unwrap();
    agent
        .settle_reconciled_for_test(&reference, &session)
        .await
        .unwrap();
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert_eq!(session.attempts_for_test().await.len(), 1);
    assert_eq!(session.attempts_for_test().await[0].attempt_id, attempt_id);
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
    for (db, store) in [
        (session.database(), "turn_outcomes"),
        (session.database(), "scheduled_input_receipts"),
        (&operations, "schedule_settlements"),
        (&operations, "schedule_clocks"),
    ] {
        let rows = db
            .new_transaction()
            .await
            .unwrap()
            .get_store::<Table<serde_json::Value>>(store)
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap();
        assert!(!rows.is_empty());
        assert!(
            !serde_json::to_string(&rows)
                .unwrap()
                .contains("INVOCATION-PRIVATE-PROMPT")
        );
    }
}

#[tokio::test]
async fn fresh_refuses_other_request_and_unknown_receipt_without_capacity_mutation() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    // Bind a Fresh handoff to a real but unrelated human request. Even a
    // successful terminal on it cannot prove the Fresh scheduled turn ran.
    let (row, attempt) = admitted(&agent, &mut session, false).await;
    let receipt = session
        .bind_schedule_input(&row.reference, &attempt.attempt_id)
        .await
        .unwrap();
    let before = session.database().snapshot().await.unwrap();
    assert_eq!(
        session
            .bind_schedule_input(&row.reference, &attempt.attempt_id)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(before, session.database().snapshot().await.unwrap());
    let other_request = session
        .add_entry(SessionEntry {
            sender: "human".into(),
            content: "unrelated".into(),
            timestamp: now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let other_attempt = session
        .start_turn_attempt(other_request.clone())
        .await
        .unwrap();
    assert!(
        session
            .bind_schedule_input(&row.reference, &other_attempt.attempt_id)
            .await
            .is_err(),
        "binding cannot silently move to another attempt"
    );
    let original = serde_json::to_value(&receipt).unwrap();
    let mut wrong = receipt.clone();
    wrong.attempt_id = other_attempt.attempt_id.clone();
    wrong.request_id = other_request;
    wrong.state = JobInputState::Included { model_sequence: 3 };
    put(
        session.database(),
        "scheduled_input_receipts",
        &row.reference.delivery_id,
        serde_json::to_value(&wrong).unwrap(),
    )
    .await;
    finish(&mut session, &other_attempt, TurnOutcome::Success).await;
    let before = operations.snapshot().await.unwrap();
    assert!(
        agent
            .settle_reconciled_for_test(&row.reference, &session)
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let mut unknown = original.clone();
    unknown["version"] = json!(99);
    put(
        session.database(),
        "scheduled_input_receipts",
        &row.reference.delivery_id,
        unknown,
    )
    .await;
    assert!(
        agent
            .settle_reconciled_for_test(&row.reference, &session)
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    put(
        session.database(),
        "scheduled_input_receipts",
        &row.reference.delivery_id,
        original,
    )
    .await;
    included(&session, &row, &attempt).await;
    finish(&mut session, &attempt, TurnOutcome::Success).await;
    agent
        .settle_reconciled_for_test(&row.reference, &session)
        .await
        .unwrap();
    assert_eq!(accounting(&agent).await.successful_fires, 1);
    // A second completion cannot relabel success as failure or append an error.
    let before = session.database().snapshot().await.unwrap();
    let error = SessionEntry {
        sender: "timer".into(),
        content: "late error".into(),
        timestamp: now(),
        entry_type: EntryType::Error,
        metadata: None,
        routing: None,
    };
    assert!(
        session
            .complete_turn_attempt(&attempt, Some(error))
            .await
            .is_err()
    );
    assert_eq!(before, session.database().snapshot().await.unwrap());
}
