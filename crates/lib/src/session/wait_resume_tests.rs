use super::*;
use crate::runtime::ToolCallRequest;
use crate::test_support::fresh_session;

const ATTEMPT: &str = "attempt";
const CHILD: &str = "child-db";

fn request() -> TurnRequestId {
    TurnRequestId::parse("request")
}

fn record(sequence: u64, message: TurnTranscriptMessage) -> TurnTranscriptRecord {
    TurnTranscriptRecord {
        request_id: request(),
        attempt_id: ATTEMPT.into(),
        sequence,
        timestamp: Utc::now(),
        message,
    }
}

fn call(id: &str, name: &str) -> ToolCallRequest {
    ToolCallRequest {
        id: id.into(),
        name: name.into(),
        arguments: if name == "job_wait" {
            serde_json::json!({"session_db_id": CHILD, "timeout_seconds": 30}).to_string()
        } else {
            "{}".into()
        },
    }
}

fn model(sequence: u64, model_sequence: u64, calls: &[(&str, &str)]) -> TurnTranscriptRecord {
    let mut provider_extra = serde_json::Map::new();
    provider_extra.insert("reasoning".into(), format!("step-{model_sequence}").into());
    record(
        sequence,
        TurnTranscriptMessage::ModelResponse {
            model_sequence,
            content: Some(format!("thinking {model_sequence}")),
            tool_calls: calls.iter().map(|(id, name)| call(id, name)).collect(),
            provider_extra,
            metadata: Some(crate::runtime::ResponseMetadata {
                model: format!("m{model_sequence}"),
                ..Default::default()
            }),
            terminal: false,
        },
    )
}

fn result(
    sequence: u64,
    model_sequence: u64,
    call_index: usize,
    (id, name): (&str, &str),
    outcome: ToolResultOutcome,
) -> TurnTranscriptRecord {
    record(
        sequence,
        TurnTranscriptMessage::ToolResult {
            model_sequence,
            call_index,
            call_id: id.into(),
            name: name.into(),
            output: format!("{name} <output>"),
            outcome,
        },
    )
}

async fn checkpoint(model_sequence: u64, call_index: usize, call_id: &str) -> JobWaitCheckpoint {
    let (_instance, session) = fresh_session().await;
    let snapshot = session.lock().await.database().snapshot().await.unwrap();
    let checkpointed_at = Utc::now();
    JobWaitCheckpoint::new(
        request(),
        ATTEMPT.into(),
        "alpha".into(),
        snapshot,
        JobWaitIntent {
            model_sequence,
            call_index,
            call_id: call_id.into(),
            arguments: call(call_id, "job_wait").arguments,
            child_session_db_id: CHILD.into(),
            checkpointed_at,
            deadline: checkpointed_at + chrono::Duration::seconds(30),
            policy_timeout_seconds: 300,
        },
    )
}

/// Earlier effect A, then a batch [B, wait] with B done and the wait pending.
fn effect_then_partial_batch() -> Vec<TurnTranscriptRecord> {
    vec![
        model(0, 0, &[("a", "effect")]),
        result(1, 0, 0, ("a", "effect"), ToolResultOutcome::Success),
        model(2, 1, &[("b", "matrix__send"), ("w", "job_wait")]),
        result(3, 1, 0, ("b", "matrix__send"), ToolResultOutcome::Success),
    ]
}

#[tokio::test]
async fn pending_final_wait_reuses_every_earlier_record_verbatim() {
    let checkpoint = checkpoint(1, 1, "w").await;
    let (resume, next) = plan_job_wait_resume(&checkpoint, &effect_then_partial_batch()).unwrap();
    assert_eq!(next, 4, "the resumed result follows the persisted records");
    assert_eq!(resume.model_sequence, 1);
    assert_eq!(resume.call_index, 1);
    assert_eq!(resume.call.id, "w");
    assert_eq!(resume.call.arguments, checkpoint.arguments);
    assert_eq!(resume.deadline, checkpoint.deadline, "deadline never moves");
    assert_eq!(
        resume.policy_deadline,
        checkpoint.checkpointed_at + chrono::Duration::seconds(300)
    );
    assert!(resume.matrix_send_queued);
    assert_eq!(
        resume
            .metadata
            .iter()
            .map(|m| m.model.as_str())
            .collect::<Vec<_>>(),
        ["m0", "m1"]
    );
    assert_eq!(
        resume
            .rate_limited_calls
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["effect", "matrix__send", "job_wait"]
    );
    let shape: Vec<String> = resume
        .prior_messages
        .iter()
        .map(|message| match message {
            RuntimeMessage::AssistantToolCalls {
                tool_calls,
                provider_extra,
                ..
            } => format!(
                "assistant:{}:{}",
                tool_calls
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                provider_extra["reasoning"]
            ),
            RuntimeMessage::ToolResult { call_id, content } => {
                assert!(content.contains("&lt;output&gt;"), "{content}");
                format!("result:{call_id}")
            }
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "assistant:a:\"step-0\"",
            "result:a",
            "assistant:b,w:\"step-1\"",
            "result:b",
        ]
    );
}

