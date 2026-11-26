use super::*;
use crate::agent_db::{AgentDbConfig, AgentMeta, ExecutorRef, create_agent_db};
use serde_json::json;

async fn fixture() -> (
    eidetica::Instance,
    eidetica::AccountSession,
    AgentDb,
    Database,
) {
    let (instance, mut user, _) = crate::session::test_helpers::test_session_db().await;
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
    (
        instance,
        user,
        agent.with_scheduling_database(operations.clone()),
        operations,
    )
}

fn definition() -> Schedule {
    let mut schedule = Schedule::new(
        "wake",
        crate::routine::Trigger::Interval {
            period: std::time::Duration::from_secs(60),
        },
        "PRIVATE-WAKE-MATERIAL",
        ScheduleTarget::Fresh,
    );
    schedule.lifetime = Some("lifetime".into());
    schedule
}

fn reference(agent: &AgentDb, schedule: &Schedule) -> ScheduleReference {
    ScheduleReference {
        version: 1,
        owner_agent_db_id: agent.id().to_string(),
        schedule_id: schedule.id.clone(),
        lifetime: schedule.lifetime_key(),
        revision: schedule.revision_key().unwrap(),
        occurrence_id: "occurrence".into(),
        delivery_id: "delivery".into(),
    }
}

async fn put(db: &Database, store: &str, key: &str, raw: serde_json::Value) {
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<serde_json::Value>>(store)
        .await
        .unwrap()
        .set(key, raw)
        .await
        .unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn immutable_revisions_resolve_only_for_current_exact_agent() {
    let (_instance, mut user, agent, operations) = fixture().await;
    let mut schedule = definition();
    schedule.fire_count = 7;
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let first = reference(&agent, &schedule);
    assert_eq!(
        agent
            .resolve_schedule_reference(&first, Utc::now())
            .await
            .unwrap()
            .prompt,
        schedule.prompt
    );
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains(&schedule.prompt)
    );
    let (other, _) = create_agent_db(
        &mut user,
        "other",
        &AgentDbConfig::default(),
        &AgentMeta::default(),
    )
    .await
    .unwrap();
    other.upsert_schedule(schedule.clone()).await.unwrap();
    assert!(
        other
            .resolve_schedule_reference(&first, Utc::now())
            .await
            .unwrap_err()
            .to_string()
            .contains("another Agent")
    );

    // Counter-only historical changes cannot forge a new executable identity.
    let mut counters = schedule.clone();
    counters.fire_count = 8;
    counters.consecutive_failures = 2;
    counters.last_error = Some("legacy failure".into());
    assert_eq!(counters.revision_key().unwrap(), first.revision);
    schedule.prompt = "UPDATED-PRIVATE-MATERIAL".into();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let second = reference(&agent, &schedule);
    assert_ne!(first.revision, second.revision);
    assert_eq!(
        first.lifetime, second.lifetime,
        "edits retain accounting lifetime"
    );
    assert!(
        agent
            .resolve_schedule_reference(&first, Utc::now())
            .await
            .is_err()
    );
    let before = agent.database().snapshot().await.unwrap();
    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap());
    assert_eq!(
        reopened
            .resolve_schedule_reference(&second, Utc::now())
            .await
            .unwrap()
            .prompt,
        schedule.prompt
    );
    assert_eq!(
        before,
        agent.database().snapshot().await.unwrap(),
        "resolver is read only"
    );
    let txn = agent.database().new_transaction().await.unwrap();
    let archived = txn
        .get_store::<Table<crate::agent_db::ScheduleRevision>>("schedule_revisions")
        .await
        .unwrap();
    assert_eq!(
        archived
            .get(&first.revision)
            .await
            .unwrap()
            .definition
            .prompt,
        "PRIVATE-WAKE-MATERIAL"
    );
    assert_eq!(archived.search(|_| true).await.unwrap().len(), 2);
    let txn = operations.new_transaction().await.unwrap();
    assert!(
        txn.get_store::<Table<serde_json::Value>>("schedule_revisions")
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap()
            .is_empty()
    );
    agent.remove_schedule("wake").await.unwrap();
    assert!(
        agent
            .resolve_schedule_reference(&second, Utc::now())
            .await
            .is_err()
    );
    let mut recreated = definition();
    recreated.lifetime = Some("new-lifetime".into());
    agent.upsert_schedule(recreated.clone()).await.unwrap();
    assert!(
        agent
            .resolve_schedule_reference(&second, Utc::now())
            .await
            .is_err()
    );
    let fresh = reference(&agent, &recreated);
    assert!(
        agent
            .resolve_schedule_reference(&fresh, Utc::now())
            .await
            .is_ok()
    );
    recreated.enabled = false;
    agent.upsert_schedule(recreated).await.unwrap();
    assert!(
        agent
            .resolve_schedule_reference(&fresh, Utc::now())
            .await
            .is_err()
    );
    let txn = agent.database().new_transaction().await.unwrap();
    let mut conflict = definition();
    conflict.prompt = "conflicting private material".into();
    txn.get_store::<Table<Schedule>>(crate::agent_db::SCHEDULES_STORE)
        .await
        .unwrap()
        .insert(conflict)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = agent.database().snapshot().await.unwrap();
    assert!(preview(&agent).await.is_err());
    assert!(agent.find_schedule("wake").await.is_err());
    assert_eq!(before, agent.database().snapshot().await.unwrap());
}

