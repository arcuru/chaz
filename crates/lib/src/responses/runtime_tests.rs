//! Offline runtime, durable reopen and pending-wait recovery through the native wire client.
use super::tests::{backend, final_output, output, response};
use super::*;
use crate::{
    backends::BackendManager,
    config::{ContextConfig, ModelApi},
    context::ContextBuilder,
    runtime::{self, ModelCallScope, RuntimeRecord, RuntimeRecorder, ToolResultOutcome},
    session::{
        EntryType, Session, SessionEntry, TurnAttempt, TurnTranscriptMessage, TurnTranscriptRecord,
    },
    test_support::{empty_secrets, fresh_session, permissive_security, tool_context},
    tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicyRegistry, ToolRegistry},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Mutex as AsyncMutex;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

pub(super) struct Echo {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) name: &'static str,
}
impl Tool for Echo {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.into(),
            description: "Echo or observe".into(),
            parameters: json!({"type":"object", "properties":{"text":{"type":"string"}}}),
        }
    }
    fn execute<'a>(
        &'a self,
        arguments: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if arguments["text"] == "first" {
                Err(ToolError::Execution("first failed".into()))
            } else {
                Ok(arguments["text"].as_str().unwrap_or("observed").into())
            }
        })
    }
}
pub(super) struct Recorder {
    pub(super) session: Arc<AsyncMutex<Session>>,
    pub(super) attempt: TurnAttempt,
    pub(super) sequence: Mutex<u64>,
}
impl RuntimeRecorder for Recorder {
    fn record<'a>(
        &'a self,
        record: RuntimeRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let sequence = {
                let mut n = self.sequence.lock().unwrap();
                let next = *n;
                *n += 1;
                next
            };
            let message = match record {
                RuntimeRecord::Cancelled => TurnTranscriptMessage::Cancelled,
                RuntimeRecord::ModelResponse {
                    model_sequence,
                    content,
                    tool_calls,
                    provider_extra,
                    metadata,
                    terminal,
                } => TurnTranscriptMessage::ModelResponse {
                    model_sequence,
                    content,
                    tool_calls,
                    provider_extra,
                    metadata,
                    terminal,
                },
                RuntimeRecord::ToolResult {
                    model_sequence,
                    call_index,
                    call_id,
                    name,
                    output,
                    outcome,
                } => TurnTranscriptMessage::ToolResult {
                    model_sequence,
                    call_index,
                    call_id,
                    name,
                    output,
                    outcome,
                },
            };
            self.session
                .lock()
                .await
                .append_turn_transcript(
                    TurnTranscriptRecord {
                        request_id: self.attempt.request_id.clone(),
                        attempt_id: self.attempt.attempt_id.clone(),
                        sequence,
                        timestamp: chrono::Utc::now(),
                        message,
                    },
                    vec![],
                )
                .await
                .map_err(|e| e.to_string())
        })
    }
}
pub(super) fn entry(
    sender: &str,
    content: &str,
    metadata: Option<ResponseMetadata>,
) -> SessionEntry {
    SessionEntry {
        sender: sender.into(),
        content: content.into(),
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Message,
        metadata,
        routing: None,
    }
}
pub(super) async fn context(session: &Session) -> Vec<RuntimeMessage> {
    let (entries, history) = session.context_with_tool_history().await.unwrap();
    ContextBuilder::new(
        &entries,
        "test-agent",
        "Policy",
        &ContextConfig {
            max_context_tokens: 8192,
            reserved_output_tokens: 256,
        },
    )
    .with_tool_history(&history)
    .build()
    .await
    .messages
}
pub(super) fn native_backend(base: &str) -> Backend {
    let mut config = backend(base);
    config.models.as_mut().unwrap()[0].api = Some(ModelApi::Responses);
    config
}