#[tokio::test]
async fn denied_results_keep_the_unwrapped_live_form() {
    let checkpoint = checkpoint(1, 0, "w").await;
    let records = vec![
        model(0, 0, &[("a", "effect")]),
        result(1, 0, 0, ("a", "effect"), ToolResultOutcome::Denied),
        model(2, 1, &[("w", "job_wait")]),
    ];
    let (resume, _) = plan_job_wait_resume(&checkpoint, &records).unwrap();
    assert!(matches!(&resume.prior_messages[1],
        RuntimeMessage::ToolResult { content, .. } if content == "effect <output>"));
}

#[tokio::test]
async fn ambiguous_or_advanced_attempts_are_held() {
    let base = effect_then_partial_batch();
    let pending = checkpoint(1, 1, "w").await;
    let with_result = {
        let mut records = base.clone();
        records.push(result(
            4,
            1,
            1,
            ("w", "job_wait"),
            ToolResultOutcome::Success,
        ));
        records
    };
    let advanced = {
        let mut records = with_result.clone();
        records.push(model(5, 2, &[("c", "effect")]));
        records
    };
    let gap = {
        let mut records = base.clone();
        records.remove(1);
        records
    };
    let other_attempt = {
        let mut records = base.clone();
        records[3].attempt_id = "other".into();
        records
    };
    let mispaired = {
        let mut records = base.clone();
        records[3] = result(3, 1, 0, ("x", "matrix__send"), ToolResultOutcome::Success);
        records
    };
    let unknown_outcome = {
        let mut records = base.clone();
        records[1] = result(
            1,
            0,
            0,
            ("a", "effect"),
            ToolResultOutcome::Unknown {
                kind: "Future".into(),
                raw: serde_json::json!("Future"),
            },
        );
        records
    };
    let terminal = {
        let mut records = base.clone();
        if let TurnTranscriptMessage::ModelResponse { terminal, .. } = &mut records[0].message {
            *terminal = true;
        }
        records
    };
    let missing_batch_result = base[..3].to_vec();
    for (name, records, checkpoint) in [
        ("result recorded", with_result, pending.clone()),
        ("turn advanced", advanced, pending.clone()),
        ("sequence gap", gap, pending.clone()),
        ("other attempt", other_attempt, pending.clone()),
        ("mispaired result", mispaired, pending.clone()),
        ("unknown outcome", unknown_outcome, pending.clone()),
        ("terminal response", terminal, pending.clone()),
        (
            "missing batch result",
            missing_batch_result,
            pending.clone(),
        ),
        ("before checkpoint", Vec::new(), pending.clone()),
        ("wait not last", base.clone(), checkpoint(1, 0, "b").await),
        ("wrong call id", base.clone(), checkpoint(1, 1, "z").await),
    ] {
        assert!(
            plan_job_wait_resume(&checkpoint, &records).is_err(),
            "{name} must hold"
        );
    }
}