#[test]
fn occurrence_identity_separates_slot_owner_revision_and_lifetime() {
    let schedule = definition();
    let material = r#"{"enabled":true,"id":"wake","lifetime":"lifetime","max_failures":3,"prompt":"PRIVATE-WAKE-MATERIAL","target":{"kind":"fresh"},"trigger":{"kind":"interval","period":{"nanos":0,"secs":60}}}"#;
    assert_eq!(
        schedule.revision_key().unwrap(),
        format!("v1:{}", blake3::hash(material.as_bytes())),
        "immutable revision encoding changed"
    );
    let now = DateTime::parse_from_rfc3339("2026-11-06T12:00:00Z")
        .unwrap()
        .to_utc();
    let slot = DueSlot::Interval {
        anchor: now,
        ordinal: 3,
    };
    let id = occurrence_id("agent", &schedule, &slot).unwrap();
    assert_eq!(id, occurrence_id("agent", &schedule, &slot).unwrap());
    let mut counter = schedule.clone();
    counter.fire_count = 4;
    assert_eq!(id, occurrence_id("agent", &counter, &slot).unwrap());
    let mut edited = schedule.clone();
    edited.max_fires = Some(4);
    assert_ne!(id, occurrence_id("agent", &edited, &slot).unwrap());
    let mut replacement = schedule.clone();
    replacement.lifetime = Some("replacement".into());
    assert_ne!(id, occurrence_id("agent", &replacement, &slot).unwrap());
    assert_ne!(id, occurrence_id("other-agent", &schedule, &slot).unwrap());
    for different in [
        DueSlot::Calendar { due_at: now },
        DueSlot::Interval {
            anchor: now,
            ordinal: 4,
        },
    ] {
        assert_ne!(id, occurrence_id("agent", &schedule, &different).unwrap());
    }
    let wire = serde_json::to_value(&slot).unwrap();
    assert_eq!(
        wire,
        json!({"Interval":{"anchor":"2026-11-06T12:00:00Z","ordinal":3}})
    );
    assert_eq!(serde_json::from_value::<DueSlot>(wire).unwrap(), slot);
}

#[tokio::test]
async fn preview_preserves_legacy_baselines_and_never_initializes_operations() {
    let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
    let agent = AgentDb::from_database(db.clone());
    let mut schedule = definition();
    schedule.lifetime = None;
    schedule.fire_count = 7;
    schedule.enabled = false;
    agent.upsert_schedule(schedule).await.unwrap();
    let before = db.snapshot().await.unwrap();
    let rows = preview(&agent).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].legacy_identity);
    assert!(!rows[0].enabled);
    assert!(!rows[0].operational_initialized);
    assert_eq!(
        rows[0].accounting,
        AccountingView::Legacy {
            capacity_used: 7,
            consumed: false,
            retired: false
        }
    );
    assert!(
        !serde_json::to_string(&rows)
            .unwrap()
            .contains("PRIVATE-WAKE")
    );
    assert!(agent.scheduling_reference().await.unwrap().is_none());
    assert_eq!(before, db.snapshot().await.unwrap());
}

