use super::*;
use std::collections::HashSet;

fn reference() -> ScheduleReference {
    ScheduleReference {
        version: 1,
        owner_agent_db_id: "agent".into(),
        schedule_id: "wake".into(),
        lifetime: "lifetime".into(),
        revision: "revision".into(),
        occurrence_id: "occurrence".into(),
        delivery_id: "delivery".into(),
    }
}

#[tokio::test]
async fn prepared_ack_is_idempotent_inert_and_contains_only_references() {
    let (_instance, user, db) = test_helpers::test_session_db().await;
    let mut session = Session::new(ConversationId("timer".into()), db.clone()).await;
    Session::initialize_turn_schema(&db).await.unwrap();
    let reference = reference();
    let anchor = session.prepare_schedule_input(&reference).await.unwrap();
    assert_eq!(session.entries().len(), 1);
    assert_eq!(session.entries()[0].entry_type, EntryType::Ack);
    assert_eq!(
        serde_json::from_str::<ScheduleReference>(&session.entries()[0].content).unwrap(),
        reference
    );
    let before = db.snapshot().await.unwrap();
    let reopened = user.open_database(db.root_id()).await.unwrap();
    let mut session = Session::new(ConversationId("timer".into()), reopened).await;
    assert_eq!(
        session.prepare_schedule_input(&reference).await.unwrap(),
        anchor
    );
    assert_eq!(before, db.snapshot().await.unwrap());
    assert!(
        session
            .turn_request(&anchor.request_id, |_| false, &HashSet::new())
            .await
            .unwrap()
            .is_none(),
        "generic retry cannot select an Ack"
    );
    assert!(session.read_turn_attempts().await.unwrap().is_empty());
    assert!(
        session
            .turn_requests(|_| false, &HashSet::new())
            .await
            .unwrap()
            .is_empty()
    );
    // Ordinary input still wakes: preparation is not hiding a broken reader.
    let ordinary = session
        .add_entry(SessionEntry {
            sender: "human".into(),
            content: "ordinary question".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let requests = session
        .turn_requests(|_| false, &HashSet::new())
        .await
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].id, ordinary);
    let context = crate::session::compatibility_tests::context(&session).await;
    let text = format!("{context:?}");
    assert!(text.contains("ordinary question"));
    assert!(!text.contains("occurrence"));
    assert!(!text.contains("revision"));
    assert!(!text.contains("delivery"));
}

#[tokio::test]
async fn corrupt_or_reused_input_identity_refuses_without_new_ack() {
    let (_instance, _user, db) = test_helpers::test_session_db().await;
    let mut session = Session::new(ConversationId("timer".into()), db.clone()).await;
    let reference = reference();
    let anchor = session.prepare_schedule_input(&reference).await.unwrap();
    let mut wrong = reference.clone();
    wrong.owner_agent_db_id = "another-agent".into();
    let before = db.snapshot().await.unwrap();
    assert!(session.prepare_schedule_input(&wrong).await.is_err());
    assert_eq!(before, db.snapshot().await.unwrap());
    // A lost/deleted sidecar cannot mint a second Ack under a new row ID.
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<ScheduleInputAnchor>>(INPUTS)
        .await
        .unwrap()
        .delete(&reference.delivery_id)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = db.snapshot().await.unwrap();
    assert!(session.prepare_schedule_input(&reference).await.is_err());
    assert_eq!(before, db.snapshot().await.unwrap());
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<ScheduleInputAnchor>>(INPUTS)
        .await
        .unwrap()
        .set(&reference.delivery_id, anchor.clone())
        .await
        .unwrap();
    txn.get_store::<Table<SessionEntry>>("entries")
        .await
        .unwrap()
        .delete(anchor.request_id.as_str())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = db.snapshot().await.unwrap();
    assert!(session.prepare_schedule_input(&reference).await.is_err());
    assert_eq!(
        before,
        db.snapshot().await.unwrap(),
        "missing Ack cannot reset input identity"
    );

    for invalid in [
        serde_json::json!({"version":99}),
        {
            let mut value = serde_json::to_value(&anchor).unwrap();
            value["version"] = 99.into();
            value
        },
        {
            let mut value = serde_json::to_value(&anchor).unwrap();
            value["unexpected"] = true.into();
            value
        },
    ] {
        let txn = db.new_transaction().await.unwrap();
        txn.get_store::<Table<serde_json::Value>>(INPUTS)
            .await
            .unwrap()
            .set(&reference.delivery_id, invalid)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let before = db.snapshot().await.unwrap();
        assert!(session.prepare_schedule_input(&reference).await.is_err());
        assert_eq!(before, db.snapshot().await.unwrap());
    }
}