#[tokio::test]
async fn native_runtime_persists_full_replay_usage_and_identity_across_reopen() {
    let server = MockServer::start().await;
    let secrets = empty_secrets().await;
    let config = native_backend(&server.uri());
    let manager = BackendManager::new(&Some(vec![config.clone()]), secrets.clone());
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(output())))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let mut final_response = response(final_output());
    final_response["usage"]["input_tokens_details"]["cached_tokens"] = 0.into();
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(final_response))
        .with_priority(2)
        .expect(2)
        .mount(&server)
        .await;
    let (_instance, session) = fresh_session().await;
    let id = session
        .lock()
        .await
        .add_entry(entry("user", "question", None))
        .await
        .unwrap();
    let attempt = session.lock().await.start_turn_attempt(id).await.unwrap();
    let recorder = Arc::new(Recorder {
        session: session.clone(),
        attempt: attempt.clone(),
        sequence: Mutex::new(0),
    });
    let tools = Arc::new(ToolRegistry::new());
    let calls = Arc::new(AtomicUsize::new(0));
    tools.register(Echo {
        calls: calls.clone(),
        name: "echo",
    });
    let ctx = tool_context(session.clone(), tools.clone());
    let initial = context(&*session.lock().await).await;
    let result = runtime::execute_with_recorder(
        Some("test-model"),
        initial,
        &manager,
        &permissive_security(),
        &ctx,
        &ToolPolicyRegistry::empty(),
        Some(recorder),
        None,
        None,
        ModelCallScope::default(),
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.body, "Done");
    assert_eq!(
        result.metadata.as_ref().unwrap().usage.cached_tokens,
        Some(60)
    );
    assert_eq!(result.metadata.as_ref().unwrap().usage.prompt_tokens, 200);
    session
        .lock()
        .await
        .complete_turn_attempt(
            &attempt,
            Some(entry("test-agent", &result.body, result.metadata)),
        )
        .await
        .unwrap();
    let db = session.lock().await.database().clone();
    let reopened = Session::new(session.lock().await.conversation_id.clone(), db.clone()).await;
    let ledger = crate::session::usage::collect_session_usage(
        reopened.entries(),
        &crate::session::usage::UsageFilter::default(),
        &mut std::collections::BTreeMap::new(),
    );
    assert_eq!(ledger.prompt_tokens, 200);
    assert_eq!(ledger.cached_tokens, 60);
    assert_eq!(ledger.completion_tokens, 24);
    assert_eq!(
        reopened
            .entries()
            .last()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .usage
            .cached_tokens,
        Some(60)
    );
    let before = context(&*session.lock().await).await;
    let after = context(&reopened).await;
    assert_eq!(before, after);
    let client = Responses::new(&config, &secrets);
    let key = secrets.session_cache_key(
        &db.root_id().to_string(),
        "legacy-agent:test-agent",
        &config,
    );
    let payload = client
        .request(&after, &ctx.definitions(), "test-model", Some(&key))
        .unwrap();
    assert_eq!(&payload.input[1..6], output());
    assert_eq!(payload.input[6]["call_id"], "call1");
    assert!(
        payload.input[6]["output"]
            .as_str()
            .unwrap()
            .contains("first failed")
    );
    assert_eq!(payload.input[7]["call_id"], "call2");
    assert_eq!(&payload.input[8..10], final_output());
    assert_eq!(
        payload.input.len(),
        10,
        "terminal message is not flattened or duplicated"
    );
    let reopened = Arc::new(AsyncMutex::new(reopened));
    reopened
        .lock()
        .await
        .add_entry(entry("user", "next", None))
        .await
        .unwrap();
    let initial = context(&*reopened.lock().await).await;
    runtime::execute(
        Some("test-model"),
        initial,
        &manager,
        &permissive_security(),
        &tool_context(reopened, tools),
        &ToolPolicyRegistry::empty(),
        None,
        None,
    )
    .await
    .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let bodies: Vec<Value> = requests.iter().map(|r| r.body_json().unwrap()).collect();
    assert!(bodies.iter().all(|r| r["prompt_cache_key"] == key));
    assert_eq!(&bodies[1]["input"].as_array().unwrap()[1..6], output());
    assert_eq!(
        &bodies[2]["input"].as_array().unwrap()[8..10],
        final_output()
    );
    assert!(
        bodies
            .iter()
            .all(|r| !r.to_string().contains(&db.root_id().to_string()))
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "reopen never reruns historical tools"
    );
}

