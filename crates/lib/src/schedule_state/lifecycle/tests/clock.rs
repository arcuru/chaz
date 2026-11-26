use super::super::clock::{self, ClockCursor};
use super::*;
use crate::routine::Trigger;

async fn timed(
    agent: &AgentDb,
    schedule: &Schedule,
    slot: DueSlot,
    at: DateTime<Utc>,
) -> anyhow::Result<Occurrence> {
    reconcile_all(agent, schedule).await?;
    super::super::prepare_clock(agent, &schedule.id, &schedule.revision_key()?, slot, at).await
}

#[tokio::test]
async fn clock_handoff_pins_all_trigger_successors_without_starting_or_replaying() {
    for trigger in [
        Trigger::OneShot { fire_at: now() },
        Trigger::Cron {
            expr: "0 * * * * *".into(),
        },
        Trigger::Interval {
            period: std::time::Duration::from_secs(60),
        },
    ] {
        let (_instance, user, agent, operations, mut session) = fixture().await;
        let mut schedule = definition();
        schedule.trigger = trigger.clone();
        agent.upsert_schedule(schedule.clone()).await.unwrap();
        let slot = match trigger {
            Trigger::Interval { .. } => slot(7),
            _ => DueSlot::Calendar { due_at: now() },
        };
        let row = timed(&agent, &schedule, slot.clone(), now()).await.unwrap();
        assert_eq!(row.version, 4);
        let before = operations.snapshot().await.unwrap();
        assert_eq!(
            timed(&agent, &schedule, slot.clone(), now()).await.unwrap(),
            row
        );
        assert_eq!(before, operations.snapshot().await.unwrap());
        let cursor = clock::cursor(&agent, &schedule).await.unwrap().unwrap();
        assert_eq!(
            (cursor.position, cursor.next, cursor.last),
            (0, Some(slot.clone()), None)
        );
        assert!(
            super::super::prepare(
                &agent,
                "wake",
                &schedule.revision_key().unwrap(),
                slot.clone(),
                now()
            )
            .await
            .is_err(),
            "an untimed caller cannot downgrade a timed occurrence"
        );
        session
            .prepare_schedule_input(&row.reference)
            .await
            .unwrap();
        let admitted_at = now() + chrono::Duration::seconds(12);
        let published = handoff(&agent, &row.reference, &session, admitted_at)
            .await
            .unwrap();
        assert_eq!(published.publication_sequence, Some(1));
        let cursor = clock::cursor(&agent, &schedule).await.unwrap().unwrap();
        let expected = match trigger {
            Trigger::OneShot { .. } => None,
            Trigger::Cron { .. } => Some(DueSlot::Calendar {
                due_at: now() + chrono::Duration::minutes(1),
            }),
            Trigger::Interval { .. } => Some(DueSlot::Interval {
                anchor: admitted_at,
                ordinal: 8,
            }),
        };
        assert_eq!(
            (cursor.position, &cursor.next, &cursor.last),
            (1, &expected, &Some(row.reference.clone()))
        );
        // A lost acknowledgement and fresh Agent handle must retain the exact
        // admitted timestamp/lineage, not derive another anchor from restart.
        let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
            .with_scheduling_database(operations.clone());
        let before = operations.snapshot().await.unwrap();
        assert_eq!(
            handoff(
                &reopened,
                &row.reference,
                &session,
                admitted_at + chrono::Duration::hours(2)
            )
            .await
            .unwrap(),
            published
        );
        assert_eq!(
            clock::cursor(&reopened, &schedule).await.unwrap(),
            Some(cursor)
        );
        assert_eq!(before, operations.snapshot().await.unwrap());
        let mut scan = HandoffScan::open(&reopened).await.unwrap().unwrap();
        assert_eq!(
            scan.next_page(&reopened, 1).await.unwrap().unwrap(),
            vec![published]
        );
        assert!(session.attempts_for_test().await.is_empty());
        let state = accounting(&agent).await;
        assert_eq!(
            (
                state.fire_count,
                state.reservations,
                state.successful_fires,
                state.handoff_count
            ),
            (8, 1, 0, 1)
        );
        assert!(
            !serde_json::to_string(&clock::cursor(&agent, &schedule).await.unwrap())
                .unwrap()
                .contains("INVOCATION-PRIVATE-PROMPT")
        );
    }
}

