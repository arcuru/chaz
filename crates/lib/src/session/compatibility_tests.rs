//! Reader-only regressions: no typed Cancel/Cancelled variants exist here.
//! stop-writer.snap is exported from typed tables by writer revision
//! 5b199639aa923686d452f343fafeb43b3c877c4b, not hand-authored stop JSON.

use super::*;
use crate::config::ContextConfig;
use crate::context::ContextBuilder;
use crate::runtime::{RuntimeMessage, ToolResultOutcome};
use serde_json::{Value, json};

pub(crate) fn writer_rows() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/stop-writer.snap")).unwrap()
}

pub(crate) async fn install_writer_rows(db: &Database) {
    let txn = db.new_transaction().await.unwrap();
    for row in writer_rows() {
        txn.get_store::<Table<Value>>(row["store"].as_str().unwrap())
            .await
            .unwrap()
            .set(row["id"].as_str().unwrap(), row["value"].clone())
            .await
            .unwrap();
    }
    txn.commit().await.unwrap();
}

async fn context(session: &Session) -> Vec<RuntimeMessage> {
    let (entries, history) = session.context_with_tool_history().await.unwrap();
    ContextBuilder::new(&entries, "agent", "", &ContextConfig::default())
        .with_tool_history(&history)
        .build()
        .await
        .messages
}

fn native_ids(messages: &[RuntimeMessage]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            RuntimeMessage::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn precursor_reader_preserves_stop_writer_tables_and_ordinary_context() {
    let (_instance, _user, db) = test_helpers::test_session_db().await;
    Session::initialize_turn_schema(&db).await.unwrap();
    install_writer_rows(&db).await;
    for _ in 0..2 {
        let session = Session::new(ConversationId("reader".into()), db.clone()).await;
        assert_eq!(
            session.entries().len(),
            3,
            "a nonempty typed scan is required"
        );
        let stop = session
            .entries_with_ids()
            .find(|(id, _)| id.as_str() == "stop-event")
            .unwrap()
            .1;
        assert!(matches!(&stop.entry_type, EntryType::Unknown { kind, .. } if kind == "Cancel"));
        assert!(stop.bridge_role().is_none());
        assert!(stop.bridge_body().is_none());
        let requests = session
            .turn_requests(|sender| sender == "agent", &HashSet::new())
            .await
            .unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["request-cancelled", "request-supported"]
        );
        assert!(
            requests
                .iter()
                .all(|r| matches!(r.state, TurnRequestState::Completed { .. }))
        );
        assert!(
            session
                .next_turn_request(|_| false, &HashSet::new())
                .await
                .unwrap()
                .is_none()
        );
        let cancelled = session.turn_transcript("attempt-cancelled").await.unwrap();
        assert_eq!(cancelled.len(), 3);
        assert!(
            matches!(&cancelled[2].message, TurnTranscriptMessage::Unknown { kind, .. } if kind == "Cancelled")
        );
        assert!(
            matches!(&cancelled[1].message, TurnTranscriptMessage::ToolResult {
            outcome: ToolResultOutcome::Unknown { kind, .. }, ..
        } if kind == "Cancelled")
        );
        let messages = context(&session).await;
        assert_eq!(native_ids(&messages), ["call-supported"]);
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(m, RuntimeMessage::AssistantToolCalls { .. }))
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(m, RuntimeMessage::User(_)))
                .count(),
            2
        );
        assert!(!messages.iter().any(
            |m| matches!(m, RuntimeMessage::User(text) if text.contains("attempt-cancelled"))
        ));
        // Export via each production type, write back, then re-open. Unknown
        // data must survive semantic round trips, not merely raw Table<Value>.
        let txn = db.new_transaction().await.unwrap();
        for row in writer_rows() {
            let id = row["id"].as_str().unwrap();
            let value = row["value"].clone();
            let exported = match row["store"].as_str().unwrap() {
                "entries" => serde_json::to_value(
                    txn.get_store::<Table<SessionEntry>>("entries")
                        .await
                        .unwrap()
                        .get(id)
                        .await
                        .unwrap(),
                )
                .unwrap(),
                "turn_attempts" => serde_json::to_value(
                    txn.get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
                        .await
                        .unwrap()
                        .get(id)
                        .await
                        .unwrap(),
                )
                .unwrap(),
                "turn_transcript" => serde_json::to_value(
                    txn.get_store::<Table<TurnTranscriptRecord>>(TURN_TRANSCRIPT_STORE)
                        .await
                        .unwrap()
                        .get(id)
                        .await
                        .unwrap(),
                )
                .unwrap(),
                _ => unreachable!(),
            };
            assert_eq!(exported, value);
            txn.get_store::<Table<Value>>(row["store"].as_str().unwrap())
                .await
                .unwrap()
                .set(id, exported)
                .await
                .unwrap();
        }
        txn.commit().await.unwrap();
    }
}

