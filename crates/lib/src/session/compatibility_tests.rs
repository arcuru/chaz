//! stop-writer.snap holds the persisted shapes this crate writes when a user
//! stops a turn. The reader-only release before typed Cancel/Cancelled was
//! verified against these exact rows, so the writer must keep producing them.

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

pub(super) async fn context(session: &Session) -> Vec<RuntimeMessage> {
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

#[test]
fn stop_writer_rows_keep_the_verified_wire_format() {
    for row in writer_rows() {
        let raw = &row["value"];
        let reserialized = match row["store"].as_str().unwrap() {
            "entries" => {
                let entry: SessionEntry = serde_json::from_value(raw.clone()).unwrap();
                assert!(!matches!(entry.entry_type, EntryType::Unknown { .. }));
                serde_json::to_value(entry).unwrap()
            }
            TURN_TRANSCRIPT_STORE => {
                let record: TurnTranscriptRecord = serde_json::from_value(raw.clone()).unwrap();
                assert!(!matches!(
                    record.message,
                    TurnTranscriptMessage::Unknown { .. }
                        | TurnTranscriptMessage::ToolResult {
                            outcome: ToolResultOutcome::Unknown { .. },
                            ..
                        }
                ));
                serde_json::to_value(record).unwrap()
            }
            _ => continue,
        };
        assert_eq!(&reserialized, raw, "{}", row["id"]);
    }
}

#[tokio::test]
async fn stop_writer_attempts_replay_only_when_fully_supported() {
    let (_instance, _user, db) = test_helpers::test_session_db().await;
    install_writer_rows(&db).await;
    let session = Session::new(ConversationId("reader".into()), db.clone()).await;
    assert_eq!(session.entries().len(), 3);
    let stop = session
        .entries()
        .iter()
        .find(|e| e.entry_type == EntryType::Cancel)
        .unwrap();
    assert!(stop.bridge_body().is_none());
    assert!(stop.bridge_role().is_none());

    // Check both sides of replay: never leave an orphan call or result.
    let replay_ids = |messages: &[RuntimeMessage]| {
        let calls: Vec<_> = messages
            .iter()
            .filter_map(|m| match m {
                RuntimeMessage::AssistantToolCalls { tool_calls, .. } => Some(tool_calls),
                _ => None,
            })
            .flatten()
            .map(|c| c.id.clone())
            .collect();
        (calls, native_ids(messages))
    };
    let both = vec!["call-cancelled".to_string(), "call-supported".to_string()];
    assert_eq!(replay_ids(&context(&session).await), (both.clone(), both));

    // A later writer's nested outcome withholds only its own exchange.
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<Table<TurnTranscriptRecord>>(TURN_TRANSCRIPT_STORE)
        .await
        .unwrap();
    let mut result = store.get("cancelled-1").await.unwrap();
    let TurnTranscriptMessage::ToolResult { outcome, .. } = &mut result.message else {
        panic!("tool result")
    };
    *outcome = serde_json::from_value(json!("Future")).unwrap();
    store.set("cancelled-1", result).await.unwrap();
    txn.commit().await.unwrap();
    let supported = vec!["call-supported".to_string()];
    assert_eq!(
        replay_ids(&context(&session).await),
        (supported.clone(), supported)
    );
}

// Candidate private-reference wire shapes for a reader-first rollout. There is
// deliberately no writer or execution support yet: older readers must preserve
// these rows without treating a reference as a broadcast instruction or wake.
fn scheduled_reader_rows() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/scheduled-input-reader.snap")).unwrap()
}

