use super::{
    provenance::{Observation, ProvenanceStore, Record},
    record::*,
    *,
};
use crate::{
    config::ContextConfig,
    context::{AssembledContext, ContextBuilder},
    runtime::{RuntimeMessage, ToolCallRequest, ToolResultOutcome},
    session::{EntryType, Session, SessionEntry},
};
use eidetica::store::DocStore;
use serde::Serialize;

async fn stage(store: &DocStore, value: &impl Serialize) -> String {
    let text = atom::encode(value).unwrap();
    let key = atom::key(&text);
    store.set(&key, text).await.unwrap();
    key
}

async fn activate(db: &Database, boundary: &Snapshot) -> Snapshot {
    let descriptor = contract::Descriptor {
        version: 1,
        encoding: 1,
        context_semantics: 1,
        renderer: 1,
        required_operations: ["append-v1".into(), "replace-v1".into()].into(),
        legacy: contract::LegacyBoundary {
            database: db.root_id().to_string(),
            frontier: boundary.clone(),
            adapter_version: 1,
        },
    };
    let txn = db.new_transaction_at(boundary).await.unwrap();
    let store = txn
        .get_store::<contract::ContractStore>(contract::STORE)
        .await
        .unwrap();
    stage(store.doc(), &descriptor).await;
    Snapshot::from([txn.commit().await.unwrap()])
}

fn operation(
    db: &Database,
    at: &Snapshot,
    seq: u64,
    payload: Payload,
    view: View,
    targets: Vec<RepresentationId>,
) -> Operation {
    let mut op = Operation {
        version: 1,
        op_id: uuid::Uuid::new_v4().to_string(),
        branch_id: "branch".into(),
        observed: ObservedBinding {
            database: db.root_id().to_string(),
            frontier: at.clone(),
        },
        causal_seq: seq,
        kind: if targets.is_empty() {
            OperationKind::Append
        } else {
            OperationKind::Replace
        },
        targets,
        facts: vec![Fact {
            payload_version: 1,
            payload,
        }],
        outputs: vec![],
    };
    op.outputs.push(Representation {
        fact: op.fact_id(0),
        view,
        decoder_version: 1,
    });
    op
}

fn input(text: &str) -> Payload {
    Payload::Input {
        sender: "user".into(),
        text: text.into(),
    }
}
fn summary(text: &str) -> Payload {
    Payload::TransformationOutput { text: text.into() }
}

async fn commit_op(db: &Database, op: &Operation) -> Snapshot {
    let txn = db.new_transaction_at(&op.observed.frontier).await.unwrap();
    stage(
        txn.get_store::<ConversationStore>(STORE)
            .await
            .unwrap()
            .doc(),
        op,
    )
    .await;
    Snapshot::from([txn.commit().await.unwrap()])
}

async fn context(db: &Database, at: &Snapshot, budget: usize) -> AssembledContext {
    let read = Session::history_at(db, at).await.unwrap();
    let view = read.context().await.unwrap();
    let config = ContextConfig {
        max_context_tokens: budget,
        reserved_output_tokens: 0,
    };
    ContextBuilder::from_history(&view, "agent", "", &config)
        .build()
        .await
}

async fn new_session(db: &Database) -> Session {
    Session::try_new(crate::types::ConversationId("fixture".into()), db.clone())
        .await
        .unwrap()
}