async fn pending_change(change: &str) {
    use crate::session::wait_resume::{JobWaitCheckpoint, plan_job_wait_resume};
    let server = MockServer::start().await;
    let secrets = empty_secrets().await;
    let config = native_backend(&server.uri());
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(final_output())))
        .mount(&server)
        .await;
    let (_instance, session) = fresh_session().await;
    let id = session
        .lock()
        .await
        .add_entry(entry("user", "question", None))
        .await
        .unwrap();
    let attempt = session.lock().await.start_turn_attempt(id).await.unwrap();
    let now = chrono::Utc::now();
    let mut items = output();
    items[4]["name"] = "job_wait".into();
    items[4]["arguments"] = json!({"session_db_id":"child", "timeout_seconds":30})
        .to_string()
        .into();
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(items.clone())))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let LLMResponse::ToolCalls {
        content,
        tool_calls,
        mut provider_extra,
        metadata,
    } = Responses::new(&config, &secrets)
        .chat_with_tools(&[], &[], "test-model", None)
        .await
        .unwrap()
    else {
        panic!("calls")
    };
    if change == "legacy" {
        provider_extra.remove("chaz_responses_origin");
    }
    let records = vec![
        TurnTranscriptRecord {
            request_id: attempt.request_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            sequence: 0,
            timestamp: now,
            message: TurnTranscriptMessage::ModelResponse {
                model_sequence: 0,
                content,
                tool_calls: tool_calls.clone(),
                provider_extra,
                metadata,
                terminal: false,
            },
        },
        TurnTranscriptRecord {
            request_id: attempt.request_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            sequence: 1,
            timestamp: now,
            message: TurnTranscriptMessage::ToolResult {
                model_sequence: 0,
                call_index: 0,
                call_id: "call1".into(),
                name: "echo".into(),
                output: "already executed".into(),
                outcome: ToolResultOutcome::Success,
            },
        },
    ];
    for record in &records {
        session
            .lock()
            .await
            .append_turn_transcript(record.clone(), vec![])
            .await
            .unwrap();
    }
    let snapshot = session.lock().await.database().snapshot().await.unwrap();
    let checkpoint = JobWaitCheckpoint::new(
        attempt.request_id.clone(),
        attempt.attempt_id.clone(),
        "test-agent".into(),
        snapshot,
        runtime::JobWaitIntent {
            model_sequence: 0,
            call_index: 1,
            call_id: "call2".into(),
            arguments: tool_calls[1].arguments.clone(),
            child_session_db_id: "child".into(),
            checkpointed_at: now,
            deadline: now + chrono::Duration::seconds(30),
            policy_timeout_seconds: 300,
        },
    );
    session
        .lock()
        .await
        .record_job_wait_checkpoint(&checkpoint)
        .await
        .unwrap();
    let (conversation, database) = {
        let s = session.lock().await;
        (s.conversation_id.clone(), s.database().clone())
    };
    let reopened = Session::new(conversation, database).await;
    let saved = reopened
        .latest_job_wait_checkpoint(&attempt.attempt_id)
        .await
        .unwrap()
        .unwrap();
    let (resume, next_sequence) = plan_job_wait_resume(&saved, &records).unwrap();
    assert_eq!(next_sequence, 2);
    let tools = Arc::new(ToolRegistry::new());
    let prior = Arc::new(AtomicUsize::new(0));
    let wait = Arc::new(AtomicUsize::new(0));
    tools.register(Echo {
        calls: prior.clone(),
        name: "echo",
    });
    tools.register(Echo {
        calls: wait.clone(),
        name: "job_wait",
    });
    let db_id = reopened.database().root_id().to_string();
    let mut target = config.clone();
    let model = if change == "model" {
        "other-model"
    } else {
        "test-model"
    };
    match change {
        "endpoint" => target.api_base = Some(format!("{}/changed", server.uri())),
        "namespace" => target.api_key_ref = Some("other-namespace".into()),
        "connection" => target.name = Some("east".into()),
        "api" => target.models.as_mut().unwrap()[0].api = Some(ModelApi::ChatCompletions),
        "incompatible-api" => {
            target.models.as_mut().unwrap()[0].api = Some(ModelApi::AnthropicMessages)
        }
        "model" => target.api = Some(ModelApi::Responses),
        "same" | "legacy" => {}
        _ => panic!("unknown change"),
    }
    let manager = BackendManager::new(&Some(vec![target]), secrets.clone());
    let ctx = tool_context(Arc::new(AsyncMutex::new(reopened)), tools);
    let result = runtime::resume_with_recorder(
        Some(model),
        vec![RuntimeMessage::User("question".into())],
        &manager,
        &permissive_security(),
        &ctx,
        &ToolPolicyRegistry::empty(),
        None,
        None,
        None,
        resume.clone(),
        ModelCallScope::default(),
    )
    .await;
    if change != "same" {
        let error = result
            .expect_err("incompatible pending continuation must refuse")
            .to_string();
        assert!(error.contains("Pending Responses continuation"), "{error}");
        assert_eq!(prior.load(Ordering::SeqCst), 0);
        assert_eq!(
            wait.load(Ordering::SeqCst),
            0,
            "refuse before observing/retiring the pending wait"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "no resumed HTTP"
        );
        assert!(
            ctx.session
                .lock()
                .await
                .latest_job_wait_checkpoint(&attempt.attempt_id)
                .await
                .unwrap()
                .is_some()
        );
        if change == "legacy" {
            return;
        }
        // Restoring the original target reuses the same checkpointed exchange.
        let original = BackendManager::new(&Some(vec![config.clone()]), secrets.clone());
        runtime::resume_with_recorder(
            Some("test-model"),
            vec![RuntimeMessage::User("question".into())],
            &original,
            &permissive_security(),
            &ctx,
            &ToolPolicyRegistry::empty(),
            None,
            None,
            None,
            resume,
            ModelCallScope::default(),
        )
        .await
        .unwrap();
    } else {
        result.unwrap();
    }
    assert_eq!(prior.load(Ordering::SeqCst), 0);
    assert_eq!(wait.load(Ordering::SeqCst), 1);
    let requests = server.received_requests().await.unwrap();
    let payload: Value = requests[1].body_json().unwrap();
    assert_eq!(&payload["input"].as_array().unwrap()[1..6], items);
    assert_eq!(payload["input"][6]["call_id"], "call1");
    assert!(
        payload["input"][6]["output"]
            .as_str()
            .unwrap()
            .contains("already executed")
    );
    assert_eq!(payload["input"][7]["call_id"], "call2");
    assert!(
        payload["input"][7]["output"]
            .as_str()
            .unwrap()
            .contains("observed")
    );
    assert_eq!(
        payload["prompt_cache_key"],
        secrets.session_cache_key(&db_id, "legacy-agent:test-agent", &config)
    );
    let mut incomplete = records.clone();
    incomplete.pop();
    assert!(
        plan_job_wait_resume(&saved, &incomplete).is_err(),
        "never fabricate missing earlier results"
    );
}

