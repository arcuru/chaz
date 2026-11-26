use super::*;

async fn discard_cache(agent: &AgentDb) {
    // Simulate losing all disposable process-local observations. The native
    // ledger, including reservations, is deliberately left untouched.
    *agent.lock_scheduling().await = AccountingCache::default();
}

#[tokio::test]
async fn accounting_pages_never_admit_partial_history_and_known_deltas_stay_certified() {
    let (_instance, mut user, agent, operations, _session) = fixture().await;
    let mut schedule = definition();
    schedule.fire_count = 0;
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    for ordinal in 1..=5 {
        prepare(&agent, "wake", slot(ordinal), now()).await.unwrap();
    }
    discard_cache(&agent).await;
    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(user.open_database(operations.root_id()).await.unwrap());
    let before = operations.snapshot().await.unwrap();
    let revision = schedule.revision_key().unwrap();
    assert!(
        reopened
            .prepare_schedule_occurrence("wake", &revision, slot(6), now())
            .await
            .is_err()
    );
    assert!(
        agent
            .reconcile_schedule_accounting(&schedule, 0)
            .await
            .is_err()
    );
    assert!(
        agent
            .reconcile_schedule_accounting(&schedule, 129)
            .await
            .is_err()
    );
    let mut scanned = 0;
    let mut calls = 0;
    loop {
        let progress = reopened
            .reconcile_schedule_accounting(&schedule, 1)
            .await
            .unwrap();
        assert!(progress.scanned <= 1);
        scanned += progress.scanned;
        calls += 1;
        assert_eq!(
            before,
            operations.snapshot().await.unwrap(),
            "audit is read-only"
        );
        if progress.complete {
            break;
        }
        assert!(
            reopened
                .prepare_schedule_occurrence("wake", &revision, slot(6), now())
                .await
                .is_err(),
            "a partial page is not accounting authority"
        );
        assert!(calls < 50);
    }
    assert_eq!(
        scanned, 6,
        "five occurrences plus exact state; empty phases stay explicit"
    );
    assert!(calls > 5);
    let sixth = reopened
        .prepare_schedule_occurrence("wake", &revision, slot(6), now())
        .await
        .unwrap();
    let seventh = agent
        .prepare_schedule_occurrence("wake", &revision, slot(7), now())
        .await
        .unwrap();
    assert_ne!(sixth.reference, seventh.reference);
    assert_eq!(
        agent
            .reconcile_schedule_accounting(&schedule, 1)
            .await
            .unwrap(),
        AccountingProgress {
            scanned: 0,
            complete: true
        }
    );
    super::super::release_prepared(&reopened, &sixth.reference)
        .await
        .unwrap();
    assert_eq!(accounting(&agent).await.reservations, 6);
    assert_eq!(
        agent
            .reconcile_schedule_accounting(&schedule, 1)
            .await
            .unwrap()
            .scanned,
        0
    );
    // No selection or initialization side effect for a missing operational ref.
    let (uninitialized, _) = create_agent_db(
        &mut user,
        "uninitialized",
        &AgentDbConfig::default(),
        &AgentMeta::default(),
    )
    .await
    .unwrap();
    let before = uninitialized.database().snapshot().await.unwrap();
    assert!(
        uninitialized
            .reconcile_schedule_accounting(&schedule, 1)
            .await
            .is_err()
    );
    assert_eq!(before, uninitialized.database().snapshot().await.unwrap());
}

#[tokio::test]
async fn unexpected_head_and_shared_lifetime_cannot_reuse_accounting_certificates() {
    let (_instance, _user, agent, operations, _session) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    reconcile_all(&agent, &schedule).await.unwrap();
    // A write not made by a known lifecycle delta invalidates even a completed
    // audit. Blindly accepting the cached baseline would overwrite this bound.
    put(
        &operations,
        STATE,
        &schedule.lifetime_key(),
        json!({"fire_count":8,"consumed":false,"retired":false}),
    )
    .await;
    let before = operations.snapshot().await.unwrap();
    let error = agent
        .prepare_schedule_occurrence("wake", &schedule.revision_key().unwrap(), slot(1), now())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("reconciliation required"),
        "{error:#}"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());
    reconcile_all(&agent, &schedule).await.unwrap();
    assert!(
        agent
            .prepare_schedule_occurrence("wake", &schedule.revision_key().unwrap(), slot(1), now())
            .await
            .is_err()
    );
    assert!(occurrences(&agent).await.unwrap().is_empty());

    let mut other = schedule.clone();
    other.id = "another-definition".into();
    agent.upsert_schedule(other.clone()).await.unwrap();
    let error = agent
        .prepare_schedule_occurrence(&other.id, &other.revision_key().unwrap(), slot(1), now())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("lifetime is shared"),
        "{error:#}"
    );
    assert_eq!(before, operations.snapshot().await.unwrap());

    // Even a write outside the schedule tables invalidates an exact-head
    // observation; it is never silently re-labelled as an audited snapshot.
    put(&operations, "unrelated", "row", json!({"value":1})).await;
    assert!(
        !agent
            .reconcile_schedule_accounting(&schedule, 1)
            .await
            .unwrap()
            .complete
    );
    assert!(
        agent
            .prepare_schedule_occurrence("wake", &schedule.revision_key().unwrap(), slot(1), now())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn malformed_later_accounting_page_holds_without_skipping_or_resetting() {
    let (_instance, _user, agent, operations, _session) = fixture().await;
    let mut schedule = definition();
    schedule.fire_count = 0;
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let mut rows = Vec::new();
    for ordinal in 1..=4 {
        rows.push(prepare(&agent, "wake", slot(ordinal), now()).await.unwrap());
    }
    let mut corrupt = serde_json::to_value(&rows[0]).unwrap();
    corrupt["version"] = json!(99);
    put(&operations, OCCURRENCES, "zz-unsupported", corrupt).await;
    let before = operations.snapshot().await.unwrap();
    assert_eq!(
        agent
            .reconcile_schedule_accounting(&schedule, 2)
            .await
            .unwrap()
            .scanned,
        2
    );
    assert_eq!(
        agent
            .reconcile_schedule_accounting(&schedule, 2)
            .await
            .unwrap()
            .scanned,
        2
    );
    for _ in 0..2 {
        assert!(
            agent
                .reconcile_schedule_accounting(&schedule, 2)
                .await
                .is_err(),
            "failed page must not advance"
        );
        assert!(
            agent
                .prepare_schedule_occurrence(
                    "wake",
                    &schedule.revision_key().unwrap(),
                    slot(5),
                    now()
                )
                .await
                .is_err()
        );
        assert_eq!(before, operations.snapshot().await.unwrap());
    }
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<Occurrence>>(OCCURRENCES)
        .await
        .unwrap()
        .delete("zz-unsupported")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    reconcile_all(&agent, &schedule).await.unwrap();
    assert_eq!(accounting(&agent).await.reservations, 4);
    agent
        .prepare_schedule_occurrence("wake", &schedule.revision_key().unwrap(), slot(5), now())
        .await
        .unwrap();
    assert_eq!(accounting(&agent).await.reservations, 5);
}