#[tokio::test]
async fn unsupported_native_outcome_excludes_whole_attempt_not_other_attempts() {
    let (_instance, _user, db) = test_helpers::test_session_db().await;
    install_writer_rows(&db).await;
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<Table<Value>>(TURN_TRANSCRIPT_STORE)
        .await
        .unwrap();
    // Isolate the nested outcome: an otherwise interpretable, complete turn.
    let mut terminal = store.get("supported-2").await.unwrap();
    terminal["request_id"] = json!("request-cancelled");
    terminal["attempt_id"] = json!("attempt-cancelled");
    store.set("cancelled-2", terminal).await.unwrap();
    // Unsupported records on unselected and interrupted attempts are still
    // decoded during a typed Table scan, before the attempt filter is applied.
    for (id, complete) in [("superseded", true), ("interrupted", false)] {
        let mut attempt: TurnAttempt = serde_json::from_value(
            writer_rows()
                .into_iter()
                .find(|r| r["id"] == "attempt-supported")
                .unwrap()["value"]
                .clone(),
        )
        .unwrap();
        attempt.attempt_id = id.into();
        if complete {
            attempt.generation = 0;
        } else {
            attempt.request_id = TurnRequestId::parse("unselected-request");
            attempt.status = TurnAttemptStatus::Started;
            attempt.completed_at = None;
        }
        txn.get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
            .await
            .unwrap()
            .set(id, attempt.clone())
            .await
            .unwrap();
        let record = TurnTranscriptRecord {
            request_id: attempt.request_id,
            attempt_id: id.into(),
            sequence: 0,
            timestamp: Utc::now(),
            message: serde_json::from_value(
                json!({"FutureNative":{"payload":[1,{"opaque":true}]}}),
            )
            .unwrap(),
        };
        txn.get_store::<Table<TurnTranscriptRecord>>(TURN_TRANSCRIPT_STORE)
            .await
            .unwrap()
            .set(id, record)
            .await
            .unwrap();
    }
    // A newer generation makes selection independent of random attempt IDs.
    let attempts = txn
        .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
        .await
        .unwrap();
    let mut selected = attempts.get("attempt-supported").await.unwrap();
    selected.generation = 1;
    attempts.set("attempt-supported", selected).await.unwrap();
    txn.commit().await.unwrap();
    let session = Session::new(ConversationId("reader".into()), db.clone()).await;
    assert_eq!(native_ids(&context(&session).await), ["call-supported"]);
    // Positive control: replacing *only* the unknown nested outcome allows
    // the complete call/result pair to replay, never a lone call or result.
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<Table<TurnTranscriptRecord>>(TURN_TRANSCRIPT_STORE)
        .await
        .unwrap();
    let mut result = store.get("cancelled-1").await.unwrap();
    let TurnTranscriptMessage::ToolResult { outcome, .. } = &mut result.message else {
        panic!("tool result")
    };
    *outcome = ToolResultOutcome::Denied;
    store.set("cancelled-1", result).await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        native_ids(&context(&session).await),
        ["call-cancelled", "call-supported"]
    );
    // Compaction still excludes covered native groups, including unsupported
    // ones, without discarding later conversation.
    let command = SessionCommandRequest {
        command_id: TurnRequestId::parse("compact"),
        sender: "user".into(),
        created_at: Utc::now(),
        command: SessionCommand::Compact {
            source_snapshot: db.snapshot().await.unwrap(),
        },
    };
    session.submit_command(command.clone()).await.unwrap();
    let attempt = session
        .start_turn_attempt(command.command_id)
        .await
        .unwrap();
    session
        .complete_command_attempt(
            &attempt,
            SessionCommandOutcome::Compact {
                summary: "known compact summary".into(),
            },
        )
        .await
        .unwrap();
    let mut tail = Session::new(ConversationId("reader".into()), db.clone()).await;
    tail.add_entry(SessionEntry {
        sender: "user".into(),
        content: "ordinary tail".into(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    })
    .await
    .unwrap();
    let messages = context(&tail).await;
    assert!(native_ids(&messages).is_empty());
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, RuntimeMessage::User(text) if text == "ordinary tail"))
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, RuntimeMessage::User(text) if text == "known compact summary"))
    );
}