#[tokio::test]
async fn schedule_migration_preserves_legacy_capacity_and_holds_past_one_shots() {
    use crate::agent_db::SchedulePolicyOrigin;
    use crate::routine::{RoutineEngine, Trigger};

    let (_instance, _user, agent, operations) = fixture().await;
    let now = Utc::now();
    let mut recurring = definition();
    recurring.lifetime = None;
    recurring.fire_count = 7;
    recurring.max_fires = Some(10);
    agent.upsert_schedule(recurring.clone()).await.unwrap();
    let reserved = super::super::reserve(&operations, &recurring, now)
        .await
        .unwrap()
        .unwrap();

    let mut past = recurring.clone();
    past.id = "old-past".into();
    past.trigger = Trigger::OneShot {
        fire_at: now - chrono::Duration::minutes(1),
    };
    agent.upsert_schedule(past.clone()).await.unwrap();
    let mut future = past.clone();
    future.id = "old-future".into();
    future.trigger = Trigger::OneShot {
        fire_at: now + chrono::Duration::hours(1),
    };
    agent.upsert_schedule(future.clone()).await.unwrap();

    let original_operations = operations.snapshot().await.unwrap();
    assert_eq!(agent.migrate_legacy_schedules(now).await.unwrap(), 3);
    let migrated = agent.find_schedule(&recurring.id).await.unwrap().unwrap();
    assert_eq!(
        migrated.policy_origin,
        Some(SchedulePolicyOrigin::LegacyMigrated)
    );
    assert_eq!(migrated.lifetime_key(), reserved.lifetime);
    assert_eq!(migrated.fire_count, 7);
    let history = preview(&agent).await.unwrap();
    let row = history
        .iter()
        .find(|row| row.schedule_id == recurring.id)
        .unwrap();
    assert_eq!(row.pending_legacy_reservations, 1);
    assert!(matches!(
        row.accounting,
        AccountingView::Legacy {
            capacity_used: 8,
            ..
        }
    ));
    assert_eq!(operations.snapshot().await.unwrap(), original_operations);
    past.policy_origin = Some(SchedulePolicyOrigin::LegacyOneShotHeld);
    assert_eq!(
        agent.find_schedule(&past.id).await.unwrap(),
        Some(past.clone())
    );
    let held = agent.effective_schedule("old-past").await.unwrap().unwrap();
    assert!(!held.enabled);
    assert_eq!(
        held.last_error.as_deref(),
        Some(super::super::LEGACY_ONE_SHOT_HELD)
    );
    let engine = RoutineEngine::new(operations.clone(), None).await.unwrap();
    engine
        .register_agent(&agent.id().to_string(), &agent)
        .await
        .unwrap();
    let scheduled = engine.list_routines().await;
    assert!(
        scheduled
            .iter()
            .any(|(_, timer)| timer.name == future.id && timer.trigger == future.trigger)
    );
    assert!(!scheduled.iter().any(|(_, timer)| timer.name == "old-past"));
    past.trigger = future.trigger.clone();
    agent.upsert_schedule(past).await.unwrap();
    let migrated_snapshot = agent.database().snapshot().await.unwrap();
    assert_eq!(agent.migrate_legacy_schedules(now).await.unwrap(), 0);
    assert_eq!(
        agent.database().snapshot().await.unwrap(),
        migrated_snapshot
    );
    engine
        .reload_agent(&agent.id().to_string(), &agent)
        .await
        .unwrap();
    assert!(
        !engine
            .list_routines()
            .await
            .iter()
            .any(|(_, timer)| timer.name == "old-past")
    );
}