#[tokio::test]
async fn missing_session_write_authority_leaves_no_input_or_started_attempt() {
    let (instance, _user, db) = test_helpers::test_session_db().await;
    let read_only = Database::open(&instance, db.root_id()).await.unwrap();
    let mut session = Session::new(ConversationId("timer".into()), read_only).await;
    let before = db.snapshot().await.unwrap();
    assert!(session.prepare_schedule_input(&reference()).await.is_err());
    assert_eq!(before, db.snapshot().await.unwrap());
    assert!(session.entries().is_empty());
    assert!(session.read_turn_attempts().await.unwrap().is_empty());
}

#[tokio::test]
async fn targeted_start_and_receipt_are_atomic_and_lost_ack_never_restarts() {
    let (instance, user, db) = test_helpers::test_session_db().await;
    let mut session = Session::new(ConversationId("timer".into()), db.clone()).await;
    let reference = reference();
    let anchor = session.prepare_schedule_input(&reference).await.unwrap();
    let before = db.snapshot().await.unwrap();
    let no_key = Session::new(
        ConversationId("timer".into()),
        Database::open(&instance, db.root_id()).await.unwrap(),
    )
    .await;
    assert!(
        no_key
            .start_schedule_input_as(&reference, "unwritable".into(), None)
            .await
            .is_err()
    );
    assert_eq!(before, db.snapshot().await.unwrap());
    assert!(session.read_turn_attempts().await.unwrap().is_empty());
    assert!(
        session
            .schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .is_none()
    );

    let attempt = session
        .start_schedule_input_as(&reference, "initial".into(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attempt.request_id, anchor.request_id);
    let reopened = Session::new(
        ConversationId("timer".into()),
        user.open_database(db.root_id()).await.unwrap(),
    )
    .await;
    let before = db.snapshot().await.unwrap();
    assert!(
        reopened
            .start_schedule_input_as(&reference, "lost-ack-retry".into(), None)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(before, db.snapshot().await.unwrap());
    let evidence = reopened
        .schedule_delivery_evidence(&reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(evidence.input.attempt_id, attempt.attempt_id);
    assert_eq!(evidence.input.request_id, anchor.request_id);
    assert_eq!(evidence.input.state, steering::JobInputState::Queued);
    assert_eq!(reopened.read_turn_attempts().await.unwrap().len(), 1);
    // Even a positively completed request cannot be republished as initial work.
    session.complete_turn_attempt(&attempt, None).await.unwrap();
    assert!(
        session
            .start_schedule_input_as(&reference, "completed-retry".into(), None)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(session.read_turn_attempts().await.unwrap().len(), 1);
    let mut orphan = reference.clone();
    orphan.delivery_id = "orphan-start".into();
    let anchor = session.prepare_schedule_input(&orphan).await.unwrap();
    session.start_turn_attempt(anchor.request_id).await.unwrap();
    let before = db.snapshot().await.unwrap();
    assert!(
        session
            .start_schedule_input_as(&orphan, "must-not-recover-blindly".into(), None)
            .await
            .is_err()
    );
    assert_eq!(before, db.snapshot().await.unwrap());
    assert_eq!(session.read_turn_attempts().await.unwrap().len(), 2);
}