#[tokio::test]
async fn native_pending_wait_resume_preserves_items_and_never_reexecutes_prior_effects() {
    pending_change("same").await;
}
#[tokio::test]
async fn origin_pending_model_change() {
    pending_change("model").await;
}
#[tokio::test]
async fn origin_pending_connection_change() {
    pending_change("connection").await;
}
#[tokio::test]
async fn origin_pending_endpoint_change() {
    pending_change("endpoint").await;
}
#[tokio::test]
async fn origin_pending_namespace_change() {
    pending_change("namespace").await;
}
#[tokio::test]
async fn origin_pending_legacy_missing() {
    pending_change("legacy").await;
}
#[tokio::test]
async fn origin_pending_incompatible_api_change() {
    pending_change("incompatible-api").await;
}
#[tokio::test]
async fn origin_pending_api_change() {
    pending_change("api").await;
}

#[tokio::test]
async fn native_final_replay_keeps_bridge_provenance() {
    use crate::session::EntryRouting;
    let server = MockServer::start().await;
    let config = native_backend(&server.uri());
    let client = Responses::new(&config, &empty_secrets().await);
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(final_output())))
        .expect(1)
        .mount(&server)
        .await;
    let LLMResponse::Text { metadata, .. } = client
        .chat_with_tools(&[], &[], "test-model", None)
        .await
        .unwrap()
    else {
        panic!("text response")
    };
    for delivery in ["local", "pending", "sent"] {
        let mut final_entry = entry("test-agent", "Done", metadata.clone());
        let mut entries = vec![entry("user", "question", None)];
        if delivery != "local" {
            final_entry.routing = Some(EntryRouting {
                outbound_id: Some("outbound".into()),
                ..Default::default()
            });
        }
        entries.push(final_entry);
        if delivery == "sent" {
            let mut receipt = entry("test-agent", "", None);
            receipt.entry_type = EntryType::MatrixSent;
            receipt.routing = Some(EntryRouting {
                reply_to: Some("outbound".into()),
                ..Default::default()
            });
            entries.push(receipt);
        }
        let messages = ContextBuilder::new(
            &entries,
            "test-agent",
            "Policy",
            &ContextConfig {
                max_context_tokens: 8192,
                reserved_output_tokens: 256,
            },
        )
        .with_matrix_binding("@agent:example", "!room:example")
        .build()
        .await
        .messages;
        let request = client.request(&messages, &[], "test-model", None).unwrap();
        assert_eq!(
            &request.input[1..3],
            final_output(),
            "native items stay unchanged"
        );
        let envelope: Value = request
            .input
            .iter()
            .filter(|item| item["role"] == "user")
            .filter_map(|item| item["content"][0]["text"].as_str())
            .filter_map(|text| serde_json::from_str::<Value>(text).ok())
            .find(|value| value["sender"] == "test-agent")
            .expect("native replay must retain the host-rendered bridge envelope");
        assert_eq!(envelope["body"], "Done");
        if delivery == "local" {
            assert_eq!(envelope["kind"], "local_message");
        } else {
            assert_eq!(envelope["kind"], "matrix_reply");
            assert_eq!(envelope["delivery"], delivery);
        }
    }
}

