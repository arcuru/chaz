use super::*;
use crate::agent_db::{AgentDbConfig, AgentMeta, ExecutorRef, create_agent_db};
use crate::types::ConversationId;
use serde_json::json;

mod clock;
mod order;
mod refresh;

async fn fixture() -> (
    eidetica::Instance,
    eidetica::AccountSession,
    AgentDb,
    Database,
    Session,
) {
    let (instance, user, session_db) = crate::session::test_helpers::test_session_db().await;
    fixture_on(instance, user, session_db).await
}

async fn fixture_on(
    instance: eidetica::Instance,
    mut user: eidetica::AccountSession,
    session_db: Database,
) -> (
    eidetica::Instance,
    eidetica::AccountSession,
    AgentDb,
    Database,
    Session,
) {
    let (agent, key) = create_agent_db(
        &mut user,
        "timer",
        &AgentDbConfig::default(),
        &AgentMeta::default(),
    )
    .await
    .unwrap();
    let (operations, peer) = crate::executor_db::ensure(&mut user, &agent, &key)
        .await
        .unwrap();
    agent
        .pin_scheduling_reference(ExecutorRef {
            db_id: operations.root_id().to_string(),
            peer_pubkey: peer.to_string(),
        })
        .await
        .unwrap();
    let session = Session::new(ConversationId("timer".into()), session_db).await;
    (
        instance,
        user,
        agent.with_scheduling_database(operations.clone()),
        operations,
        session,
    )
}

async fn prepare(
    agent: &AgentDb,
    id: &str,
    slot: DueSlot,
    now: DateTime<Utc>,
) -> anyhow::Result<Occurrence> {
    let schedule = agent.find_schedule(id).await?.unwrap();
    reconcile_all(agent, &schedule).await?;
    let revision = schedule.revision_key()?;
    super::prepare(agent, id, &revision, slot, now).await
}

// Existing mutation scenarios arrange a completed audit explicitly. Production
// never drives an unbounded scan from a prepare/handoff/settle call.
async fn reconcile_all(agent: &AgentDb, schedule: &Schedule) -> anyhow::Result<()> {
    for _ in 0..1000 {
        let progress = agent.reconcile_schedule_accounting(schedule, 2).await?;
        assert!(progress.scanned <= 2);
        if progress.complete {
            return Ok(());
        }
    }
    anyhow::bail!("test reconciliation did not finish")
}

async fn handoff(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session: &Session,
    now: DateTime<Utc>,
) -> anyhow::Result<Occurrence> {
    let schedule = archived_definition(agent, reference).await?;
    reconcile_all(agent, &schedule).await?;
    if schedule.target == ScheduleTarget::Fresh
        && session.schedule_input(reference).await?.is_some()
    {
        let txn = agent
            .scheduling_database()
            .await?
            .unwrap()
            .new_transaction()
            .await?;
        if read_occurrence(&txn, agent, &reference.occurrence_id)
            .await?
            .is_some_and(|row| matches!(row.preparation, Preparation::Prepared))
        {
            fresh::request(agent, reference, now).await?;
            fresh::bind(
                agent,
                reference,
                &session.database().root_id().to_string(),
                now,
            )
            .await?;
        }
    }
    super::handoff(agent, reference, session, now).await
}

async fn release_prepared(agent: &AgentDb, reference: &ScheduleReference) -> anyhow::Result<()> {
    reconcile_all(agent, &archived_definition(agent, reference).await?).await?;
    super::release_prepared(agent, reference).await
}

impl AgentDb {
    async fn settle_reconciled_for_test(
        &self,
        reference: &ScheduleReference,
        session: &Session,
    ) -> anyhow::Result<Option<crate::session::scheduled::ScheduleDeliveryEvidence>> {
        reconcile_all(self, &archived_definition(self, reference).await?).await?;
        self.settle_schedule_occurrence(reference, session).await
    }
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-06T12:00:00Z")
        .unwrap()
        .to_utc()
}

fn slot(ordinal: u64) -> DueSlot {
    DueSlot::Interval {
        anchor: now() - chrono::Duration::minutes(1),
        ordinal,
    }
}

