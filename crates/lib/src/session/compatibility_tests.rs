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
