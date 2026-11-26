use super::*;
use std::time::Duration;

async fn must_wait<F: std::future::Future>(future: &mut std::pin::Pin<&mut F>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), future)
            .await
            .is_err(),
        "schedule mutation escaped the shared local boundary"
    );
}

#[tokio::test]
async fn reopened_edit_and_handoff_share_the_same_mutation_boundary() {
    let (_instance, user, agent, operations, mut session) = fixture().await;
    let schedule = definition();
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let row = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(operations.clone());

    let guard = agent.lock_scheduling().await;
    let mut edited = schedule;
    edited.enabled = false;
    let edit = reopened.upsert_schedule(edited);
    tokio::pin!(edit);
    must_wait(&mut edit).await;
    // Queue handoff behind the operator edit on another handle. It must reread
    // authority after acquiring the boundary, not use the pre-wait definition.
    let handoff = agent.handoff_schedule_occurrence(&row.reference, &session, now());
    tokio::pin!(handoff);
    must_wait(&mut handoff).await;
    drop(guard);
    tokio::time::timeout(Duration::from_secs(5), &mut edit)
        .await
        .unwrap()
        .unwrap();
    assert!(handoff.await.is_err());
    assert_eq!(accounting(&agent).await.handoff_count, 0);
    assert_eq!(accounting(&agent).await.reservations, 1);
    assert!(
        session
            .schedule_delivery_evidence(&row.reference)
            .await
            .unwrap()
            .is_none()
    );
    agent
        .release_schedule_preparation(&row.reference)
        .await
        .unwrap();
    assert_eq!(accounting(&agent).await.fire_count, 7);
    assert!(!agent.find_schedule("wake").await.unwrap().unwrap().enabled);
}

#[tokio::test]
async fn concurrent_reopened_admissions_cannot_oversubscribe_the_last_slot() {
    let (_instance, user, agent, operations, _session) = fixture().await;
    agent.upsert_schedule(definition()).await.unwrap();
    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(operations);
    reconcile_all(&agent, &definition()).await.unwrap();
    let revision = definition().revision_key().unwrap();
    let first = agent.prepare_schedule_occurrence("wake", &revision, slot(1), now());
    let second = reopened.prepare_schedule_occurrence("wake", &revision, slot(2), now());
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(first, second)
    })
    .await
    .unwrap();
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let state = accounting(&agent).await;
    assert_eq!((state.fire_count, state.reservations), (8, 1));
    assert_eq!(agent.schedule_occurrences().await.unwrap().len(), 1);
}

#[tokio::test]
async fn remove_release_and_settlement_wait_but_unrelated_agent_does_not() {
    let (_instance, user, agent, operations, mut session) = fixture().await;
    let (row, attempt) = super::settlement::admitted(&agent, &mut session, false).await;
    super::settlement::included(&session, &row, &attempt).await;
    super::settlement::finish(
        &mut session,
        &attempt,
        crate::session::outcome::TurnOutcome::Success,
    )
    .await;
    let reopened = AgentDb::from_database(user.open_database(&agent.id()).await.unwrap())
        .with_scheduling_database(operations);
    let (_other_instance, _other_user, other, _other_operations, _other_session) = fixture().await;
    let guard = agent.lock_scheduling().await;
    let remove = reopened.remove_schedule("wake");
    let settle = agent.settle_schedule_occurrence(&row.reference, &session);
    let release = agent.release_schedule_preparation(&row.reference);
    tokio::pin!(remove, settle, release);
    must_wait(&mut remove).await;
    must_wait(&mut settle).await;
    must_wait(&mut release).await;
    tokio::time::timeout(Duration::from_secs(5), other.upsert_schedule(definition()))
        .await
        .unwrap()
        .unwrap();
    drop(guard);
    assert!(remove.await.unwrap());
    assert!(settle.await.unwrap().is_some());
    assert!(
        release.await.is_err(),
        "a handoff cannot be refunded as unused preparation"
    );
    let state = operations_state_after_removal(&agent, &row).await;
    assert_eq!((state.successful_fires, state.reservations), (1, 0));
}

async fn operations_state_after_removal(agent: &AgentDb, row: &Occurrence) -> AccountingState {
    agent
        .scheduling_database()
        .await
        .unwrap()
        .unwrap()
        .new_transaction()
        .await
        .unwrap()
        .get_store::<Table<AccountingState>>(STATE)
        .await
        .unwrap()
        .get(&row.reference.lifetime)
        .await
        .unwrap()
}
