use super::*;
use crate::bridge::ApprovalDecision;
use crate::test_support::{
    MockBackend, empty_secrets, fresh_session, permissive_security, security_with_decision,
    tool_context,
};
use crate::tool::{ApprovalRequirement, Tool, ToolDescriptor, ToolError, ToolPolicy, ToolRegistry};
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

#[derive(Default)]
struct Recorder {
    intents: Mutex<Vec<JobWaitIntent>>,
    records: Mutex<Vec<RuntimeRecord>>,
    fail_finish: bool,
}

impl RuntimeRecorder for Recorder {
    fn record<'a>(
        &'a self,
        message: RuntimeRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        self.records.lock().unwrap().push(message);
        Box::pin(async { Ok(()) })
    }

    fn checkpoint_job_wait<'a>(
        &'a self,
        wait: JobWaitIntent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        self.intents.lock().unwrap().push(wait);
        Box::pin(async { Ok(()) })
    }

    fn finish_job_wait<'a>(
        &'a self,
        _: u64,
        _: usize,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if self.fail_finish {
                Err("retirement failed".into())
            } else {
                Ok(())
            }
        })
    }
}

/// A `job_wait` stand-in that records the deadline the runtime gave it.
struct Wait {
    deadlines: Arc<Mutex<Vec<Option<chrono::DateTime<chrono::Utc>>>>>,
    capacity: Option<Arc<Semaphore>>,
    hang: bool,
}

impl Tool for Wait {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_wait".into(),
            description: "test wait".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            timeout: 300,
            ..Default::default()
        }
    }

    fn execute<'a>(
        &'a self,
        _: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            self.deadlines
                .lock()
                .unwrap()
                .push(ctx.observation_deadline);
            if let Some(semaphore) = &self.capacity {
                assert_eq!(
                    semaphore.available_permits(),
                    1,
                    "a resumed wait observes without holding turn capacity"
                );
            }
            if self.hang {
                std::future::pending::<()>().await;
            }
            Ok("observed".into())
        })
    }
}

struct Effect {
    calls: Arc<AtomicUsize>,
    approval: bool,
    rate_limit: Option<u32>,
}

impl Tool for Effect {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "effect".into(),
            description: "test effect".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            approval: if self.approval {
                ApprovalRequirement::Always
            } else {
                ApprovalRequirement::Never
            },
            rate_limit: self.rate_limit,
            ..Default::default()
        }
    }

    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("effect ran".into())
        })
    }
}

fn wait_args(seconds: u64) -> String {
    serde_json::json!({"session_db_id": "child", "timeout_seconds": seconds}).to_string()
}

#[tokio::test]
async fn only_a_final_wait_in_a_restorable_turn_is_checkpointed() {
    for case in ["final", "not-final", "approve-all", "invalid"] {
        let deadlines = Arc::new(Mutex::new(Vec::new()));
        let effects = Arc::new(AtomicUsize::new(0));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Wait {
            deadlines: deadlines.clone(),
            capacity: None,
            hang: false,
        });
        tools.register(Effect {
            calls: effects.clone(),
            approval: case == "approve-all",
            rate_limit: None,
        });
        let (_instance, session) = fresh_session().await;
        let ctx = tool_context(session, tools);
        let mock = Arc::new(MockBackend::new());
        let wait = (
            "w".to_string(),
            "job_wait".to_string(),
            wait_args(if case == "invalid" { 0 } else { 30 }),
        );
        let effect = ("e".to_string(), "effect".to_string(), "{}".to_string());
        match case {
            "not-final" => mock.push_tool_calls([wait, effect]),
            "approve-all" => {
                mock.push_tool_calls([effect]);
                mock.push_tool_calls([wait]);
            }
            _ => mock.push_tool_calls([wait]),
        }
        mock.push_text("done");
        let backend = BackendManager::with_mock(mock, empty_secrets().await);
        let (security, _approver) = security_with_decision(ApprovalDecision::ApproveAll);
        let recorder = Arc::new(Recorder::default());
        let outcome = execute_with_recorder(
            None,
            vec![RuntimeMessage::User("go".into())],
            &backend,
            &security,
            &ctx,
            &ToolPolicyRegistry::empty(),
            Some(recorder.clone()),
            None,
            None,
            ModelCallScope::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.body, "done", "{case}");
        let intents = recorder.intents.lock().unwrap().clone();
        let deadlines = deadlines.lock().unwrap().clone();
        match case {
            "final" => {
                assert_eq!(intents.len(), 1);
                let intent = &intents[0];
                assert_eq!((intent.model_sequence, intent.call_index), (0, 0));
                assert_eq!(intent.call_id, "w");
                assert_eq!(intent.child_session_db_id, "child");
                assert_eq!(intent.policy_timeout_seconds, 300);
                assert_eq!(
                    intent.deadline - intent.checkpointed_at,
                    chrono::Duration::seconds(30)
                );
                assert_eq!(
                    deadlines,
                    [Some(intent.deadline)],
                    "the live wait uses the persisted deadline"
                );
            }
            "not-final" | "approve-all" => {
                assert!(intents.is_empty(), "{case} must not checkpoint");
                assert!(deadlines[0].is_some(), "{case} still uses a fixed deadline");
            }
            _ => {
                assert!(intents.is_empty());
                assert_eq!(deadlines, [None]);
            }
        }
    }
}

