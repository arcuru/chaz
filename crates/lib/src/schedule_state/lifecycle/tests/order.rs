use super::*;

#[tokio::test]
async fn handoff_order_is_atomic_snapshot_pinned_and_not_timestamp_or_hash_order() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    assert!(HandoffScan::open(&agent).await.unwrap().is_none());
    let mut schedule = definition();
    schedule.fire_count = 0;
    schedule.max_fires = None;
    schedule.target = ScheduleTarget::Pinned {
        session_db_id: session.database().root_id().to_string(),
    };
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let mut prepared = Vec::new();
    for ordinal in 0..19 {
        let row = prepare(&agent, "wake", slot(ordinal), now()).await.unwrap();
        session
            .prepare_schedule_input(&row.reference)
            .await
            .unwrap();
        prepared.push(row);
    }
    prepared.sort_by(|a, b| b.reference.occurrence_id.cmp(&a.reference.occurrence_id));
    let mut expected = Vec::new();
    for (index, row) in prepared.iter().take(18).enumerate() {
        let at = now() - chrono::Duration::seconds(index as i64);
        let published = handoff(&agent, &row.reference, &session, at).await.unwrap();
        assert_eq!(published.publication_sequence, Some(index as u64 + 1));
        // Lost acknowledgement returns the exact publication, no new ordinal.
        assert_eq!(
            handoff(&agent, &row.reference, &session, now())
                .await
                .unwrap(),
            published
        );
        expected.push(row.reference.clone());
    }
    let mut scan = HandoffScan::open(&agent).await.unwrap().unwrap();
    let first = scan.next_page(&agent, 16).await.unwrap().unwrap();
    assert_eq!(first.len(), 16);
    let last = &prepared[18];
    handoff(&agent, &last.reference, &session, now())
        .await
        .unwrap();
    let second = scan.next_page(&agent, 16).await.unwrap().unwrap();
    assert_eq!(
        second.len(),
        2,
        "a later publication cannot enter this snapshot"
    );
    assert!(scan.next_page(&agent, 16).await.unwrap().is_none());
    assert_eq!(
        first
            .into_iter()
            .chain(second)
            .map(|row| row.reference)
            .collect::<Vec<_>>(),
        expected
    );
    let reopened = agent.clone().with_scheduling_database(operations.clone());
    let mut scan = HandoffScan::open(&reopened).await.unwrap().unwrap();
    let mut observed = Vec::new();
    while let Some(page) = scan.next_page(&reopened, 3).await.unwrap() {
        assert!(page.len() <= 3);
        observed.extend(page.into_iter().map(|row| row.reference));
    }
    expected.push(last.reference.clone());
    assert_eq!(observed, expected);
    let before = operations.snapshot().await.unwrap();
    assert!(scan.next_page(&agent, 0).await.is_err());
    assert_eq!(before, operations.snapshot().await.unwrap());
    assert!(session.attempts_for_test().await.is_empty());
}

#[tokio::test]
async fn missing_publication_head_or_index_never_resets_another_lifetimes_order() {
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    let mut schedule = definition();
    schedule.target = ScheduleTarget::Pinned {
        session_db_id: session.database().root_id().to_string(),
    };
    agent.upsert_schedule(schedule.clone()).await.unwrap();
    let first = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    session
        .prepare_schedule_input(&first.reference)
        .await
        .unwrap();
    handoff(&agent, &first.reference, &session, now())
        .await
        .unwrap();
    let mut other = schedule.clone();
    other.id = "other".into();
    other.lifetime = Some("other-lifetime".into());
    agent.upsert_schedule(other.clone()).await.unwrap();
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<serde_json::Value>>(super::super::order::HEAD)
        .await
        .unwrap()
        .delete("head")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(HandoffScan::open(&agent).await.is_err());
    assert!(
        reconcile_all(&agent, &other).await.is_err(),
        "an audit of another lifetime must notice missing ordering custody"
    );
    assert!(
        super::super::prepare(
            &agent,
            "other",
            &other.revision_key().unwrap(),
            slot(1),
            now()
        )
        .await
        .is_err()
    );
    assert_eq!(before, operations.snapshot().await.unwrap());

    put(
        &operations,
        super::super::order::HEAD,
        "head",
        json!({"version":1,"owner":agent.id().to_string(),"last":1}),
    )
    .await;
    let txn = operations.new_transaction().await.unwrap();
    txn.get_store::<Table<serde_json::Value>>(super::super::order::HANDOFFS)
        .await
        .unwrap()
        .delete("00000000000000000001")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let mut scan = HandoffScan::open(&agent).await.unwrap().unwrap();
    let before = operations.snapshot().await.unwrap();
    assert!(scan.next_page(&agent, 1).await.is_err());
    assert!(
        scan.next_page(&agent, 1).await.is_err(),
        "a bad page never advances"
    );
    assert!(reconcile_all(&agent, &other).await.is_err());
    assert_eq!(before, operations.snapshot().await.unwrap());
}

#[tokio::test]
async fn old_handoff_reader_refuses_ordered_writes_while_new_reader_keeps_legacy_inert() {
    // This is the actual pre-order strict wire shape, not a permissive Value.
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
        #[serde(default, rename = "fresh_root")]
        _fresh_root: Option<FreshRoot>,
    }
    let (_instance, _user, agent, operations, mut session) = fixture().await;
    agent.upsert_schedule(definition()).await.unwrap();
    let row = prepare(&agent, "wake", slot(1), now()).await.unwrap();
    let wire = serde_json::to_value(&row).unwrap();
    assert!(serde_json::from_value::<PreviousOccurrence>(wire).is_ok());
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    let published = handoff(&agent, &row.reference, &session, now())
        .await
        .unwrap();
    let wire = serde_json::to_value(&published).unwrap();
    assert!(serde_json::from_value::<PreviousOccurrence>(wire.clone()).is_err());
    let mut legacy = wire;
    legacy["version"] = json!(2);
    legacy
        .as_object_mut()
        .unwrap()
        .remove("publication_sequence");
    put(
        &operations,
        OCCURRENCES,
        &row.reference.occurrence_id,
        legacy,
    )
    .await;
    // Losing or changing the indexed occurrence cannot create runnable history.
    let mut scan = HandoffScan::open(&agent).await.unwrap().unwrap();
    assert!(scan.next_page(&agent, 16).await.is_err());
    assert!(session.attempts_for_test().await.is_empty());
}
