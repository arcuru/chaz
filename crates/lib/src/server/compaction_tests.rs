use super::*;

const COMPACT_SUMMARY: &str = "SUMMARY-1: request one handled";

/// Plain text of every user/assistant message a backend call carried.
fn sent_texts(call: &crate::test_support::RecordedCall) -> Vec<String> {
    use crate::runtime::RuntimeMessage;
    call.messages
        .iter()
        .filter_map(|message| match message {
            RuntimeMessage::User(text) | RuntimeMessage::Assistant(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn sent_position(call: &crate::test_support::RecordedCall, needle: &str) -> Option<usize> {
    sent_texts(call)
        .iter()
        .position(|text| text.contains(needle))
}

async fn compact_tool_fixture() -> (
    Instance,
    Arc<Server>,
    Arc<crate::session::SessionRegistry>,
    DbEntry,
    eidetica::Database,
    String,
) {
    let (instance, server, registry) = server_fixture().await;
    server.tools.register(crate::tools::Compact);
    let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
    register_alpha_agent_runtime(&server);
    let (_conv, db) = registry.create_session(Some("compact")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .attach_agent_to_session(&sid, &entry)
        .await
        .unwrap();
    (instance, server, registry, entry, db, sid)
}

fn push_compact_call(mock: &crate::test_support::MockBackend) {
    mock.push_tool_calls(vec![(
        "call-compact".into(),
        "compact".into(),
        serde_json::json!({ "summary": COMPACT_SUMMARY }).to_string(),
    )]);
}

#[tokio::test]
async fn request_queued_before_compact_tool_summary_reaches_its_own_turn() {
    use crate::test_support::MockBackend;

    let (_instance, server, registry, _entry, db, sid) = compact_tool_fixture().await;
    let mock = Arc::new(MockBackend::new());
    push_compact_call(&mock);
    mock.push_text("FINAL-1");
    mock.push_text("FINAL-2");
    mock.push_text("FINAL-3");
    let gate = mock.block_next_call();
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid, "REQUEST-1").await;
    tokio::time::timeout(std::time::Duration::from_secs(2), gate.wait_started())
        .await
        .expect("turn one must start");
    // Written while turn one runs, before its compact call writes the Summary.
    write_user_message_with_content(&db, &sid, "REQUEST-2").await;
    gate.release();
    await_call_count(&mock, 3).await;
    await_no_processing(&server, &sid).await;

    let calls = mock.recorded_calls();
    let own_turn = &calls[2];
    let summary = sent_position(own_turn, COMPACT_SUMMARY).expect("summary is the boundary");
    let request = sent_position(own_turn, "REQUEST-2").expect("queued request reaches its turn");
    let reply = sent_position(own_turn, "FINAL-1").expect("previous reply stays visible");
    assert!(
        summary < request && request < reply,
        "session order is kept after the boundary: {:?}",
        sent_texts(own_turn)
    );
    assert!(
        sent_position(own_turn, "REQUEST-1").is_none(),
        "completed history stays compacted: {:?}",
        sent_texts(own_turn)
    );

    // Negative control: once answered, both earlier requests stay compacted.
    write_user_message_with_content(&db, &sid, "REQUEST-3").await;
    await_call_count(&mock, 4).await;
    await_no_processing(&server, &sid).await;
    let next_turn = &mock.recorded_calls()[3];
    assert!(sent_position(next_turn, COMPACT_SUMMARY).is_some());
    assert!(sent_position(next_turn, "REQUEST-3").is_some());
    for hidden in ["REQUEST-1", "REQUEST-2"] {
        assert!(
            sent_position(next_turn, hidden).is_none(),
            "{hidden} must stay behind the summary: {:?}",
            sent_texts(next_turn)
        );
    }
}

#[tokio::test]
async fn retried_request_is_not_hidden_by_its_own_compact_summary() {
    use crate::test_support::MockBackend;

    let (_instance, server, registry, entry, db, sid) = compact_tool_fixture().await;
    let mock = Arc::new(MockBackend::new());
    push_compact_call(&mock);
    mock.push_text("must not commit after shutdown");
    let compact_gate = mock.block_next_call();
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    let request_id = write_user_message_with_content(&db, &sid, "REQUEST-1").await;
    let timeout = std::time::Duration::from_secs(2);
    tokio::time::timeout(timeout, compact_gate.wait_started())
        .await
        .expect("turn must start");
    let final_gate = mock.block_next_call();
    compact_gate.release();
    tokio::time::timeout(timeout, final_gate.wait_started())
        .await
        .expect("post-compact call must start");
    tokio::time::timeout(timeout, server.shutdown())
        .await
        .expect("shutdown must cancel the turn");
    tokio::time::timeout(timeout, final_gate.wait_stopped())
        .await
        .expect("post-compact call must be cancelled");
    drop(server);

    let observed = Session::new(ConversationId(sid.clone()), db.clone()).await;
    assert!(
        observed
            .entries()
            .iter()
            .any(|entry| entry.entry_type == EntryType::Summary),
        "the compact side effect is already durable"
    );

    let (_instance2, restarted, _) = server_fixture_from_registry(registry.clone()).await;
    register_alpha_agent_runtime(&restarted);
    restarted.agent_index().register(entry);
    let retry_mock = Arc::new(MockBackend::new());
    retry_mock.push_text("RETRY-FINAL");
    let backend = crate::backends::BackendManager::with_mock(
        retry_mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    restarted
        .register_session(&db, backend, Some("alpha".into()), None)
        .await
        .unwrap();
    restarted
        .retry_interrupted_turn(&sid, &request_id)
        .await
        .unwrap();
    await_call_count(&retry_mock, 1).await;
    await_no_processing(&restarted, &sid).await;

    let retried = &retry_mock.recorded_calls()[0];
    let summary = sent_position(retried, COMPACT_SUMMARY).expect("summary is the boundary");
    let request = sent_position(retried, "REQUEST-1").expect("retried request is visible");
    assert!(summary < request, "request renders after the boundary");
}

#[tokio::test]
async fn turn_context_keeps_only_current_and_queued_requests_past_a_summary() {
    let (_instance, _server, registry) = server_fixture().await;
    let (_conv, db) = registry.create_session(Some("rule")).await.unwrap();
    let sid = db.root_id().to_string();
    let completed = write_user_message_with_content(&db, &sid, "COMPLETED").await;
    let interrupted = write_user_message_with_content(&db, &sid, "INTERRUPTED").await;
    let current = write_user_message_with_content(&db, &sid, "CURRENT").await;
    let queued = write_user_message_with_content(&db, &sid, "QUEUED").await;
    let mut session = Session::new(ConversationId(sid.clone()), db.clone()).await;
    let attempt = session.start_turn_attempt(completed).await.unwrap();
    session
        .complete_turn_attempt(
            &attempt,
            Some(SessionEntry {
                sender: "alpha".into(),
                content: "AGENT-REPLY".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            }),
        )
        .await
        .unwrap();
    session.start_turn_attempt(interrupted).await.unwrap();
    session.start_turn_attempt(current.clone()).await.unwrap();
    session
        .add_entry(SessionEntry {
            sender: "alpha".into(),
            content: "SUMMARY".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Summary,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid, "LATER").await;

    let mut session = Session::new(ConversationId(sid), db).await;
    let snapshot = session.database().snapshot().await.unwrap();
    let view = session
        .turn_context_view_at(snapshot.clone(), &current, |name| name == "alpha")
        .await
        .unwrap();
    assert_eq!(view.entry_ids.len(), view.entries.len());
    assert_eq!(view.tool_history.len(), view.entries.len());
    for id in [&current, &queued] {
        let position = view
            .entry_ids
            .iter()
            .position(|row| row.as_ref() == Some(id))
            .unwrap();
        assert!(view.tool_history[position].is_empty());
    }
    // Later completion must not change classification in the pinned view.
    let attempt = session.start_turn_attempt(queued).await.unwrap();
    session.complete_turn_attempt(&attempt, None).await.unwrap();
    let pinned = session
        .turn_context_view_at(snapshot, &current, |name| name == "alpha")
        .await
        .unwrap();
    assert_eq!(view.entry_ids, pinned.entry_ids);
    assert_eq!(
        view.entries
            .iter()
            .map(|row| &row.content)
            .collect::<Vec<_>>(),
        pinned
            .entries
            .iter()
            .map(|row| &row.content)
            .collect::<Vec<_>>()
    );
    let contents = |entries: Vec<SessionEntry>| {
        entries
            .into_iter()
            .map(|entry| entry.content)
            .collect::<Vec<_>>()
    };
    let (turn, history) = session
        .turn_context_with_tool_history(&current, |name| name == "alpha")
        .await
        .unwrap();
    assert_eq!(turn.len(), history.len());
    let turn = contents(turn);
    let boundary = turn.iter().position(|c| c == "SUMMARY").unwrap();
    assert_eq!(&turn[boundary..], ["SUMMARY", "CURRENT", "LATER"]);
    let pinned = contents(pinned.entries);
    let boundary = pinned.iter().position(|c| c == "SUMMARY").unwrap();
    assert_eq!(
        &pinned[boundary..],
        ["SUMMARY", "CURRENT", "QUEUED", "LATER"]
    );

    // Without a turn, the positional boundary is unchanged.
    let (plain, _) = session.context_with_tool_history().await.unwrap();
    let plain = contents(plain);
    let boundary = plain.iter().position(|c| c == "SUMMARY").unwrap();
    assert_eq!(&plain[boundary..], ["SUMMARY", "LATER"]);
}

#[tokio::test]
async fn native_compact_keeps_current_and_queued_requests_it_covered() {
    let (_instance, _server, registry) = server_fixture().await;
    let (_conv, db) = registry.create_session(Some("native")).await.unwrap();
    let sid = db.root_id().to_string();
    let completed = write_user_message_with_content(&db, &sid, "COMPLETED").await;
    let current = write_user_message_with_content(&db, &sid, "CURRENT").await;
    write_user_message_with_content(&db, &sid, "QUEUED").await;
    let mut session = Session::new(ConversationId(sid.clone()), db.clone()).await;
    let attempt = session.start_turn_attempt(completed).await.unwrap();
    session.complete_turn_attempt(&attempt, None).await.unwrap();
    session.start_turn_attempt(current.clone()).await.unwrap();

    let command_id = TurnRequestId::parse("native-compact");
    session
        .submit_command(crate::session::SessionCommandRequest {
            command_id: command_id.clone(),
            sender: "user".into(),
            created_at: Utc::now(),
            command: crate::session::SessionCommand::Compact {
                source_snapshot: db.snapshot().await.unwrap(),
            },
        })
        .await
        .unwrap();
    let command = session.start_turn_attempt(command_id).await.unwrap();
    session
        .complete_command_attempt(
            &command,
            crate::session::SessionCommandOutcome::Compact {
                summary: "NATIVE-SUMMARY".into(),
            },
        )
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid, "LATER").await;

    let session = Session::new(ConversationId(sid), db).await;
    let contents = |entries: Vec<SessionEntry>| {
        entries
            .into_iter()
            .map(|entry| entry.content)
            .collect::<Vec<_>>()
    };
    let (turn, _) = session
        .turn_context_with_tool_history(&current, |name| name == "alpha")
        .await
        .unwrap();
    assert_eq!(
        contents(turn),
        ["NATIVE-SUMMARY", "CURRENT", "QUEUED", "LATER"]
    );
    assert_eq!(
        contents(session.context_entries().await.unwrap()),
        ["NATIVE-SUMMARY", "LATER"]
    );
}