#[tokio::test]
async fn unknown_commands_and_results_roundtrip_without_queued_work_or_compaction() {
    let (_instance, _user, db) = test_helpers::test_session_db().await;
    Session::initialize_turn_schema(&db).await.unwrap();
    let session = Session::new(ConversationId("reader".into()), db.clone()).await;
    let mut requests = Vec::new();
    for (id, wire) in [
        (
            "unknown",
            json!({"FutureCommand":{"target":"request","nested":[1,null,{"secret":"opaque"}]}}),
        ),
        ("unknown-unit", json!("FutureCommand")),
        (
            "known-with-unknown-result",
            serde_json::to_value(SessionCommand::Compact {
                source_snapshot: db.snapshot().await.unwrap(),
            })
            .unwrap(),
        ),
    ] {
        let request = SessionCommandRequest {
            command_id: TurnRequestId::parse(id),
            sender: "user".into(),
            created_at: Utc::now(),
            command: serde_json::from_value(wire.clone()).unwrap(),
        };
        session.submit_command(request.clone()).await.unwrap();
        session.submit_command(request.clone()).await.unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap()["command"], wire);
        requests.push(request);
    }
    let raw = json!({"FutureOutcome":{"summary":"not a compact success","anything":[false,42]}});
    let result = SessionCommandResult {
        command_id: TurnRequestId::parse("known-with-unknown-result"),
        attempt_id: "not-executed".into(),
        completed_at: Utc::now(),
        outcome: serde_json::from_value(raw.clone()).unwrap(),
    };
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<SessionCommandResult>>(SESSION_COMMAND_RESULTS_STORE)
        .await
        .unwrap()
        .set(result.command_id.as_str(), result.clone())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let reloaded = Session::new(ConversationId("reader".into()), db.clone()).await;
    assert!(
        reloaded
            .command_requests(&HashSet::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        reloaded
            .next_command_request(&HashSet::new())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !reloaded
            .has_queued_work_at(db.snapshot().await.unwrap(), |_| false, &HashSet::new())
            .await
            .unwrap()
    );
    assert!(reloaded.context_entries().await.unwrap().is_empty());
    let observed = reloaded
        .command_result(&result.command_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&observed.outcome, SessionCommandOutcome::Unknown { kind, .. } if kind == "FutureOutcome")
    );
    assert_eq!(serde_json::to_value(&observed).unwrap()["outcome"], raw);
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<Table<SessionCommandRequest>>(SESSION_COMMANDS_STORE)
        .await
        .unwrap();
    for request in requests {
        let saved = store.get(request.command_id.as_str()).await.unwrap();
        assert_eq!(saved, request);
        store
            .set(saved.command_id.as_str(), saved.clone())
            .await
            .unwrap();
    }
    txn.commit().await.unwrap();
}