#[tokio::test]
async fn malformed_or_unknown_checkpoints_are_held() {
    let records = effect_then_partial_batch();
    let valid = checkpoint(1, 1, "w").await;
    assert!(plan_job_wait_resume(&valid, &records).is_ok());
    let mut variants = Vec::new();
    let mut future = valid.clone();
    future.version = JOB_WAIT_CHECKPOINT_VERSION + 1;
    variants.push(("future version", future));
    let mut long = valid.clone();
    long.deadline = long.checkpointed_at + chrono::Duration::seconds(241);
    variants.push(("deadline beyond bound", long));
    let mut backwards = valid.clone();
    backwards.deadline = backwards.checkpointed_at;
    variants.push(("deadline not after start", backwards));
    let mut other_child = valid.clone();
    other_child.child_session_db_id = "another-child".into();
    variants.push(("arguments name another child", other_child));
    let mut no_policy = valid.clone();
    no_policy.policy_timeout_seconds = 0;
    variants.push(("no policy timeout", no_policy));
    let mut finished = valid.clone();
    finished.observation_finished = true;
    variants.push(("observation finished", finished));
    let mut index_overflow = valid.clone();
    index_overflow.call_index = usize::MAX;
    variants.push(("call index overflow", index_overflow));
    let mut policy_overflow = valid.clone();
    policy_overflow.policy_timeout_seconds = u64::MAX;
    variants.push(("policy deadline overflow", policy_overflow));
    for (name, checkpoint) in variants {
        assert!(
            plan_job_wait_resume(&checkpoint, &records).is_err(),
            "{name} must hold"
        );
    }
}

#[tokio::test]
async fn newest_checkpoint_is_selected_and_rewrites_are_idempotent() {
    let (_instance, session) = fresh_session().await;
    let session = session.lock().await;
    let first = checkpoint(0, 0, "w0").await;
    let second = checkpoint(2, 0, "w2").await;
    for checkpoint in [&second, &first, &second] {
        session
            .record_job_wait_checkpoint(checkpoint)
            .await
            .unwrap();
    }
    assert_eq!(
        session.latest_job_wait_checkpoint(ATTEMPT).await.unwrap(),
        Some(second)
    );
    assert!(
        session
            .latest_job_wait_checkpoint("other")
            .await
            .unwrap()
            .is_none()
    );
    let txn = session.database().new_transaction().await.unwrap();
    assert_eq!(
        txn.get_store::<Table<JobWaitCheckpoint>>(JOB_WAIT_CHECKPOINTS_STORE)
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn steering_and_nonterminal_text_keep_exact_call_sources_or_refuse() {
    let checkpoint = checkpoint(2, 0, "w").await;
    let mut rows = effect_then_partial_batch();
    rows.truncate(2);
    rows.push(model(2, 1, &[]));
    let input = super::super::steering::JobInputRequest::new(
        ATTEMPT.into(),
        "continue with the small example".into(),
    );
    rows.push(record(
        3,
        TurnTranscriptMessage::Steering {
            model_sequence: 2,
            inputs: vec![input.clone()],
        },
    ));
    rows.push(model(4, 2, &[("w", "job_wait")]));
    let (resume, next) = plan_job_wait_resume(&checkpoint, &rows).unwrap();
    assert_eq!(next, 5);
    assert_eq!(resume.prior_model_sequences, [0, 0, 1, 2, 2]);
    assert_eq!(
        resume.prior_messages[2],
        RuntimeMessage::Assistant("thinking 1".into())
    );
    assert_eq!(
        resume.prior_messages[3],
        RuntimeMessage::User(input.text.clone())
    );
    // Empty responses still advance the call sequence without adding text.
    if let TurnTranscriptMessage::ModelResponse { content, .. } = &mut rows[2].message {
        *content = Some(String::new());
    }
    let (resume, _) = plan_job_wait_resume(&checkpoint, &rows).unwrap();
    assert_eq!(resume.prior_model_sequences, [0, 0, 2, 2]);
    let raw = serde_json::to_value(&rows[3].message).unwrap();
    assert_eq!(
        serde_json::from_value::<TurnTranscriptMessage>(raw.clone()).unwrap(),
        rows[3].message
    );
    for bad in ["wrong-attempt", "empty", "duplicate", "unknown"] {
        let mut changed = rows.clone();
        if let TurnTranscriptMessage::Steering { inputs, .. } = &mut changed[3].message {
            match bad {
                "wrong-attempt" => inputs[0].attempt_id = "another".into(),
                "empty" => inputs[0].text.clear(),
                "duplicate" => inputs.push(inputs[0].clone()),
                _ => {}
            }
        }
        if bad == "unknown" {
            changed[3].message = TurnTranscriptMessage::Unknown {
                kind: "FutureSteering".into(),
                raw: serde_json::json!({"FutureSteering":raw}),
            };
        }
        assert!(
            plan_job_wait_resume(&checkpoint, &changed).is_err(),
            "{bad} must hold, not partially resume"
        );
    }
}