fn resume_fixture(
    deadline: chrono::DateTime<chrono::Utc>,
    policy_deadline: chrono::DateTime<chrono::Utc>,
) -> RuntimeResume {
    let mut provider_extra = serde_json::Map::new();
    provider_extra.insert("reasoning".into(), "kept".into());
    RuntimeResume {
        prior_messages: vec![
            RuntimeMessage::AssistantToolCalls {
                exchange_id: None,
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: "e".into(),
                    name: "effect".into(),
                    arguments: "{}".into(),
                }],
                provider_extra: Default::default(),
            },
            RuntimeMessage::ToolResult {
                call_id: "e".into(),
                content: wrap_tool_output("effect", "effect ran"),
            },
            RuntimeMessage::AssistantToolCalls {
                exchange_id: None,
                content: Some("waiting".into()),
                tool_calls: vec![ToolCallRequest {
                    id: "w".into(),
                    name: "job_wait".into(),
                    arguments: wait_args(30),
                }],
                provider_extra,
            },
        ],
        model_sequence: 1,
        call_index: 0,
        call: ToolCallRequest {
            id: "w".into(),
            name: "job_wait".into(),
            arguments: wait_args(30),
        },
        deadline,
        policy_deadline,
        policy_timeout_seconds: 300,
        metadata: vec![ResponseMetadata {
            model: "first".into(),
            usage: TokenUsage {
                prompt_tokens: 7,
                ..Default::default()
            },
            ..Default::default()
        }],
        matrix_send_queued: false,
        rate_limited_calls: vec![("effect".into(), chrono::Utc::now())],
    }
}

#[tokio::test]
async fn resume_runs_only_the_pending_wait_then_continues_the_turn() {
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let effects = Arc::new(AtomicUsize::new(0));
    let semaphore = Arc::new(Semaphore::new(1));
    let tools = Arc::new(ToolRegistry::new());
    tools.register(Wait {
        deadlines: deadlines.clone(),
        capacity: Some(semaphore.clone()),
        hang: false,
    });
    tools.register(Effect {
        calls: effects.clone(),
        approval: false,
        rate_limit: Some(1),
    });
    let (_instance, session) = fresh_session().await;
    let ctx = tool_context(session, tools);
    let mock = Arc::new(MockBackend::new());
    // The continuation asks for the effect again: the restored rate window
    // still counts the first run's call, so it is not re-executed.
    mock.push_tool_calls([("e2".into(), "effect".into(), "{}".into())]);
    mock.push_text("done");
    let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
    let recorder = Arc::new(Recorder::default());
    let deadline = chrono::Utc::now() + chrono::Duration::seconds(5);
    let permit = semaphore.clone().acquire_owned().await.unwrap();
    let outcome = resume_with_recorder(
        None,
        vec![RuntimeMessage::User("go".into())],
        &backend,
        &permissive_security(),
        &ctx,
        &ToolPolicyRegistry::empty(),
        Some(recorder.clone()),
        None,
        Some(ExecutionCapacity::new(semaphore.clone(), permit, None)),
        resume_fixture(
            deadline,
            chrono::Utc::now() + chrono::Duration::seconds(300),
        ),
        ModelCallScope::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.body, "done");
    assert_eq!(outcome.metadata.unwrap().usage.prompt_tokens, 7);
    assert_eq!(*deadlines.lock().unwrap(), [Some(deadline)]);
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "no earlier effect reruns"
    );
    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 2, "the model is not asked for the saved batch");
    let shape: Vec<String> = calls[0]
        .messages
        .iter()
        .map(|message| match message {
            RuntimeMessage::User(text) => format!("user:{text}"),
            RuntimeMessage::AssistantToolCalls { provider_extra, .. } => {
                format!("assistant:{}", provider_extra.len())
            }
            RuntimeMessage::ToolResult { call_id, content } => {
                format!("result:{call_id}:{}", content.contains("observed"))
            }
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "user:go",
            "assistant:0",
            "result:e:false",
            "assistant:1",
            "result:w:true"
        ]
    );
    let records = recorder.records.lock().unwrap().clone();
    assert!(matches!(&records[0],
        RuntimeRecord::ToolResult { model_sequence: 1, call_index: 0, call_id, outcome: ToolResultOutcome::Success, .. } if call_id == "w"));
    assert!(matches!(
        &records[1],
        RuntimeRecord::ModelResponse {
            model_sequence: 2,
            terminal: false,
            ..
        }
    ));
    assert!(matches!(
        &records[2],
        RuntimeRecord::ToolResult {
            model_sequence: 2,
            outcome: ToolResultOutcome::RateLimited,
            ..
        }
    ));
    assert!(matches!(
        &records[3],
        RuntimeRecord::ModelResponse {
            model_sequence: 3,
            terminal: true,
            ..
        }
    ));
    assert_eq!(records.len(), 4);
    assert!(
        recorder.intents.lock().unwrap().is_empty(),
        "the resumed wait reuses its checkpoint"
    );
    assert_eq!(semaphore.available_permits(), 1, "no leaked capacity");
}