#[test]
fn scheduled_input_candidate_rows_roundtrip_as_inert_unknowns() {
    for row in scheduled_reader_rows() {
        let raw = &row["value"];
        let roundtrip = match row["store"].as_str().unwrap() {
            "entries" => {
                let entry: SessionEntry = serde_json::from_value(raw.clone()).unwrap();
                assert!(matches!(entry.entry_type, EntryType::Unknown { .. }));
                assert!(entry.bridge_role().is_none());
                assert!(entry.bridge_body().is_none());
                serde_json::to_value(entry).unwrap()
            }
            TURN_TRANSCRIPT_STORE => {
                let record: TurnTranscriptRecord = serde_json::from_value(raw.clone()).unwrap();
                assert!(matches!(
                    record.message,
                    TurnTranscriptMessage::Unknown { .. }
                ));
                serde_json::to_value(record).unwrap()
            }
            _ => panic!("unexpected fixture store"),
        };
        assert_eq!(&roundtrip, raw, "{}", row["id"]);
    }
}

#[tokio::test]
async fn scheduled_input_unknown_rows_survive_reload_without_context_or_wake() {
    let (_instance, _user, db) = test_helpers::test_session_db().await;
    let txn = db.new_transaction().await.unwrap();
    for row in scheduled_reader_rows() {
        txn.get_store::<Table<Value>>(row["store"].as_str().unwrap())
            .await
            .unwrap()
            .set(row["id"].as_str().unwrap(), row["value"].clone())
            .await
            .unwrap();
    }
    txn.commit().await.unwrap();

    let mut session = Session::new(ConversationId("reader".into()), db.clone()).await;
    assert_eq!(session.entries().len(), 1, "opaque entry remains readable");
    assert!(
        session
            .turn_requests(|_| false, &HashSet::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        context(&session)
            .await
            .iter()
            .all(|message| { !matches!(message, RuntimeMessage::User(_)) })
    );
    assert_eq!(session.turn_transcript("attempt").await.unwrap().len(), 1);

    // Non-vacuous control: an ordinary human request is still selected and
    // enters context (with its date marker) beside an unsupported reference.
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
    let reloaded = Session::new(ConversationId("reader".into()), db.clone()).await;
    let requests = reloaded
        .turn_requests(|_| false, &HashSet::new())
        .await
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].id, ordinary);
    let messages = context(&reloaded).await;
    let users: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            RuntimeMessage::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users.len(), 2);
    assert!(users[0].starts_with("current_date: "));
    assert_eq!(users[1], "ordinary question");

    // Reading and constructing context must not rewrite opaque stored bytes.
    let txn = db.new_transaction().await.unwrap();
    for row in scheduled_reader_rows() {
        let stored = txn
            .get_store::<Table<Value>>(row["store"].as_str().unwrap())
            .await
            .unwrap()
            .get(row["id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(stored, row["value"]);
    }
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
}

#[test]
fn scheduled_reference_boundary_is_reader_first_and_prompt_free() {
    // The previous reader's extensible wire boundary preserves the new tag;
    // it cannot reconstruct it as ordinary steering or executable history.
    crate::session::wire::session_wire_enum! {
        #[derive(Debug)]
        enum PreviousTranscript {
            Cancelled,
            Steering { model_sequence: u64, inputs: Vec<steering::JobInputRequest> },
        }
    }
    let raw = json!({"ScheduledInput":{"model_sequence":2,"references":[{
        "version":1,"owner_agent_db_id":"owner","schedule_id":"schedule",
        "lifetime":"lifetime","revision":"immutable","occurrence_id":"slot","delivery_id":"delivery"
    }]}});
    let previous: PreviousTranscript = serde_json::from_value(raw.clone()).unwrap();
    assert!(
        matches!(&previous, PreviousTranscript::Unknown { kind, .. } if kind == "ScheduledInput")
    );
    assert_eq!(serde_json::to_value(previous).unwrap(), raw);
    let current: TurnTranscriptMessage = serde_json::from_value(raw.clone()).unwrap();
    assert!(matches!(
        current,
        TurnTranscriptMessage::ScheduledInput { .. }
    ));
    assert_eq!(serde_json::to_value(current).unwrap(), raw);
    assert!(
        serde_json::from_value::<TurnTranscriptMessage>(
            json!({"ScheduledInput":{"model_sequence":2,"references":"private text"}})
        )
        .is_err()
    );
}