#[tokio::test]
async fn versioned_accounting_is_readable_but_not_executable_by_legacy_writers() {
    let (_instance, _user, agent, operations) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let key = schedule.lifetime_key();
    let raw = json!({"version":2,"fire_count":10,"legacy_baseline":7,"successful_fires":1,
        "reservations":2,"handoff_count":6,"consumed":false,"retired":false});
    put(&operations, STATE, &key, raw.clone()).await;
    let before = operations.snapshot().await.unwrap();
    let rows = preview(&agent).await.unwrap();
    let AccountingView::Current(view) = &rows[0].accounting else {
        panic!("recognized current accounting")
    };
    assert_eq!(
        (
            view.legacy_baseline,
            view.successful_fires,
            view.reservations,
            view.handoff_count
        ),
        (7, 1, 2, 6)
    );
    assert_eq!(view.fire_count, 10);
    assert!(!view.consumed);
    assert!(
        super::super::reserve(&operations, &schedule, Utc::now())
            .await
            .is_err(),
        "reader-first release cannot execute a new ledger through old writer"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    for invalid in [
        {
            let mut r = raw.clone();
            r["version"] = json!(99);
            r
        },
        {
            let mut r = raw.clone();
            r["fire_count"] = json!(0);
            r
        },
        {
            let mut r = raw.clone();
            r["successful_fires"] = json!(u32::MAX);
            r
        },
        {
            let mut r = raw.clone();
            r["unrecognized_policy"] = json!(true);
            r
        },
        {
            let mut r = raw.clone();
            r.as_object_mut().unwrap().remove("reservations");
            r
        },
    ] {
        put(&operations, STATE, &key, invalid.clone()).await;
        let before = operations.snapshot().await.unwrap();
        assert!(preview(&agent).await.is_err());
        assert!(
            super::super::reserve(&operations, &schedule, Utc::now())
                .await
                .is_err()
        );
        assert_eq!(before, operations.snapshot().await.unwrap());
        let txn = operations.new_transaction().await.unwrap();
        assert_eq!(
            txn.get_store::<Table<serde_json::Value>>(STATE)
                .await
                .unwrap()
                .get(&key)
                .await
                .unwrap(),
            invalid
        );
    }
}

#[tokio::test]
async fn held_legacy_reservations_are_not_counted_again_or_refunded_by_preview() {
    let (_instance, _user, agent, operations) = fixture().await;
    let mut schedule = definition();
    schedule.fire_count = 7;
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let reservation = super::super::reserve(&operations, &schedule, Utc::now())
        .await
        .unwrap()
        .unwrap();
    let before = operations.snapshot().await.unwrap();
    let row = preview(&agent).await.unwrap().pop().unwrap();
    assert_eq!(row.pending_legacy_reservations, 1);
    assert_eq!(
        row.accounting,
        AccountingView::Legacy {
            capacity_used: 8,
            consumed: false,
            retired: false
        }
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<ScheduleState>>(STATE)
        .await
        .unwrap()
        .delete(&reservation.lifetime)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        preview(&agent)
            .await
            .unwrap_err()
            .to_string()
            .contains("state missing")
    );
    put(
        &operations,
        STATE,
        &schedule.lifetime_key(),
        json!({"fire_count":8,"consumed":false,"retired":false}),
    )
    .await;
    super::super::handoff(
        &operations,
        &reservation,
        ScheduleFire {
            schedule_id: schedule.id.clone(),
            firing_id: Some(reservation.firing_id.clone()),
            lifetime: Some(reservation.lifetime.clone()),
            request_id: None,
            attempt_id: None,
            fired_at: Utc::now(),
            session_db_id: "target".into(),
            fresh: true,
            usage: None,
        },
    )
    .await
    .unwrap();
    let row = preview(&agent).await.unwrap().pop().unwrap();
    assert_eq!(
        (row.pending_legacy_reservations, row.recorded_handoffs),
        (0, 1)
    );
    put(
        &operations,
        STATE,
        &schedule.lifetime_key(),
        json!({"fire_count":0,"consumed":false,"retired":false}),
    )
    .await;
    assert!(
        preview(&agent).await.is_err(),
        "history cannot be reset to an empty count"
    );
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<ScheduleState>>(STATE)
        .await
        .unwrap()
        .delete(&reservation.lifetime)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        preview(&agent).await.is_err(),
        "handoff without state is unknown, not a fresh ledger"
    );
}

#[tokio::test]
async fn malformed_or_future_private_references_never_resolve() {
    let (_instance, _user, agent, _) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let valid = reference(&agent, &schedule);
    assert!(
        agent
            .resolve_schedule_reference(&valid, Utc::now())
            .await
            .is_ok()
    );
    let txn = agent.database().new_transaction().await.unwrap();
    let archive = txn
        .get_store::<Table<serde_json::Value>>("schedule_revisions")
        .await
        .unwrap();
    let original = archive.get(&valid.revision).await.unwrap();
    let mut future_row = original.clone();
    future_row["version"] = json!(99);
    archive.set(&valid.revision, future_row).await.unwrap();
    txn.commit().await.unwrap();
    assert!(
        agent
            .resolve_schedule_reference(&valid, Utc::now())
            .await
            .is_err()
    );
    put(
        agent.database(),
        "schedule_revisions",
        &valid.revision,
        original,
    )
    .await;
    let mut future = valid.clone();
    future.version = 99;
    assert!(
        agent
            .resolve_schedule_reference(&future, Utc::now())
            .await
            .is_err()
    );
    let mut wrong_schedule = valid.clone();
    wrong_schedule.schedule_id = "other".into();
    assert!(
        agent
            .resolve_schedule_reference(&wrong_schedule, Utc::now())
            .await
            .is_err()
    );
    let mut wrong = valid.clone();
    wrong.lifetime = "wrong".into();
    assert!(
        agent
            .resolve_schedule_reference(&wrong, Utc::now())
            .await
            .is_err()
    );
    let mut expired = schedule;
    expired.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
    agent.upsert_schedule(expired.clone()).await.unwrap();
    assert!(
        agent
            .resolve_schedule_reference(&reference(&agent, &expired), Utc::now())
            .await
            .is_err()
    );
    let mut incomplete = valid.clone();
    incomplete.delivery_id.clear();
    assert!(
        agent
            .resolve_schedule_reference(&incomplete, Utc::now())
            .await
            .is_err()
    );
    let mut raw = serde_json::to_value(&valid).unwrap();
    raw["prompt"] = json!("untrusted");
    assert!(serde_json::from_value::<ScheduleReference>(raw).is_err());
}