fn definition() -> Schedule {
    let mut schedule = Schedule::new(
        "wake",
        crate::routine::Trigger::Interval {
            period: std::time::Duration::from_secs(60),
        },
        "INVOCATION-PRIVATE-PROMPT",
        ScheduleTarget::Fresh,
    );
    schedule.lifetime = Some("lifetime".into());
    schedule.max_fires = Some(8);
    schedule.fire_count = 7;
    schedule
}

async fn accounting(agent: &AgentDb) -> AccountingState {
    let view = agent.preview_scheduling().await.unwrap();
    let super::super::readers::AccountingView::Current(state) = &view[0].accounting else {
        panic!("current accounting")
    };
    state.clone()
}

async fn put(db: &Database, store: &str, key: &str, value: serde_json::Value) {
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<serde_json::Value>>(store)
        .await
        .unwrap()
        .set(key, value)
        .await
        .unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn last_slot_survives_reopen_and_handoff_does_not_start_or_count_success() {
    let (_instance, user, agent, operations, mut session) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let definitions_before = agent.database().snapshot().await.unwrap();
    let first = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    let state = accounting(&agent).await;
    assert_eq!(
        (
            state.legacy_baseline,
            state.successful_fires,
            state.reservations,
            state.handoff_count,
            state.fire_count
        ),
        (7, 0, 1, 0, 8)
    );
    assert!(
        prepare(&agent, "wake", slot(2), now()).await.is_err(),
        "last slot is reserved, not oversubscribed"
    );
    assert!(
        session
            .database()
            .new_transaction()
            .await
            .unwrap()
            .get_store::<Table<crate::session::TurnAttempt>>("turn_attempts")
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap()
            .is_empty()
    );
    let before = operations.snapshot().await.unwrap();
    assert!(
        handoff(&agent, &first.reference, &session, now())
            .await
            .is_err(),
        "no publication without inert Session anchor"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let anchor = session
        .prepare_schedule_input(&first.reference)
        .await
        .unwrap();
    assert!(
        matches!(
            occurrences(&agent).await.unwrap()[0].preparation,
            Preparation::Prepared
        ),
        "Ack alone never hands off"
    );
    // An unrelated immutable audit must not be overwritten on an ID collision.
    let collision = ScheduleFire {
        schedule_id: "another-schedule".into(),
        firing_id: Some(first.reference.delivery_id.clone()),
        lifetime: Some("another-lifetime".into()),
        request_id: None,
        attempt_id: None,
        fired_at: now(),
        session_db_id: "another-session".into(),
        fresh: true,
        usage: None,
    };
    put(
        &operations,
        FIRES,
        &first.reference.delivery_id,
        serde_json::to_value(&collision).unwrap(),
    )
    .await;
    let before = operations.snapshot().await.unwrap();
    assert!(
        handoff(&agent, &first.reference, &session, now())
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<ScheduleFire>>(FIRES)
        .await
        .unwrap()
        .delete(&first.reference.delivery_id)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(user.open_database(operations.root_id()).await.unwrap());
    assert_eq!(
        prepare(&reopened, "wake", slot(1), now()).await.unwrap(),
        first
    );
    let handed = handoff(&reopened, &first.reference, &session, now())
        .await
        .unwrap();
    assert!(
        matches!(handed.preparation, Preparation::HandedOff { ref input, .. } if input.as_ref() == &anchor)
    );
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        handoff(&reopened, &first.reference, &session, now())
            .await
            .unwrap(),
        handed
    );
    assert_eq!(
        before,
        operations.snapshot().await.unwrap(),
        "lost ack readback is not another history write"
    );
    let state = accounting(&agent).await;
    assert_eq!(
        (
            state.successful_fires,
            state.reservations,
            state.handoff_count,
            state.fire_count
        ),
        (0, 1, 1, 8)
    );
    assert!(
        release_prepared(&agent, &first.reference).await.is_err(),
        "uncertain handoff never refunds"
    );
    assert!(
        session
            .database()
            .new_transaction()
            .await
            .unwrap()
            .get_store::<Table<crate::session::TurnAttempt>>("turn_attempts")
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap()
            .is_empty()
    );
    let fires = agent.list_schedule_fires().await.unwrap();
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].request_id.as_ref(), Some(&anchor.request_id));
    assert!(fires[0].attempt_id.is_none());
    assert_eq!(
        definitions_before,
        agent.database().snapshot().await.unwrap(),
        "no per-fire definition write"
    );
    let txn = operations.new_transaction().await.unwrap();
    for store in [STATE, OCCURRENCES, FIRES] {
        let raw = txn
            .get_store::<Table<serde_json::Value>>(store)
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap();
        assert!(
            !serde_json::to_string(&raw)
                .unwrap()
                .contains(&schedule.prompt)
        );
    }
    assert!(
        !serde_json::to_string(&session.entries())
            .unwrap()
            .contains(&schedule.prompt)
    );
    // A legacy writer cannot silently overwrite the new protocol.
    assert!(
        super::super::reserve(&operations, &schedule, now())
            .await
            .is_err()
    );
    // Retirement is a supported version-aware mutation; it must preserve
    // successful/reserved accounting rather than downgrade to the old shape.
    super::super::retire(&operations, &schedule, false)
        .await
        .unwrap();
    assert_eq!(accounting(&reopened).await, state);
}