#[tokio::test]
async fn clock_only_accepts_its_current_successor_and_preserves_old_slots() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.fire_count = 0;
    schedule.max_fires = None;
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let first = timed(&agent, &schedule, slot(0), now()).await.unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(
        timed(&agent, &schedule, slot(1), now()).await.is_err(),
        "cannot skip a prepared slot"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    session
        .prepare_schedule_input(&first.reference)
        .await
        .unwrap();
    let first = handoff(&agent, &first.reference, &session, now())
        .await
        .unwrap();
    let next = clock::cursor(&agent, &schedule)
        .await
        .unwrap()
        .unwrap()
        .next
        .unwrap();
    let second = timed(
        &agent,
        &schedule,
        next.clone(),
        now() + chrono::Duration::minutes(1),
    )
    .await
    .unwrap();
    session
        .prepare_schedule_input(&second.reference)
        .await
        .unwrap();
    let second = handoff(
        &agent,
        &second.reference,
        &session,
        now() + chrono::Duration::minutes(1),
    )
    .await
    .unwrap();
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        timed(
            &agent,
            &schedule,
            first.slot.clone(),
            now() + chrono::Duration::hours(1)
        )
        .await
        .unwrap(),
        first
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let mut scan = OccurrenceScan::open(&agent).await.unwrap().unwrap();
    let mut rows = Vec::new();
    while let Some(page) = scan.next_page(&agent, 1).await.unwrap() {
        rows.extend(page);
    }
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&first) && rows.contains(&second));
    assert_eq!(
        clock::cursor(&agent, &schedule)
            .await
            .unwrap()
            .unwrap()
            .position,
        2
    );
    assert!(session.attempts_for_test().await.is_empty());
}

#[tokio::test]
async fn missing_or_conflicting_clock_holds_all_lifetimes_without_reinitialization() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let row = timed(&agent, &schedule, slot(0), now()).await.unwrap();
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    handoff(&agent, &row.reference, &session, now())
        .await
        .unwrap();
    let original = clock::cursor(&agent, &schedule).await.unwrap().unwrap();
    let mut other = schedule.clone();
    other.id = "other".into();
    other.lifetime = Some("other-lifetime".into());
    agent.upsert_schedule(other.clone()).await.unwrap();
    let revision = schedule.revision_key().unwrap();
    let mut wrong = original.clone();
    wrong.next = Some(slot(999));
    put(
        &operations,
        clock::CLOCKS,
        &revision,
        serde_json::to_value(wrong).unwrap(),
    )
    .await;
    let before = operations.snapshot().await.unwrap();
    assert!(clock::cursor(&agent, &schedule).await.is_err());
    assert!(timed(&agent, &other, slot(0), now()).await.is_err());
    assert_eq!(before, operations.snapshot().await.unwrap());
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<ClockCursor>>(clock::CLOCKS)
        .await
        .unwrap()
        .delete(&revision)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(
        timed(&agent, &other, slot(0), now()).await.is_err(),
        "another lifetime's audit must detect lost clock custody"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    put(
        &operations,
        clock::CLOCKS,
        &revision,
        serde_json::to_value(original).unwrap(),
    )
    .await;
    assert!(timed(&agent, &other, slot(0), now()).await.is_ok());
}

#[tokio::test]
async fn clock_native_failed_commits_never_publish_or_advance_and_old_reader_refuses() {
    let (instance, _user, agent, operations, mut session) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let no_writer = agent.clone().with_scheduling_database(
        Database::open(&instance, operations.root_id())
            .await
            .unwrap(),
    );
    let before = operations.snapshot().await.unwrap();
    assert!(timed(&no_writer, &schedule, slot(0), now()).await.is_err());
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert!(clock::cursor(&agent, &schedule).await.unwrap().is_none());
    let row = timed(&agent, &schedule, slot(0), now()).await.unwrap();
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    // Exact pre-clock occurrence wire: the old decoder must refuse rather
    // than drop the field, regardless of an old caller's version handling.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PreviousOccurrence {
        #[serde(rename = "version")]
        _version: u32,
        #[serde(rename = "reference")]
        _reference: ScheduleReference,
        #[serde(rename = "slot")]
        _slot: DueSlot,
        #[serde(rename = "target")]
        _target: ScheduleTarget,
        #[serde(rename = "prepared_at")]
        _prepared_at: DateTime<Utc>,
        #[serde(rename = "preparation")]
        _preparation: Preparation,
        #[serde(default, rename = "publication_sequence")]
        _publication_sequence: Option<u64>,
        #[serde(default, rename = "fresh_root")]
        _fresh_root: Option<FreshRoot>,
    }
    assert!(
        serde_json::from_value::<PreviousOccurrence>(serde_json::to_value(&row).unwrap()).is_err()
    );
    let mut old_wire = serde_json::to_value(&row).unwrap();
    old_wire.as_object_mut().unwrap().remove("clock");
    assert!(
        serde_json::from_value::<Occurrence>(old_wire)
            .unwrap()
            .validate(&agent)
            .is_err()
    );
    let before = operations.snapshot().await.unwrap();
    assert!(
        handoff(&no_writer, &row.reference, &session, now())
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert!(HandoffScan::open(&agent).await.unwrap().is_none());
    assert_eq!(
        clock::cursor(&agent, &schedule)
            .await
            .unwrap()
            .unwrap()
            .position,
        0
    );
    assert!(session.attempts_for_test().await.is_empty());
    handoff(&agent, &row.reference, &session, now())
        .await
        .unwrap();
    assert_eq!(
        clock::cursor(&agent, &schedule)
            .await
            .unwrap()
            .unwrap()
            .position,
        1
    );
}

