use super::*;
use chaz_core::{
    runtime::ToolCallRequest,
    session::{TurnCallIdentity, TurnRequestId},
};

fn invocation() -> TurnTranscriptRecord {
    TurnTranscriptRecord {
        request_id: TurnRequestId::parse("request"),
        attempt_id: "attempt".into(),
        sequence: 0,
        timestamp: Utc::now(),
        identity: Some(TurnCallIdentity {
            agent: "chaz".into(),
            requested_model: "fixture".into(),
            execution_id: Some("branch".into()),
        }),
        message: TurnTranscriptMessage::ModelResponse {
            model_sequence: 2,
            content: None,
            terminal: false,
            tool_calls: vec![ToolCallRequest {
                id: "same-provider-id".into(),
                name: "query".into(),
                arguments: "{\"partial\":".into(),
            }],
            provider_extra: Default::default(),
            metadata: None,
        },
    }
}

fn response() -> TurnTranscriptRecord {
    TurnTranscriptRecord {
        sequence: 1,
        message: TurnTranscriptMessage::ToolResult {
            model_sequence: 2,
            call_index: 0,
            call_id: "same-provider-id".into(),
            name: "query".into(),
            output: "retained output".into(),
            outcome: ToolResultOutcome::Success,
        },
        ..invocation()
    }
}

fn row(record: &TurnTranscriptRecord, kind: EntryType) -> (String, SessionEntry) {
    (
        RunKey::from_record(record)
            .unwrap()
            .row_id(record.sequence, 0),
        SessionEntry {
            sender: record.identity.as_ref().unwrap().agent.clone(),
            content: "audit text is not identity".into(),
            timestamp: record.timestamp,
            entry_type: kind,
            metadata: None,
            routing: None,
        },
    )
}

fn project(rows: &[(String, SessionEntry)], records: Vec<TurnTranscriptRecord>) -> ToolActivity {
    ToolActivity::project(
        rows.iter().map(|(id, row)| (id.as_str(), row)),
        records.into_iter(),
        std::iter::empty(),
    )
}

#[test]
fn nested_tool_pairing_requires_every_identity_component_and_matching_name() {
    for component in [
        "request",
        "attempt",
        "execution",
        "agent",
        "model",
        "index",
        "id",
        "name",
    ] {
        let call = invocation();
        let mut result = response();
        match component {
            "request" => result.request_id = TurnRequestId::parse("other-request"),
            "attempt" => result.attempt_id = "other-attempt".into(),
            "execution" => {
                result.identity.as_mut().unwrap().execution_id = Some("other-branch".into())
            }
            "agent" => result.identity.as_mut().unwrap().agent = "other-agent".into(),
            _ => {
                if let TurnTranscriptMessage::ToolResult {
                    model_sequence,
                    call_index,
                    call_id,
                    name,
                    ..
                } = &mut result.message
                {
                    match component {
                        "model" => *model_sequence += 1,
                        "index" => *call_index += 1,
                        "id" => *call_id = "other-id".into(),
                        "name" => *name = "other-tool".into(),
                        _ => unreachable!(),
                    }
                }
            }
        }
        let rows = [
            row(&call, EntryType::ToolCall),
            row(&result, EntryType::ToolResult),
        ];
        let projection = project(&rows, vec![result, call]);
        assert!(
            projection.items.iter().all(|i| i.result.is_none()),
            "mismatched {component}"
        );
        assert!(
            !projection
                .rows
                .values()
                .any(|r| matches!(r, ToolRow::Result)),
            "never consume unmatched {component}"
        );
    }
    let call = invocation();
    let result = response();
    let rows = [
        row(&call, EntryType::ToolCall),
        row(&result, EntryType::ToolResult),
    ];
    let projection = project(&rows, vec![result, call]);
    assert_eq!(
        projection.items[0].result.as_ref().unwrap().output,
        "retained output"
    );
    assert_eq!(projection.items[0].arguments, "{\"partial\":");
}

#[test]
fn nested_tool_ambiguous_or_wrong_audit_anchors_are_never_suppressed() {
    let call = invocation();
    let result = response();
    let rows = [
        row(&call, EntryType::ToolCall),
        row(&result, EntryType::ToolResult),
    ];
    for records in [
        vec![call.clone(), call.clone(), result.clone()],
        vec![call.clone(), result.clone(), result.clone()],
    ] {
        let projection = project(&rows, records);
        assert!(
            projection.rows.is_empty(),
            "conflicting identities remain visible as unpaired audit rows"
        );
    }
    for (index, change_kind) in [(0, true), (0, false), (1, true), (1, false)] {
        let mut rows = rows.clone();
        if change_kind {
            rows[index].1.entry_type = EntryType::Message;
        } else {
            rows[index].1.sender = "other-agent".into();
        }
        let projection = project(&rows, vec![call.clone(), result.clone()]);
        assert!(
            !projection.rows.contains_key(&rows[index].0),
            "never hide prose or another sender's row"
        );
        assert!(
            !projection
                .rows
                .values()
                .any(|r| matches!(r, ToolRow::Result))
        );
    }
    let mut unknown_call = call.clone();
    unknown_call.identity = None;
    assert!(
        project(&rows, vec![unknown_call, result.clone()])
            .rows
            .is_empty()
    );
    let mut other = invocation();
    other.sequence = 10;
    other.request_id = TurnRequestId::parse("another-request");
    // Same display anchor claimed by two actual identities cannot be resolved by order.
    let mut conflicting = call.clone();
    conflicting.request_id = other.request_id.clone();
    assert!(
        project(&rows, vec![call, conflicting, result])
            .rows
            .is_empty()
    );
}

#[test]
fn nested_tool_completed_attempt_is_not_competing_branch_completion() {
    let call = invocation();
    let rows = [row(&call, EntryType::ToolCall)];
    let completed = TurnAttempt {
        attempt_id: call.attempt_id.clone(),
        request_id: call.request_id.clone(),
        generation: 0,
        started_at: call.timestamp,
        status: TurnAttemptStatus::Completed,
        completed_at: Some(Utc::now()),
    };
    let projection = ToolActivity::project(
        rows.iter().map(|(id, row)| (id.as_str(), row)),
        [call].into_iter(),
        [completed].into_iter(),
    );
    assert_eq!(
        projection.runs[0].lifecycle(),
        "attempt completed elsewhere; branch state unknown"
    );
    assert_eq!(
        projection.items[0].status(&projection.runs[0]),
        "result not recorded; branch state unknown"
    );
}