#[tokio::test]
async fn handoff_revalidates_exact_agent_revision_and_pinned_target() {
    let (_instance, mut user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.target = ScheduleTarget::Pinned {
        session_db_id: session.database().root_id().to_string(),
    };
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    assert!(
        super::prepare(&agent, "wake", "stale-revision", slot(1), now())
            .await
            .is_err()
    );
    assert!(occurrences(&agent).await.unwrap().is_empty());
    let prepared = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    let reference = &prepared.reference;
    session.prepare_schedule_input(reference).await.unwrap();
    let (other, _) = create_agent_db(
        &mut user,
        "other",
        &AgentDbConfig::default(),
        &AgentMeta::default(),
    )
    .await
    .unwrap();
    other.upsert_schedule(schedule.clone()).await.unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(handoff(&other, reference, &session, now()).await.is_err());
    let (_instance2, _user2, other_db) = crate::session::test_helpers::test_session_db().await;
    let mut wrong_session = Session::new(ConversationId("wrong".into()), other_db).await;
    wrong_session
        .prepare_schedule_input(reference)
        .await
        .unwrap();
    assert!(
        handoff(&agent, reference, &wrong_session, now())
            .await
            .is_err()
    );
    for changed in [
        {
            let mut s = schedule.clone();
            s.prompt = "changed".into();
            s
        },
        {
            let mut s = schedule.clone();
            s.enabled = false;
            s
        },
        {
            let mut s = schedule.clone();
            s.expires_at = Some(now());
            s
        },
    ] {
        agent.upsert_schedule(changed).await.unwrap();
        assert!(handoff(&agent, reference, &session, now()).await.is_err());
        assert_eq!(before, operations.snapshot().await.unwrap());
    }
    agent.remove_schedule("wake").await.unwrap();
    assert!(handoff(&agent, reference, &session, now()).await.is_err());
    schedule.lifetime = Some("replacement".into());
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    assert!(handoff(&agent, reference, &session, now()).await.is_err());
    // Old cleanup releases only its original lifetime, once.
    release_prepared(&agent, reference).await.unwrap();
    let before = operations.snapshot().await.unwrap();
    release_prepared(&agent, reference).await.unwrap();
    assert_eq!(before, operations.snapshot().await.unwrap());
    let current = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    assert_ne!(current.reference.lifetime, reference.lifetime);
    assert_eq!(accounting(&agent).await.fire_count, 8);
    assert_eq!(
        agent.find_schedule("wake").await.unwrap().unwrap(),
        schedule
    );
}

#[tokio::test]
async fn one_shot_handoff_consumes_only_the_original_trigger() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.trigger = crate::routine::Trigger::OneShot { fire_at: now() };
    agent.upsert_schedule(schedule).await.unwrap();
    let due = DueSlot::Calendar { due_at: now() };
    let prepared = prepare(&agent, "wake", due.clone(), now()).await.unwrap();
    session
        .prepare_schedule_input(&prepared.reference)
        .await
        .unwrap();
    handoff(&agent, &prepared.reference, &session, now())
        .await
        .unwrap();
    assert!(accounting(&agent).await.consumed);
    assert!(release_prepared(&agent, &prepared.reference).await.is_err());
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        prepare(&agent, "wake", due, now()).await.unwrap().reference,
        prepared.reference
    );
    assert!(
        prepare(
            &agent,
            "wake",
            DueSlot::Calendar {
                due_at: now() - chrono::Duration::seconds(1)
            },
            now()
        )
        .await
        .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert_eq!(
        agent.list_schedules().await.unwrap().len(),
        1,
        "one-shot history and definition survive"
    );
}