#[test]
fn unknown_payloads_preserved_known_payloads_still_strict() {
    fn roundtrip<T: serde::de::DeserializeOwned + Serialize>(raw: Value) {
        let value: T = serde_json::from_value(raw.clone()).unwrap();
        let serialized = serde_json::to_value(value).unwrap();
        assert_eq!(serialized, raw);
        let reloaded: T = serde_json::from_value(serialized).unwrap();
        assert_eq!(serde_json::to_value(reloaded).unwrap(), raw);
    }
    for raw in [
        json!("Future"),
        json!({"Future":{"nested":[null,12,{"type":"Compact"}]}}),
    ] {
        roundtrip::<SessionCommand>(raw.clone());
        roundtrip::<SessionCommandOutcome>(raw.clone());
        roundtrip::<TurnTranscriptMessage>(raw.clone());
        roundtrip::<ToolResultOutcome>(raw);
    }
    for raw in [
        json!({"Compact":{}}),
        json!({"Retry":{"target_request_id":"r","expected_interrupted_attempt_id":"a","extra":true}}),
        json!("Retry"),
    ] {
        assert!(serde_json::from_value::<SessionCommand>(raw).is_err());
    }
    assert!(
        serde_json::from_value::<SessionCommandOutcome>(json!({"Compact":{"summary":12}})).is_err()
    );
    assert!(
        serde_json::from_value::<TurnTranscriptMessage>(json!({"ToolResult":{"outcome":"Future"}}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<TurnTranscriptMessage>(
            json!({"ModelResponse":{"terminal":"yes"}})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolResultOutcome>(json!({"Success":{"payload":42}})).is_err()
    );
    for raw in [
        json!(null),
        json!(12),
        json!({}),
        json!({"Future":{},"Retry":{}}),
    ] {
        assert!(serde_json::from_value::<SessionCommand>(raw.clone()).is_err());
        assert!(serde_json::from_value::<TurnTranscriptMessage>(raw.clone()).is_err());
        assert!(serde_json::from_value::<ToolResultOutcome>(raw).is_err());
    }
    for outcome in [
        ToolResultOutcome::Success,
        ToolResultOutcome::Error,
        ToolResultOutcome::Denied,
        ToolResultOutcome::ApprovalTimedOut,
        ToolResultOutcome::RateLimited,
        ToolResultOutcome::Blocked,
        ToolResultOutcome::TimedOut,
        ToolResultOutcome::Unavailable,
    ] {
        let wire = serde_json::to_value(&outcome).unwrap();
        assert!(wire.is_string());
        assert_eq!(
            serde_json::from_value::<ToolResultOutcome>(wire).unwrap(),
            outcome
        );
    }
}

#[allow(dead_code)]
mod closed {
    include!("fixtures/closed_decoders.rs");
}

#[tokio::test]
async fn pinned_closed_readers_fail_on_stop_while_positive_native_scan_is_nonempty() {
    let (_instance, _user, db) = test_helpers::test_session_db().await;
    install_writer_rows(&db).await;
    let txn = db.new_transaction().await.unwrap();
    let transcript = txn
        .get_store::<Table<Value>>(TURN_TRANSCRIPT_STORE)
        .await
        .unwrap();
    for sequence in 0..3 {
        let old = transcript
            .get(format!("supported-{sequence}"))
            .await
            .unwrap();
        assert!(
            serde_json::from_value::<closed::TurnTranscriptMessage>(old["message"].clone()).is_ok()
        );
    }
    let stopped = transcript.get("cancelled-2").await.unwrap();
    let failed =
        serde_json::from_value::<closed::TurnTranscriptMessage>(stopped["message"].clone())
            .unwrap_err();
    assert!(failed.to_string().contains("unknown variant `Cancelled`"));
    let stopped_tool = transcript.get("cancelled-1").await.unwrap();
    let failed =
        serde_json::from_value::<closed::TurnTranscriptMessage>(stopped_tool["message"].clone())
            .unwrap_err();
    assert!(failed.to_string().contains("unknown variant `Cancelled`"));
    assert!(serde_json::from_value::<closed::ToolResultOutcome>(json!("Success")).is_ok());
    assert!(serde_json::from_value::<closed::ToolResultOutcome>(json!("Cancelled")).is_err());
    assert!(serde_json::from_value::<closed::EntryType>(json!("Message")).is_ok());
    assert!(serde_json::from_value::<closed::EntryType>(json!("Cancel")).is_err());
    let known_command = json!({"Retry":{"target_request_id":"request","expected_interrupted_attempt_id":"attempt"}});
    assert!(serde_json::from_value::<closed::SessionCommand>(known_command).is_ok());
    assert!(
        serde_json::from_value::<closed::SessionCommand>(json!({"FutureCommand":{"data":1}}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<closed::SessionCommandOutcome>(
            json!({"Rejected":{"message":"known"}})
        )
        .is_ok()
    );
    assert!(
        serde_json::from_value::<closed::SessionCommandOutcome>(
            json!({"FutureOutcome":{"data":1}})
        )
        .is_err()
    );
    // Table::search decodes every row before applying the filter. The closed
    // decoder cannot even scan the supported attempt beside the cancelled one.
    #[derive(Clone, Serialize, Deserialize)]
    struct ClosedRecord {
        attempt_id: String,
        #[serde(rename = "message")]
        _message: closed::TurnTranscriptMessage,
    }
    assert!(
        txn.get_store::<Table<ClosedRecord>>(TURN_TRANSCRIPT_STORE)
            .await
            .unwrap()
            .search(|r| r.attempt_id == "attempt-supported")
            .await
            .is_err()
    );
    let session = Session::new(ConversationId("reader".into()), db).await;
    assert_eq!(
        session
            .turn_transcript("attempt-supported")
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(native_ids(&context(&session).await), ["call-supported"]);
}

#[tokio::test]
async fn malformed_known_table_rows_are_errors_not_unknown_records() {
    let (_instance, _user, db) = test_helpers::test_session_db().await;
    install_writer_rows(&db).await;
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<Table<Value>>(TURN_TRANSCRIPT_STORE)
        .await
        .unwrap();
    let mut malformed = store.get("supported-1").await.unwrap();
    malformed["message"]["ToolResult"]["call_index"] = json!("not an index");
    store.set("bad-known", malformed).await.unwrap();
    txn.commit().await.unwrap();
    let session = Session::new(ConversationId("reader".into()), db.clone()).await;
    assert!(session.turn_transcript("attempt-supported").await.is_err());
    assert!(session.context_with_tool_history().await.is_err());
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<Value>>(SESSION_COMMANDS_STORE).await.unwrap().set("bad-known", json!({
        "command_id":"bad-known", "sender":"user", "created_at":Utc::now(), "command":{"Retry":{"target_request_id":"request"}}
    })).await.unwrap();
    txn.commit().await.unwrap();
    assert!(session.command_requests(&HashSet::new()).await.is_err());
}