#[tokio::test]
async fn native_silence_and_tool_free_turns_keep_terminal_items_and_scoped_identity() {
    for silent in [false, true] {
        let server = MockServer::start().await;
        let mut items = final_output();
        if silent {
            items[1] = json!({"type":"function_call", "id":"silent-item", "call_id":"silent-call", "name":"no_reply", "arguments":"{}"});
        }
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response(items.clone())))
            .expect(2)
            .mount(&server)
            .await;
        let (_instance, session) = fresh_session().await;
        let request = session
            .lock()
            .await
            .add_entry(entry("user", "question", None))
            .await
            .unwrap();
        let attempt = session
            .lock()
            .await
            .start_turn_attempt(request)
            .await
            .unwrap();
        let recorder = Arc::new(Recorder {
            session: session.clone(),
            attempt: attempt.clone(),
            sequence: Mutex::new(0),
        });
        let config = native_backend(&server.uri());
        let secrets = empty_secrets().await;
        let manager = BackendManager::new(&Some(vec![config.clone()]), secrets.clone());
        let mut ctx = tool_context(session.clone(), Arc::new(ToolRegistry::new()));
        ctx.allow_no_reply = silent;
        let result = runtime::execute_with_recorder(
            Some("test-model"),
            vec![RuntimeMessage::User("question".into())],
            &manager,
            &permissive_security(),
            &ctx,
            &ToolPolicyRegistry::empty(),
            Some(recorder),
            None,
            None,
            ModelCallScope::default(),
        )
        .await
        .unwrap();
        let final_entry = if silent {
            None
        } else {
            Some(entry("test-agent", &result.body, result.metadata))
        };
        session
            .lock()
            .await
            .complete_turn_attempt(&attempt, final_entry)
            .await
            .unwrap();
        let messages = context(&*session.lock().await).await;
        let client = Responses::new(&config, &secrets);
        let replay = client.request(&messages, &[], "test-model", None).unwrap();
        assert_eq!(&replay.input[1..3], items);
        if silent {
            assert_eq!(
                replay.input[3],
                json!({"type":"function_call_output", "call_id":"silent-call", "output":""})
            );
        } else {
            assert_eq!(replay.input.len(), 3);
        }
        let requests = server.received_requests().await.unwrap();
        let payload: Value = requests[0].body_json().unwrap();
        let db_id = session.lock().await.database().root_id().to_string();
        assert_eq!(
            payload["prompt_cache_key"],
            secrets.session_cache_key(&db_id, "legacy-agent:test-agent", &config)
        );
        ctx.agent_name = "other-agent".into();
        runtime::execute(
            Some("test-model"),
            vec![RuntimeMessage::User("question".into())],
            &manager,
            &permissive_security(),
            &ctx,
            &ToolPolicyRegistry::empty(),
            None,
            None,
        )
        .await
        .unwrap();
        let requests = server.received_requests().await.unwrap();
        let other: Value = requests[1].body_json().unwrap();
        assert_ne!(payload["prompt_cache_key"], other["prompt_cache_key"]);
        assert_eq!(
            other["prompt_cache_key"],
            secrets.session_cache_key(&db_id, "legacy-agent:other-agent", &config)
        );
    }
}