#[tokio::test]
async fn released_preparation_is_not_an_implicit_retry() {
    let (_instance, _user, agent, _operations, _session) = fixture().await;
    agent.upsert_schedule(definition()).await.unwrap();
    let first = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    release_prepared(&agent, &first.reference).await.unwrap();
    assert_eq!(accounting(&agent).await.fire_count, 7);
    let repeated = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    assert_eq!(repeated.reference, first.reference);
    assert_eq!(repeated.preparation, Preparation::NotApplied);
    assert_eq!(accounting(&agent).await.reservations, 0);
    assert!(
        prepare(&agent, "wake", slot(2), now()).await.is_ok(),
        "another recurring slot is distinct work"
    );
}

#[tokio::test]
async fn legacy_unknown_and_inconsistent_state_never_reset_capacity() {
    let (_instance, _user, agent, operations, _session) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let key = schedule.lifetime_key();
    put(
        &operations,
        STATE,
        &key,
        json!({"fire_count":8,"consumed":false,"retired":false}),
    )
    .await;
    let before = operations.snapshot().await.unwrap();
    assert!(prepare(&agent, "wake", slot(1), now()).await.is_err());
    assert_eq!(before, operations.snapshot().await.unwrap());
    put(
        &operations,
        STATE,
        &key,
        json!({"fire_count":7,"consumed":false,"retired":false}),
    )
    .await;
    put(
        &operations,
        RESERVATIONS,
        "old",
        json!({"firing_id":"old","schedule_id":"wake","lifetime":key,"one_shot":false}),
    )
    .await;
    let before = operations.snapshot().await.unwrap();
    assert!(prepare(&agent, "wake", slot(1), now()).await.is_err());
    assert_eq!(before, operations.snapshot().await.unwrap());
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<Reservation>>(RESERVATIONS)
        .await
        .unwrap()
        .delete("old")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let first = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    let valid_state = serde_json::to_value(accounting(&agent).await).unwrap();
    for corrupt in [
        {
            let mut s = valid_state.clone();
            s["version"] = json!(99);
            s
        },
        {
            let mut s = valid_state.clone();
            s["fire_count"] = json!(7);
            s
        },
        {
            let mut s = valid_state.clone();
            s["legacy_baseline"] = json!(6);
            s["fire_count"] = json!(7);
            s
        },
        {
            let mut s = valid_state.clone();
            s["reservations"] = json!(0);
            s["fire_count"] = json!(7);
            s
        },
        {
            let mut s = valid_state.clone();
            s["unexpected"] = json!(true);
            s
        },
    ] {
        put(&operations, STATE, &key, corrupt).await;
        let before = operations.snapshot().await.unwrap();
        assert!(prepare(&agent, "wake", slot(1), now()).await.is_err());
        assert_eq!(before, operations.snapshot().await.unwrap());
    }
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<serde_json::Value>>(STATE)
        .await
        .unwrap()
        .delete(&key)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(
        prepare(&agent, "wake", slot(1), now()).await.is_err(),
        "existing occurrence with missing state holds"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    put(&operations, STATE, &key, valid_state).await;
    let mut raw = serde_json::to_value(&first).unwrap();
    raw["reference"]["delivery_id"] = json!("conflict");
    raw["version"] = json!(99);
    put(
        &operations,
        OCCURRENCES,
        &first.reference.occurrence_id,
        raw,
    )
    .await;
    assert!(occurrences(&agent).await.is_err());
    let before = operations.snapshot().await.unwrap();
    assert!(prepare(&agent, "wake", slot(2), now()).await.is_err());
    assert_eq!(before, operations.snapshot().await.unwrap());
}

#[tokio::test]
async fn required_ledger_writes_fail_without_publishing_or_refunding() {
    let (instance, _user, agent, operations, mut session) = fixture().await;
    agent.upsert_schedule(definition()).await.unwrap();
    let read_only = Database::open(&instance, operations.root_id())
        .await
        .unwrap();
    let unavailable_writer = agent.clone().with_scheduling_database(read_only);
    let before = operations.snapshot().await.unwrap();
    assert!(
        prepare(&unavailable_writer, "wake", slot(1), now())
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert!(occurrences(&agent).await.unwrap().is_empty());
    let error = agent
        .prepare_schedule_occurrence(
            "wake",
            &definition().revision_key().unwrap(),
            slot(1),
            now(),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("reconciliation required"),
        "failed commit must discard the certificate: {error:#}"
    );
    let prepared = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    session
        .prepare_schedule_input(&prepared.reference)
        .await
        .unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(
        handoff(&unavailable_writer, &prepared.reference, &session, now())
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert!(agent.list_schedule_fires().await.unwrap().is_empty());
    assert!(
        HandoffScan::open(&agent).await.unwrap().is_none(),
        "a failed handoff cannot publish an index/head without its occurrence/fire"
    );
    assert_eq!(accounting(&agent).await.reservations, 1);
    assert!(
        release_prepared(&unavailable_writer, &prepared.reference)
            .await
            .is_err()
    );
    assert_eq!(
        before,
        operations.snapshot().await.unwrap(),
        "failed refund cannot free capacity"
    );
    // The exact acknowledged handoff can still complete once with valid keys.
    handoff(&agent, &prepared.reference, &session, now())
        .await
        .unwrap();
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 1);
}

#[tokio::test]
async fn legacy_handoff_history_is_a_baseline_not_inferred_success() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.max_fires = Some(9);
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let key = schedule.lifetime_key();
    let old = ScheduleFire {
        schedule_id: "wake".into(),
        firing_id: Some("old".into()),
        lifetime: Some(key.clone()),
        request_id: None,
        attempt_id: None,
        fired_at: now() - chrono::Duration::days(1),
        session_db_id: "legacy-session".into(),
        fresh: true,
        usage: None,
    };
    put(
        &operations,
        STATE,
        &key,
        json!({"fire_count":8,"consumed":false,"retired":false}),
    )
    .await;
    put(
        &operations,
        FIRES,
        "old",
        serde_json::to_value(&old).unwrap(),
    )
    .await;
    let prepared = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    let state = accounting(&agent).await;
    assert_eq!(
        (
            state.legacy_baseline,
            state.fire_count,
            state.handoff_count,
            state.successful_fires
        ),
        (8, 9, 1, 0)
    );
    session
        .prepare_schedule_input(&prepared.reference)
        .await
        .unwrap();
    handoff(&agent, &prepared.reference, &session, now())
        .await
        .unwrap();
    let state = accounting(&agent).await;
    assert_eq!(
        (
            state.legacy_baseline,
            state.fire_count,
            state.handoff_count,
            state.successful_fires
        ),
        (8, 9, 2, 0)
    );
    let txn = operations.new_transaction().await.unwrap();
    assert_eq!(
        serde_json::to_value(
            txn.get_store::<Table<ScheduleFire>>(FIRES)
                .await
                .unwrap()
                .get("old")
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(old).unwrap(),
        "legacy evidence remains byte-equivalent"
    );
    assert_eq!(
        agent.find_schedule("wake").await.unwrap().unwrap(),
        schedule
    );
}

#[path = "tests/accounting.rs"]
mod accounting_tests;
#[path = "tests/serialization.rs"]
mod serialization;
#[path = "tests/settlement.rs"]
mod settlement;

#[tokio::test]
async fn bounded_occurrence_scan_keeps_one_snapshot_and_never_initializes_or_resets() {
    let (_instance, mut user, agent, operations, _session) = fixture().await;
    let (uninitialized, _) = create_agent_db(
        &mut user,
        "other",
        &AgentDbConfig::default(),
        &AgentMeta::default(),
    )
    .await
    .unwrap();
    let untouched = uninitialized.database().snapshot().await.unwrap();
    assert!(
        OccurrenceScan::open(&uninitialized)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        untouched,
        uninitialized.database().snapshot().await.unwrap()
    );

    let mut schedule = definition();
    schedule.fire_count = 0;
    agent.upsert_schedule(schedule).await.unwrap();
    for ordinal in 1..=5 {
        prepare(&agent, "wake", slot(ordinal), now()).await.unwrap();
    }
    let mut scan = OccurrenceScan::open(&agent).await.unwrap().unwrap();
    let mut observed = scan.next_page(&agent, 2).await.unwrap().unwrap();
    assert_eq!(observed.len(), 2);
    let later = prepare(&agent, "wake", slot(6), now()).await.unwrap();
    let unavailable = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap());
    let before = operations.snapshot().await.unwrap();
    assert!(scan.next_page(&unavailable, 2).await.is_err());
    assert!(scan.next_page(&uninitialized, 2).await.is_err());
    assert!(scan.next_page(&agent, 0).await.is_err());
    assert!(scan.next_page(&agent, 129).await.is_err());
    while let Some(page) = scan.next_page(&agent, 2).await.unwrap() {
        assert!(page.len() <= 2);
        observed.extend(page);
    }
    observed.sort_by(|a, b| a.reference.occurrence_id.cmp(&b.reference.occurrence_id));
    observed.dedup_by(|a, b| a.reference == b.reference);
    assert_eq!(observed.len(), 5);
    assert!(
        !observed.contains(&later),
        "no mixed-head scan or duplicated cursor page"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());

    let mut corrupt = serde_json::to_value(&later).unwrap();
    corrupt["version"] = json!(99);
    put(&operations, OCCURRENCES, "zz-unsupported", corrupt).await;
    let before = operations.snapshot().await.unwrap();
    let mut scan = OccurrenceScan::open(&agent).await.unwrap().unwrap();
    let mut good = 0;
    loop {
        match scan.next_page(&agent, 2).await {
            Ok(Some(page)) => good += page.len(),
            Ok(None) => panic!("unknown data must not look like an empty/end-of-scan ledger"),
            Err(_) => break,
        }
    }
    assert_eq!(good, 6);
    assert!(
        scan.next_page(&agent, 2).await.is_err(),
        "failed page did not advance"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
}

#[tokio::test]
async fn legacy_occurrences_remain_readable_but_never_acquire_fresh_creation_rights() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    agent.upsert_schedule(definition()).await.unwrap();
    let row = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(
        super::handoff(&agent, &row.reference, &session, now())
            .await
            .is_err(),
        "an unbound Fresh root cannot be handed off"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let current = serde_json::to_value(&row).unwrap();
    let mut legacy = current.clone();
    legacy["version"] = json!(1);
    legacy.as_object_mut().unwrap().remove("fresh_root");
    put(
        &operations,
        OCCURRENCES,
        &row.reference.occurrence_id,
        legacy,
    )
    .await;
    reconcile_all(&agent, &definition()).await.unwrap();
    assert_eq!(agent.schedule_occurrences().await.unwrap()[0].version, 1);
    let before = operations.snapshot().await.unwrap();
    assert!(fresh::request(&agent, &row.reference, now()).await.is_err());
    assert!(
        super::handoff(&agent, &row.reference, &session, now())
            .await
            .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    put(
        &operations,
        OCCURRENCES,
        &row.reference.occurrence_id,
        current,
    )
    .await;
    handoff(&agent, &row.reference, &session, now())
        .await
        .unwrap();
    let mut old_history =
        serde_json::to_value(agent.schedule_occurrences().await.unwrap().pop().unwrap()).unwrap();
    old_history["version"] = json!(1);
    old_history.as_object_mut().unwrap().remove("fresh_root");
    old_history
        .as_object_mut()
        .unwrap()
        .remove("publication_sequence");
    put(
        &operations,
        OCCURRENCES,
        &row.reference.occurrence_id,
        old_history.clone(),
    )
    .await;
    reconcile_all(&agent, &definition()).await.unwrap();
    assert_eq!(
        serde_json::to_value(agent.schedule_occurrences().await.unwrap().pop().unwrap()).unwrap(),
        old_history
    );
    assert_eq!(accounting(&agent).await.reservations, 1);
    assert_eq!(accounting(&agent).await.successful_fires, 0);
    assert!(session.attempts_for_test().await.is_empty());
}