#[tokio::test]
async fn expired_policy_budget_reports_the_original_timeout_without_waiting() {
    let tools = Arc::new(ToolRegistry::new());
    tools.register(Wait {
        deadlines: Default::default(),
        capacity: None,
        hang: true,
    });
    let (_instance, session) = fresh_session().await;
    let ctx = tool_context(session, tools);
    let mock = Arc::new(MockBackend::new());
    mock.push_text("done");
    let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
    let past = chrono::Utc::now() - chrono::Duration::seconds(1);
    let started = std::time::Instant::now();
    tokio::time::timeout(
        Duration::from_secs(5),
        resume_with_recorder(
            None,
            vec![RuntimeMessage::User("go".into())],
            &backend,
            &permissive_security(),
            &ctx,
            &ToolPolicyRegistry::empty(),
            None,
            None,
            None,
            resume_fixture(past, past),
            ModelCallScope::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        mock.recorded_calls()[0]
            .messages
            .iter()
            .any(|message| matches!(message,
        RuntimeMessage::ToolResult { call_id, content } if call_id == "w"
            && content.contains("Tool timed out after 300 seconds")))
    );
}

#[tokio::test]
async fn retirement_failure_stops_live_and_resumed_turns_before_recording_or_continuing() {
    for resumed in [false, true] {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Wait {
            deadlines: Default::default(),
            capacity: None,
            hang: false,
        });
        let (_instance, session) = fresh_session().await;
        let ctx = tool_context(session, tools);
        let mock = Arc::new(MockBackend::new());
        if !resumed {
            mock.push_tool_calls([("w".into(), "job_wait".into(), wait_args(30))]);
        }
        mock.push_text("must not continue");
        let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
        let recorder = Arc::new(Recorder {
            fail_finish: true,
            ..Default::default()
        });
        let now = chrono::Utc::now();
        let result = run_turn(
            None,
            vec![RuntimeMessage::User("go".into())],
            &backend,
            &permissive_security(),
            &ctx,
            &ToolPolicyRegistry::empty(),
            Some(recorder.clone()),
            None,
            None,
            None,
            resumed.then(|| {
                resume_fixture(
                    now + chrono::Duration::seconds(30),
                    now + chrono::Duration::seconds(300),
                )
            }),
            ModelCallScope::default(),
        )
        .await;
        assert_eq!(
            result.err().map(|error| error.to_string()).as_deref(),
            Some("runtime persistence failed: retirement failed")
        );
        assert_eq!(mock.recorded_calls().len(), usize::from(!resumed));
        assert!(
            recorder
                .records
                .lock()
                .unwrap()
                .iter()
                .all(|record| matches!(
                    record,
                    RuntimeRecord::ModelResponse {
                        terminal: false,
                        ..
                    }
                ))
        );
    }
}

#[tokio::test]
async fn resumed_wait_obeys_a_shorter_current_policy_timeout() {
    let tools = Arc::new(ToolRegistry::new());
    tools.register(Wait {
        deadlines: Default::default(),
        capacity: None,
        hang: true,
    });
    let (_instance, session) = fresh_session().await;
    let ctx = tool_context(session, tools);
    let mock = Arc::new(MockBackend::new());
    mock.push_text("done");
    let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
    let policies = ToolPolicyRegistry::new(std::collections::HashMap::from([(
        "job_wait".into(),
        ToolPolicy {
            timeout: 1,
            ..Default::default()
        },
    )]));
    let now = chrono::Utc::now();
    tokio::time::timeout(
        Duration::from_secs(3),
        resume_with_recorder(
            None,
            vec![RuntimeMessage::User("go".into())],
            &backend,
            &permissive_security(),
            &ctx,
            &policies,
            None,
            None,
            None,
            resume_fixture(
                now + chrono::Duration::seconds(30),
                now + chrono::Duration::seconds(300),
            ),
            ModelCallScope::default(),
        ),
    )
    .await
    .expect("current policy must bound the resumed wait")
    .unwrap();
    assert!(mock.recorded_calls()[0].messages.iter().any(|message| matches!(message,
        RuntimeMessage::ToolResult { content, .. } if content.contains("Tool timed out after 1 seconds"))));
}
