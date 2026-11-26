use super::*;
use serde_json::Value;

#[tokio::test]
async fn unrelated_executor_writes_do_not_starve_partial_accounting_or_lose_reservations() {
    let (_instance, _user, agent, operations, _session) = fixture().await;
    let mut schedule = definition();
    schedule.fire_count = 0;
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    for ordinal in 1..=5 {
        prepare(&agent, "wake", slot(ordinal), now()).await.unwrap();
    }
    // Register before the audit: changing the store registry itself requires
    // re-audit, whereas ordinary writes to this job table do not.
    put(&operations, "jobs", "activity", json!(0)).await;
    *agent.lock_scheduling().await = AccountingCache::default();
    let mut complete = false;
    let mut scanned = 0;
    for ordinal in 1..=15 {
        put(&operations, "jobs", "activity", json!(ordinal)).await;
        let progress = agent
            .reconcile_schedule_accounting(&schedule, 1)
            .await
            .unwrap();
        assert!(progress.scanned <= 1);
        scanned += progress.scanned;
        if progress.complete {
            complete = true;
            break;
        }
        assert!(
            agent
                .prepare_schedule_occurrence(
                    "wake",
                    &schedule.revision_key().unwrap(),
                    slot(6),
                    now()
                )
                .await
                .is_err()
        );
    }
    assert!(
        complete,
        "unrelated writes must not repeatedly discard audited pages"
    );
    assert_eq!(scanned, 6);
    put(&operations, "jobs", "activity", json!(99)).await;
    agent
        .prepare_schedule_occurrence("wake", &schedule.revision_key().unwrap(), slot(6), now())
        .await
        .unwrap();
    assert_eq!(accounting(&agent).await.reservations, 6);
    assert_eq!(
        operations
            .new_transaction()
            .await
            .unwrap()
            .get_store::<Table<Value>>("jobs")
            .await
            .unwrap()
            .get("activity")
            .await
            .unwrap(),
        json!(99)
    );
}

#[tokio::test]
async fn bounded_refresh_checks_all_branches_and_never_certifies_an_unchecked_head() {
    let (_instance, _user, agent, operations, _session) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    put(&operations, "jobs", "activity", json!(0)).await;
    reconcile_all(&agent, &schedule).await.unwrap();
    let boundary = operations.snapshot().await.unwrap();
    for ordinal in 1..=20 {
        put(&operations, "jobs", "activity", json!(ordinal)).await;
    }
    assert!(
        agent
            .prepare_schedule_occurrence("wake", &schedule.revision_key().unwrap(), slot(1), now())
            .await
            .is_err(),
        "mutation may not hide an unbounded refresh"
    );
    reconcile_all(&agent, &schedule).await.unwrap();
    // A divergent scheduling write is not erased by an unrelated newer branch.
    let txn = operations.new_transaction_at(&boundary).await.unwrap();
    txn.get_store::<Table<Value>>(STATE)
        .await
        .unwrap()
        .set(
            &schedule.lifetime_key(),
            json!({"fire_count":8,"consumed":false,"retired":false}),
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    put(&operations, "jobs", "activity", json!(21)).await;
    let before = operations.snapshot().await.unwrap();
    assert!(
        agent
            .prepare_schedule_occurrence("wake", &schedule.revision_key().unwrap(), slot(1), now())
            .await
            .is_err()
    );
    reconcile_all(&agent, &schedule).await.unwrap();
    assert!(
        agent
            .prepare_schedule_occurrence("wake", &schedule.revision_key().unwrap(), slot(1), now())
            .await
            .is_err(),
        "the sibling scheduling change exhausted capacity"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert!(occurrences(&agent).await.unwrap().is_empty());
}