fn entry(text: &str) -> SessionEntry {
    SessionEntry {
        sender: "user".into(),
        content: text.into(),
        timestamp: "2026-10-06T00:00:00Z".parse().unwrap(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    }
}

#[tokio::test]
async fn v1_reader_keeps_old_call_and_legacy_prefix_through_signed_merge() {
    let (instance, _account, db) = super::super::test_helpers::test_session_db().await;
    let mut session = new_session(&db).await;
    let first = session.add_entry(entry("pinned legacy")).await.unwrap();
    let second = session
        .add_entry(SessionEntry {
            timestamp: "2026-10-06T00:01:00Z".parse().unwrap(),
            ..entry("replace this legacy row")
        })
        .await
        .unwrap();
    let boundary = db.snapshot().await.unwrap();
    let legacy = context(&db, &boundary, 4096).await;
    let active = activate(&db, &boundary).await;
    let read = Session::history_at(&db, &active).await.unwrap();
    let pinned = read.context().await.unwrap();
    let anchors = |id| {
        pinned
            .groups
            .iter()
            .find(|g| g.entry_id() == Some(id))
            .unwrap()
            .representation
            .clone()
            .unwrap()
    };
    let (first, second) = (anchors(&first), anchors(&second));
    let original = context(&db, &active, 4096).await;
    assert_eq!(original.messages, legacy.messages);
    assert_eq!(pinned.entries().count(), 2);

    let op = operation(
        &db,
        &active,
        1,
        input("original v1 input"),
        View::Message(MessageRole::User),
        vec![],
    );
    let before_call = commit_op(&db, &op).await;
    let prepared_before = context(&db, &before_call, 4096).await;
    let call = CallId {
        request: "req".into(),
        branch: "branch".into(),
        model_sequence: 0,
    };
    let input = CallInput {
        call: call.clone(),
        attempt: "attempt".into(),
        observed: ObservedBinding {
            database: db.root_id().to_string(),
            frontier: before_call.clone(),
        },
        representations: prepared_before
            .sources
            .iter()
            .filter_map(|source| match source {
                crate::extension::projection::ContextSource::ConversationRepresentation { id } => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect(),
        renderer_version: 1,
        context_version: 1,
        requested_model: "fixture".into(),
        api: "fixture".into(),
        effective_nonsecret_settings: BTreeMap::new(),
        sources: vec![],
        accepted_additions: vec![],
        messages: vec![],
        tools: vec![],
        fidelity: Fidelity::Approximate,
        reasons: vec!["fixture has no backend serializer".into()],
    };
    assert!(input.representations.contains(&first));
    assert!(input.representations.contains(&second));
    assert!(input.representations.contains(&op.representation_id(0)));
    let txn = db.new_transaction_at(&before_call).await.unwrap();
    stage(
        txn.get_store::<ProvenanceStore>(provenance::STORE)
            .await
            .unwrap()
            .doc(),
        &Record {
            version: 1,
            observation: Observation::Prepared {
                input: Box::new(input.clone()),
            },
        },
    )
    .await;
    let prepared = Snapshot::from([txn.commit().await.unwrap()]);
    let replacement = operation(
        &db,
        &prepared,
        2,
        summary("replacement"),
        View::Summary,
        vec![second.clone(), op.representation_id(0)],
    );
    let replaced = commit_op(&db, &replacement).await;
    let unrelated = operation(
        &db,
        &prepared,
        2,
        self::input("concurrent append"),
        View::Message(MessageRole::User),
        vec![],
    );
    let concurrent = commit_op(&db, &unrelated).await;
    let merged = Snapshot::new([replaced.tips(), concurrent.tips()].concat());
    let txn = db.new_transaction_at(&merged).await.unwrap();
    txn.get_store::<DocStore>("fixture_merge")
        .await
        .unwrap()
        .set("merge", true)
        .await
        .unwrap();
    let head = Snapshot::from([txn.commit().await.unwrap()]);
    assert_eq!(db.snapshot().await.unwrap(), head);

    drop(read);
    drop(session);
    let reopened = eidetica::Database::open(&instance, db.root_id())
        .await
        .unwrap();
    let accepted = Session::history(&reopened).await.unwrap();
    assert_eq!(
        accepted.recorded_call(&call, "attempt").await.unwrap(),
        Some(&input)
    );
    assert_eq!(
        context(&reopened, &input.observed.frontier, 4096)
            .await
            .messages,
        prepared_before.messages
    );
    assert_eq!(
        accepted
            .display_rows()
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.content)
            .collect::<Vec<_>>(),
        ["pinned legacy", "replacement", "concurrent append"]
    );
    let now = context(&reopened, &head, 4096).await;
    let texts: Vec<_> = now
        .messages
        .iter()
        .filter_map(|m| match m {
            RuntimeMessage::User(text) if !text.starts_with("current_date:") => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        [
            "pinned legacy".to_owned(),
            view::historical_transformation("replacement"),
            "concurrent append".into()
        ]
    );
    assert_eq!(
        accepted
            .inspect_conversation()
            .await
            .unwrap()
            .unwrap()
            .operations
            .atoms
            .len(),
        3
    );
    assert_eq!(
        context(&reopened, &boundary, 4096).await.messages,
        legacy.messages
    );
    let mut writer = Session::new(crate::types::ConversationId("fixture".into()), db.clone()).await;
    assert!(
        writer
            .add_entry(entry("must not append a legacy row"))
            .await
            .unwrap_err()
            .to_string()
            .contains("read-only")
    );
    assert_eq!(db.snapshot().await.unwrap(), head);
}

#[tokio::test]
async fn v1_reader_exposes_hidden_malformed_signed_delta() {
    let (_instance, _account, db) = super::super::test_helpers::test_session_db().await;
    let start = activate(&db, &db.snapshot().await.unwrap()).await;
    let op = operation(
        &db,
        &start,
        1,
        input("event"),
        View::Message(MessageRole::User),
        vec![],
    );
    let wanted = atom::encode(&op).unwrap();
    let key = atom::key(&wanted);
    let txn = db.new_transaction_at(&start).await.unwrap();
    txn.get_store::<ConversationStore>(STORE)
        .await
        .unwrap()
        .doc()
        .set(&key, "bad digest")
        .await
        .unwrap();
    let malformed = txn.commit().await.unwrap();
    let both = Snapshot::from([malformed.clone()]);
    // A later signed overwrite produces a valid final materialization. It
    // cannot erase the invalid occurrence. Reuse the valid original bytes.
    let txn = db.new_transaction_at(&both).await.unwrap();
    txn.get_store::<ConversationStore>(STORE)
        .await
        .unwrap()
        .doc()
        .set(&key, wanted.as_str())
        .await
        .unwrap();
    let head = Snapshot::from([txn.commit().await.unwrap()]);
    let txn = db.new_transaction_at(&head).await.unwrap();
    assert_eq!(
        atom::atoms(
            &txn.get_store::<ConversationStore>(STORE)
                .await
                .unwrap()
                .doc()
                .get_all()
                .await
                .unwrap()
        )
        .unwrap(),
        [(key, wanted)].into()
    );
    let read = Session::history(&db).await.unwrap();
    let inspected = read.inspect_conversation().await.unwrap().unwrap();
    assert_eq!(
        &inspected.operations.invalid_deltas[&malformed.to_string()],
        db.get_entry(&malformed).await.unwrap().data(STORE).unwrap()
    );
    assert!(
        read.context()
            .await
            .unwrap_err()
            .to_string()
            .contains("atom digest mismatch")
    );
    assert_eq!(db.snapshot().await.unwrap(), head);
}

#[tokio::test]
async fn v1_reader_refuses_orphan_result_and_conflicting_replacement_without_effects() {
    let (_instance, _account, db) = super::super::test_helpers::test_session_db().await;
    let active = activate(&db, &db.snapshot().await.unwrap()).await;
    let start = operation(
        &db,
        &active,
        1,
        input("original"),
        View::Message(MessageRole::User),
        vec![],
    );
    let initial = commit_op(&db, &start).await;
    let result = Payload::ToolOutcome {
        execution: ToolExecutionId {
            attempt: "attempt".into(),
            ordinal: 0,
            call: ToolCallId {
                response: start.fact_id(0),
                index: 0,
            },
        },
        executed_arguments: "{}".into(),
        content: "not paired".into(),
        outcome: ToolResultOutcome::Success,
    };
    let orphan = commit_op(
        &db,
        &operation(&db, &initial, 2, result, View::ToolResult, vec![]),
    )
    .await;
    let a = commit_op(
        &db,
        &operation(
            &db,
            &initial,
            2,
            summary("left"),
            View::Summary,
            vec![start.representation_id(0)],
        ),
    )
    .await;
    let b = commit_op(
        &db,
        &operation(
            &db,
            &initial,
            2,
            summary("right"),
            View::Summary,
            vec![start.representation_id(0)],
        ),
    )
    .await;
    let conflict = Snapshot::new([a.tips(), b.tips()].concat());
    let before = db.snapshot().await.unwrap();
    let orphan_read = Session::history_at(&db, &orphan).await.unwrap();
    assert!(
        orphan_read
            .context()
            .await
            .unwrap_err()
            .to_string()
            .contains("orphan")
    );
    assert_eq!(
        orphan_read
            .inspect_conversation()
            .await
            .unwrap()
            .unwrap()
            .operations
            .atoms
            .len(),
        2
    );
    let read = Session::history_at(&db, &conflict).await.unwrap();
    assert!(
        read.context()
            .await
            .unwrap_err()
            .to_string()
            .contains("overlapping conversation replacement")
    );
    assert_eq!(
        read.inspect_conversation()
            .await
            .unwrap()
            .unwrap()
            .operations
            .atoms
            .len(),
        3
    );
    assert_eq!(db.snapshot().await.unwrap(), before);
}

#[tokio::test]
async fn v1_reader_replays_complete_native_unit_and_no_undated_boundary() {
    let (_instance, _account, db) = super::super::test_helpers::test_session_db().await;
    let active = activate(&db, &db.snapshot().await.unwrap()).await;
    let request = operation(
        &db,
        &active,
        1,
        input("head"),
        View::Message(MessageRole::User),
        vec![],
    );
    let at = commit_op(&db, &request).await;
    let native = operation(
        &db,
        &at,
        2,
        Payload::NativeModelResponse {
            response: NativeResponse {
                call: CallId {
                    request: "request".into(),
                    branch: "branch".into(),
                    model_sequence: 0,
                },
                attempt: "attempt".into(),
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: "same-provider-id".into(),
                    name: "fenced_test_tool".into(),
                    arguments: "{}".into(),
                }],
                provider_extra: BTreeMap::new(),
                metadata: None,
            },
        },
        View::NativeExchange,
        vec![],
    );
    let pending = commit_op(&db, &native).await;
    assert!(
        Session::history_at(&db, &pending)
            .await
            .unwrap()
            .context()
            .await
            .is_err()
    );
    let complete = commit_op(
        &db,
        &operation(
            &db,
            &pending,
            3,
            Payload::ToolOutcome {
                execution: ToolExecutionId {
                    call: ToolCallId {
                        response: native.fact_id(0),
                        index: 0,
                    },
                    attempt: "attempt".into(),
                    ordinal: 0,
                },
                executed_arguments: "{}".into(),
                content: "result".into(),
                outcome: ToolResultOutcome::Success,
            },
            View::ToolResult,
            vec![],
        ),
    )
    .await;
    let full = context(&db, &complete, 4096).await;
    assert!(
        matches!(&full.messages[..], [RuntimeMessage::User(_), RuntimeMessage::AssistantToolCalls { .. }, RuntimeMessage::ToolResult { call_id, .. }] if call_id == "same-provider-id")
    );
    let tight = context(&db, &complete, 1).await;
    assert!(matches!(
        &tight.messages[..],
        [
            RuntimeMessage::AssistantToolCalls { .. },
            RuntimeMessage::ToolResult { .. }
        ]
    ));
    assert_eq!(db.snapshot().await.unwrap(), complete);
}

#[tokio::test]
async fn v1_reader_never_treats_orphan_format_as_legacy() {
    let (_instance, _account, db) = super::super::test_helpers::test_session_db().await;
    let mut session = new_session(&db).await;
    session.add_entry(entry("legacy original")).await.unwrap();
    let legacy = db.snapshot().await.unwrap();
    let op = operation(
        &db,
        &legacy,
        1,
        input("orphan v1"),
        View::Message(MessageRole::User),
        vec![],
    );
    let head = commit_op(&db, &op).await;
    let read = Session::history(&db).await.unwrap();
    assert!(
        read.context()
            .await
            .unwrap_err()
            .to_string()
            .contains("without a valid descriptor")
    );
    assert!(read.display_rows().await.is_err());
    assert_eq!(
        read.inspect_conversation()
            .await
            .unwrap()
            .unwrap()
            .operations
            .atoms
            .len(),
        1
    );
    assert!(
        Session::history_at(&db, &legacy)
            .await
            .unwrap()
            .inspect_conversation()
            .await
            .unwrap()
            .is_none()
    );
    assert!(session.add_entry(entry("must not write")).await.is_err());
    assert_eq!(db.snapshot().await.unwrap(), head);
}