#[tokio::test]
async fn clock_preparation_refuses_forged_or_stale_source_without_advancing() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let row = timed(&agent, &schedule, slot(0), now()).await.unwrap();
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    let mut forged = row.clone();
    forged.clock.as_mut().unwrap().trigger = Trigger::Interval {
        period: std::time::Duration::from_secs(30),
    };
    // Locally coherent timing is insufficient: it must name the exact archived
    // executable source, not just fit the same interval-shaped slot.
    forged.validate(&agent).unwrap();
    put(
        &operations,
        OCCURRENCES,
        &row.reference.occurrence_id,
        serde_json::to_value(forged).unwrap(),
    )
    .await;
    let before = operations.snapshot().await.unwrap();
    let mut scan = OccurrenceScan::open(&agent).await.unwrap().unwrap();
    assert!(scan.next_page(&agent, 1).await.is_err());
    assert!(
        handoff(&agent, &row.reference, &session, now())
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    put(
        &operations,
        OCCURRENCES,
        &row.reference.occurrence_id,
        serde_json::to_value(&row).unwrap(),
    )
    .await;
    let mut edited = schedule.clone();
    edited.prompt = "replacement private input".into();
    agent.upsert_schedule(edited).await.unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(
        handoff(&agent, &row.reference, &session, now())
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert_eq!(
        clock::cursor(&agent, &schedule)
            .await
            .unwrap()
            .unwrap()
            .position,
        0
    );
    assert!(HandoffScan::open(&agent).await.unwrap().is_none());
    assert!(session.attempts_for_test().await.is_empty());
}

async fn handed_off(
    agent: &AgentDb,
    session: &mut Session,
    schedule: &Schedule,
    slot: DueSlot,
    at: DateTime<Utc>,
) -> Occurrence {
    let row = timed(agent, schedule, slot, at).await.unwrap();
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    handoff(agent, &row.reference, session, at).await.unwrap()
}

async fn miss(
    agent: &AgentDb,
    schedule: &Schedule,
    slot: DueSlot,
    at: DateTime<Utc>,
) -> anyhow::Result<clock::MissedClock> {
    reconcile_all(agent, schedule).await?;
    super::super::miss_clock(agent, &schedule.id, &schedule.revision_key()?, slot, at).await
}

#[tokio::test]
async fn missed_window_off_skips_without_work_and_keeps_lineage_across_reopen() {
    let (_instance, user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.max_fires = None;
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    assert!(!schedule.effective_catch_up(), "new schedules default off");
    handed_off(&agent, &mut session, &schedule, slot(0), now()).await;
    let missed = DueSlot::Interval {
        anchor: now(),
        ordinal: 1,
    };
    let before = accounting(&agent).await;
    let restart = now() + chrono::Duration::minutes(10);
    let next = DueSlot::Interval {
        anchor: restart,
        ordinal: 2,
    };
    assert_eq!(
        miss(&agent, &schedule, missed.clone(), restart)
            .await
            .unwrap(),
        clock::MissedClock::Skipped(Some(next.clone()))
    );
    // No occurrence, reservation, handoff or Session work for the window.
    let after = accounting(&agent).await;
    assert_eq!(
        (
            after.fire_count,
            after.reservations,
            after.handoff_count,
            after.successful_fires
        ),
        (
            before.fire_count,
            before.reservations,
            before.handoff_count,
            before.successful_fires
        )
    );
    assert_eq!(session.attempts_for_test().await.len(), 0);
    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(operations.clone());
    let cursor = clock::cursor(&reopened, &schedule).await.unwrap().unwrap();
    assert_eq!(
        (
            cursor.version,
            cursor.position,
            &cursor.next,
            &cursor.missed_from
        ),
        (2, 1, &Some(next.clone()), &Some(missed.clone()))
    );
    let windows = reopened.schedule_missed_windows(&schedule).await.unwrap();
    assert_eq!(windows.len(), 1);
    // Due at +1m through +10m inclusive: ten exact slots, one record.
    assert_eq!(
        (
            windows[0].first_due,
            windows[0].last_due,
            windows[0].slots,
            windows[0].slots_exact,
            &windows[0].disposition
        ),
        (
            now() + chrono::Duration::minutes(1),
            now() + chrono::Duration::minutes(10),
            10,
            true,
            &clock::MissedDisposition::Skipped {
                next: Some(next.clone())
            }
        )
    );
    assert!(
        !serde_json::to_string(&windows)
            .unwrap()
            .contains("INVOCATION-PRIVATE-PROMPT")
    );
    // The old slot cannot be prepared. A lost skip reply reads back the
    // same successor without a second commit or a second missed window.
    assert!(
        timed(&reopened, &schedule, missed.clone(), restart)
            .await
            .is_err()
    );
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        miss(&reopened, &schedule, missed.clone(), restart)
            .await
            .unwrap(),
        clock::MissedClock::Skipped(Some(next.clone()))
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let later = restart + chrono::Duration::minutes(5);
    let after_next = DueSlot::Interval {
        anchor: later,
        ordinal: 3,
    };
    assert_eq!(
        miss(&reopened, &schedule, next.clone(), later)
            .await
            .unwrap(),
        clock::MissedClock::Skipped(Some(after_next.clone()))
    );
    let cursor = clock::cursor(&reopened, &schedule).await.unwrap().unwrap();
    assert_eq!(cursor.missed_from, Some(missed.clone()));
    assert_eq!(
        reopened
            .schedule_missed_windows(&schedule)
            .await
            .unwrap()
            .len(),
        2
    );
    // The next live slot hands off normally and restores a version-1 link.
    let resumed = later + chrono::Duration::minutes(1);
    handed_off(&reopened, &mut session, &schedule, after_next, resumed).await;
    let cursor = clock::cursor(&reopened, &schedule).await.unwrap().unwrap();
    assert_eq!(
        (cursor.version, cursor.position, cursor.missed_from),
        (1, 2, None)
    );
    assert_eq!(accounting(&reopened).await.handoff_count, 2);
}

#[tokio::test]
async fn missed_window_catch_up_fires_once_and_owned_slots_are_not_missed() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.max_fires = None;
    schedule.catch_up = Some(true);
    schedule.trigger = Trigger::Cron {
        expr: "0 * * * * *".into(),
    };
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let first = DueSlot::Calendar { due_at: now() };
    handed_off(&agent, &mut session, &schedule, first, now()).await;
    let missed = DueSlot::Calendar {
        due_at: now() + chrono::Duration::minutes(1),
    };
    let restart = now() + chrono::Duration::minutes(10) + chrono::Duration::seconds(30);
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        miss(&agent, &schedule, missed.clone(), restart)
            .await
            .unwrap(),
        clock::MissedClock::CatchUp
    );
    assert_ne!(before, operations.snapshot().await.unwrap());
    let windows = agent.schedule_missed_windows(&schedule).await.unwrap();
    assert_eq!(
        (windows.len(), windows[0].slots, &windows[0].disposition),
        (1, 10, &clock::MissedDisposition::CaughtUp)
    );
    // The window's single occurrence; the handoff successor coalesces the
    // remaining slots to the first one after admission.
    handed_off(&agent, &mut session, &schedule, missed.clone(), restart).await;
    let cursor = clock::cursor(&agent, &schedule).await.unwrap().unwrap();
    assert_eq!(
        (cursor.position, cursor.next.clone(), cursor.version),
        (
            2,
            Some(DueSlot::Calendar {
                due_at: now() + chrono::Duration::minutes(11)
            }),
            1
        )
    );
    assert_eq!(accounting(&agent).await.handoff_count, 2);
    // A slot with an existing occurrence is ordinary preparation, not a miss.
    let owned = cursor.next.unwrap();
    timed(
        &agent,
        &schedule,
        owned.clone(),
        restart + chrono::Duration::minutes(1),
    )
    .await
    .unwrap();
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        miss(
            &agent,
            &schedule,
            owned,
            restart + chrono::Duration::hours(1)
        )
        .await
        .unwrap(),
        clock::MissedClock::Owned
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
}

#[test]
fn catch_up_defaults_off_except_explicit_or_migrated_interval() {
    let mut schedule = definition();
    assert!(!schedule.effective_catch_up());
    schedule.policy_origin = Some(crate::agent_db::SchedulePolicyOrigin::LegacyMigrated);
    assert!(
        schedule.effective_catch_up(),
        "legacy interval keeps one overdue fire"
    );
    schedule.catch_up = Some(false);
    assert!(!schedule.effective_catch_up(), "explicit setting wins");
    schedule.catch_up = None;
    schedule.trigger = Trigger::Cron {
        expr: "0 * * * * *".into(),
    };
    assert!(!schedule.effective_catch_up(), "legacy cron stays off");
    // Absent policy keeps the pre-existing revision identity.
    let mut plain = definition();
    let original = plain.revision_key().unwrap();
    plain.catch_up = None;
    assert_eq!(plain.revision_key().unwrap(), original);
    plain.catch_up = Some(false);
    assert_ne!(plain.revision_key().unwrap(), original);
}
