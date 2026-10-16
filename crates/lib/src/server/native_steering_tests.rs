//! Loopback-only compatibility of steering, native replay and exact wait recovery.
use super::*;
use crate::{
    config::{Backend, BackendType, Model, ModelApi},
    responses::{OUTPUT_ITEMS, REPLAY_ORIGIN},
    runtime::{ModelCallScope, RuntimeMessage},
    session::steering::{JobInputRequest, JobInputState},
    test_support::{permissive_security, tool_context},
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
};

// Same HTTP framing as the TUI steering fixture, with explicit response barriers
// instead of timed replies. Dropping a failed test aborts the owned listener.
struct Wire {
    base: String,
    requests: mpsc::Receiver<(Value, oneshot::Sender<Value>)>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Wire {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Wire {
    async fn new(api: ModelApi) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let body = tokio::time::timeout(Duration::from_secs(30), async {
                    loop {
                        let mut buf = [0; 4096];
                        let n = stream.read(&mut buf).await.unwrap();
                        assert!(n > 0);
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers =
                                std::str::from_utf8(&bytes[..end]).unwrap().to_lowercase();
                            if headers.starts_with("get /models ") {
                                break Value::Null;
                            }
                            let route = if api == ModelApi::Responses {
                                "/responses"
                            } else {
                                "/chat/completions"
                            };
                            assert!(headers.starts_with(&format!("post {route} ")));
                            let length: usize = headers
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length: "))
                                .unwrap()
                                .parse()
                                .unwrap();
                            if bytes.len() >= end + 4 + length {
                                break serde_json::from_slice::<Value>(
                                    &bytes[end + 4..end + 4 + length],
                                )
                                .unwrap();
                            }
                        }
                    }
                })
                .await
                .unwrap();
                if body.is_null() {
                    let catalog =
                        json!({"data":[{"id":"stub","context_length":32768}]}).to_string();
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{catalog}", catalog.len()).as_bytes()).await.unwrap();
                    continue;
                }
                let (reply, receive) = oneshot::channel::<Value>();
                if tx.send((body, reply)).await.is_err() {
                    break;
                }
                let body = tokio::time::timeout(Duration::from_secs(30), receive)
                    .await
                    .unwrap()
                    .unwrap()
                    .to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        Self {
            base,
            requests,
            task,
        }
    }
    async fn next(&mut self) -> (Value, oneshot::Sender<Value>) {
        tokio::time::timeout(Duration::from_secs(30), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
    fn backend(&self, api: ModelApi, secrets: crate::security::SecretStore) -> BackendManager {
        let mut b = Backend::new(BackendType::OpenAICompatible);
        b.api_base = Some(self.base.clone());
        b.api_key = Some("test-only-token".into());
        b.max_retries = Some(0);
        b.models = Some(vec![Model {
            name: "stub".into(),
            api: Some(api),
            reasoning: None,
            price_input: None,
            price_output: None,
            price_cache_read: None,
            context_window: None,
        }]);
        BackendManager::new(&Some(vec![b]), secrets)
    }
}
fn text_items(id: &str, text: &str) -> Vec<Value> {
    vec![
        json!({"type":"reasoning", "id":format!("r-{id}"), "encrypted_content":format!("opaque-{id}"), "summary":[]}),
        json!({"type":"message", "role":"assistant", "id":id, "status":"completed", "content":[{"type":"output_text", "text":text, "annotations":[]}]}),
    ]
}
fn response(api: ModelApi, items: &[Value]) -> Value {
    if api == ModelApi::Responses {
        json!({"status":"completed", "model":"returned-alias", "output":items, "usage":{"input_tokens":100,"output_tokens":12}})
    } else {
        let text: String = items
            .iter()
            .filter_map(|i| i["content"][0]["text"].as_str())
            .collect();
        let calls: Vec<_> = items.iter().filter(|i| i["type"] == "function_call").map(|i| json!({"id":i["call_id"], "type":"function", "function":{"name":i["name"], "arguments":i["arguments"]}})).collect();
        let mut message = json!({"role":"assistant", "content":text});
        if !calls.is_empty() {
            message["tool_calls"] = calls.into();
        }
        json!({"model":"stub", "choices":[{"message":message}]})
    }
}
fn input(body: &Value, api: ModelApi) -> &[Value] {
    body[if api == ModelApi::Responses {
        "input"
    } else {
        "messages"
    }]
    .as_array()
    .unwrap()
}
fn user_text(item: &Value, api: ModelApi) -> Option<&str> {
    (item["role"] == "user")
        .then(|| {
            if api == ModelApi::Responses {
                item["content"][0]["text"].as_str()
            } else {
                item["content"].as_str()
            }
        })
        .flatten()
}
async fn publish(session: &Session, attempt: &str, id: &str, text: &str) {
    session
        .submit_turn_input(JobInputRequest {
            id: id.into(),
            attempt_id: attempt.into(),
            text: text.into(),
        })
        .await
        .unwrap();
}

async fn blocked_model_and_batch(api: ModelApi) {
    let mut wire = Wire::new(api).await;
    let (_instance, server, registry) = server_fixture().await;
    let effects = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    server.tools.register(SteeringEffect {
        calls: effects.clone(),
        started: started.clone(),
        release: release.clone(),
    });
    let (sid, db) = registry.create_session(Some("tui")).await.unwrap();
    let manager = wire.backend(
        api,
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    server
        .register_session(&db, manager, Some("agent".into()), None)
        .await
        .unwrap();
    write_user_message_with_content(&db, &sid.0, "original").await;
    let (first, reply) = wire.next().await;
    let attempt = Session::steering_attempts(&db)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let session = Session::new(sid.clone(), db.clone()).await;
    publish(
        &session,
        &attempt,
        "ffffffff-ffff-4fff-8fff-ffffffffffff",
        "during model",
    )
    .await;
    let mut items = text_items("plan", "tool plan");
    items.extend(["a","b"].map(|id| json!({"type":"function_call", "id":format!("item-{id}"), "call_id":id, "name":"steering_effect", "arguments":"{}"})));
    reply.send(response(api, &items)).unwrap();
    tokio::time::timeout(Duration::from_secs(30), started.notified())
        .await
        .unwrap();
    publish(
        &session,
        &attempt,
        "00000000-0000-4000-8000-000000000000",
        "during tools",
    )
    .await;
    assert!(
        session
            .turn_inputs()
            .await
            .unwrap()
            .iter()
            .all(|i| !i.incorporated)
    );
    assert!(
        input(&first, api)
            .iter()
            .all(|i| user_text(i, api) != Some("during model"))
    );
    assert!(
        wire.requests.try_recv().is_err(),
        "no backend call within the native batch"
    );
    release.notify_one();
    let (second, reply) = wire.next().await;
    assert_eq!(effects.load(Ordering::SeqCst), 2);
    let next = input(&second, api);
    assert_eq!(
        next.iter()
            .filter_map(|i| user_text(i, api))
            .filter(|t| t.starts_with("during"))
            .collect::<Vec<_>>(),
        ["during model", "during tools"]
    );
    let results: Vec<_> = next
        .iter()
        .filter(|i| i["type"] == "function_call_output" || i["role"] == "tool")
        .collect();
    assert_eq!(results.len(), 2);
    if api == ModelApi::Responses {
        assert_eq!(&next[2..6], items);
        assert_eq!(results[0]["call_id"], "a");
        assert_eq!(results[1]["call_id"], "b");
    }
    assert!(
        session
            .turn_inputs()
            .await
            .unwrap()
            .iter()
            .all(|i| i.incorporated
                && matches!(i.state, JobInputState::Dispatching { model_sequence: 1 }))
    );
    // A tool-free closing response is also a safe boundary, not a new turn.
    publish(
        &session,
        &attempt,
        "11111111-1111-4111-8111-111111111111",
        "after provisional",
    )
    .await;
    let provisional = text_items("provisional", "provisional");
    reply.send(response(api, &provisional)).unwrap();
    let (third, reply) = wire.next().await;
    if api == ModelApi::Responses {
        assert_eq!(&input(&third, api)[10..12], provisional);
    }
    assert_eq!(
        user_text(input(&third, api).last().unwrap(), api),
        Some("after provisional")
    );
    let final_items = text_items("final", "finished");
    reply.send(response(api, &final_items)).unwrap();
    await_no_processing(&server, &sid.0).await;
    let reopened = Session::new(sid.clone(), db.clone()).await;
    assert_eq!(reopened.attempts_for_test().await.len(), 1);
    assert_eq!(effects.load(Ordering::SeqCst), 2);
    let records = reopened.turn_transcript(&attempt).await.unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r.message, TurnTranscriptMessage::Steering { .. }))
            .count(),
        2
    );
    write_user_message_with_content(&db, &sid.0, "next turn").await;
    let (later, reply) = wire.next().await;
    if api == ModelApi::Responses {
        let retained = input(&later, api);
        assert_eq!(&retained[2..6], items);
        assert_eq!(&retained[10..12], provisional);
        assert_eq!(&retained[13..15], final_items);
        assert_eq!(later["prompt_cache_key"], first["prompt_cache_key"]);
        assert!(
            !later.to_string().contains(REPLAY_ORIGIN),
            "origin is host-only"
        );
    }
    assert_eq!(
        input(&later, api)
            .iter()
            .filter_map(|i| user_text(i, api))
            .filter(|t| *t == "during model")
            .count(),
        1
    );
    reply
        .send(response(api, &text_items("later", "later")))
        .unwrap();
    await_no_processing(&server, &sid.0).await;
    assert_eq!(
        effects.load(Ordering::SeqCst),
        2,
        "no effects replay on later turn"
    );
    server.shutdown().await;
}
#[tokio::test]
async fn chat_completions_steering_emitted_fifo_after_full_batch() {
    blocked_model_and_batch(ModelApi::ChatCompletions).await;
}
#[tokio::test]
async fn responses_steering_keeps_native_envelopes_across_next_turn() {
    blocked_model_and_batch(ModelApi::Responses).await;
}

fn recorder(
    session: Arc<Mutex<Session>>,
    attempt: &TurnAttempt,
    snapshot: eidetica::Snapshot,
    sequence: u32,
) -> Arc<SessionRuntimeRecorder> {
    Arc::new(SessionRuntimeRecorder {
        session,
        request_id: attempt.request_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        sequence: AtomicU32::new(sequence),
        terminal: Default::default(),
        explicit_bridge_send: AtomicBool::new(false),
        agent_name: "test-agent".into(),
        requested_model: "stub".into(),
        context_snapshot: Some(snapshot),
        branch: None,
        fail_after: Arc::new(std::sync::Mutex::new(None)),
    })
}
struct Wait {
    calls: Arc<AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl crate::tool::Tool for Wait {
    fn descriptor(&self) -> crate::tool::ToolDescriptor {
        crate::tool::ToolDescriptor {
            name: "job_wait".into(),
            description: "offline observation".into(),
            parameters: json!({"type":"object","properties":{"session_db_id":{"type":"string"}}}),
        }
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok("observed".into())
        })
    }
}
#[tokio::test]
async fn responses_steering_exact_wait_reopen_keeps_text_origin_and_no_effect_replay() {
    let mut wire = Wire::new(ModelApi::Responses).await;
    let (_instance, session) = crate::test_support::fresh_session().await;
    let id = session
        .lock()
        .await
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "question".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let attempt = session.lock().await.start_turn_attempt(id).await.unwrap();
    let db = session.lock().await.database().clone();
    let snapshot = db.snapshot().await.unwrap();
    let manager = wire.backend(
        ModelApi::Responses,
        crate::test_support::empty_secrets().await,
    );
    let effects = Arc::new(AtomicUsize::new(0));
    let effect_started = Arc::new(tokio::sync::Notify::new());
    let effect_release = Arc::new(tokio::sync::Notify::new());
    let waits = Arc::new(AtomicUsize::new(0));
    let wait_started = Arc::new(tokio::sync::Notify::new());
    let wait_release = Arc::new(tokio::sync::Notify::new());
    let tools = Arc::new(ToolRegistry::new());
    tools.register(SteeringEffect {
        calls: effects.clone(),
        started: effect_started.clone(),
        release: effect_release.clone(),
    });
    tools.register(Wait {
        calls: waits.clone(),
        started: wait_started.clone(),
        release: wait_release.clone(),
    });
    let ctx = tool_context(session.clone(), tools.clone());
    let record = recorder(session.clone(), &attempt, snapshot.clone(), 0);
    let task = tokio::spawn({
        let manager = manager.clone();
        let ctx = ctx.clone();
        let record = record.clone();
        async move {
            runtime::execute_with_recorder(
                Some("stub"),
                vec![RuntimeMessage::User("question".into())],
                &manager,
                &permissive_security(),
                &ctx,
                &ToolPolicyRegistry::empty(),
                Some(record),
                None,
                None,
                ModelCallScope::default(),
            )
            .await
        }
    });
    let (_, reply) = wire.next().await;
    publish(
        &*session.lock().await,
        &attempt.attempt_id,
        "ffffffff-ffff-4fff-8fff-ffffffffffff",
        "before wait",
    )
    .await;
    let prior = text_items("prior", "provisional before wait");
    reply.send(response(ModelApi::Responses, &prior)).unwrap();
    let (_, reply) = wire.next().await;
    let arguments = json!({"session_db_id":"child","timeout_seconds":240}).to_string();
    let batch = vec![
        json!({"type":"reasoning","id":"rb","encrypted_content":"opaque-batch","summary":[]}),
        json!({"type":"function_call","id":"effect-item","call_id":"effect-call","name":"steering_effect","arguments":"{}"}),
        json!({"type":"function_call","id":"wait-item","call_id":"wait-call","name":"job_wait","arguments":arguments}),
    ];
    reply.send(response(ModelApi::Responses, &batch)).unwrap();
    tokio::time::timeout(Duration::from_secs(30), effect_started.notified())
        .await
        .unwrap();
    effect_release.notify_one();
    tokio::time::timeout(Duration::from_secs(30), wait_started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let conversation = session.lock().await.conversation_id.clone();
    let reopened = Session::new(conversation, db.clone()).await;
    let checkpoint = reopened
        .latest_job_wait_checkpoint(&attempt.attempt_id)
        .await
        .unwrap()
        .unwrap();
    let records = reopened.turn_transcript(&attempt.attempt_id).await.unwrap();
    let (resume, sequence) =
        crate::session::wait_resume::plan_job_wait_resume(&checkpoint, &records).unwrap();
    let reopened = Arc::new(Mutex::new(reopened));
    let record = recorder(reopened.clone(), &attempt, snapshot, sequence as u32);
    let ctx = tool_context(reopened.clone(), tools);
    let resumed = tokio::spawn({
        let record = record.clone();
        async move {
            runtime::resume_with_recorder(
                Some("stub"),
                vec![RuntimeMessage::User("question".into())],
                &manager,
                &permissive_security(),
                &ctx,
                &ToolPolicyRegistry::empty(),
                Some(record),
                None,
                None,
                resume,
                ModelCallScope::default(),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(30), wait_started.notified())
        .await
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    publish(
        &*reopened.lock().await,
        &attempt.attempt_id,
        "00000000-0000-4000-8000-000000000000",
        "after wait",
    )
    .await;
    wait_release.notify_one();
    let (body, reply) = wire.next().await;
    let items = input(&body, ModelApi::Responses);
    assert_eq!(
        &items[1..3],
        prior,
        "restored text must keep raw output and original origin"
    );
    assert_eq!(
        user_text(&items[3], ModelApi::Responses),
        Some("before wait")
    );
    assert_eq!(&items[4..7], batch);
    assert_eq!(items[7]["call_id"], "effect-call");
    assert_eq!(items[8]["call_id"], "wait-call");
    assert_eq!(
        user_text(&items[9], ModelApi::Responses),
        Some("after wait")
    );
    reply
        .send(response(ModelApi::Responses, &text_items("final", "done")))
        .unwrap();
    assert_eq!(resumed.await.unwrap().unwrap().body, "done");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        waits.load(Ordering::SeqCst),
        2,
        "one interrupted observation, one resume"
    );
    let saved = reopened
        .lock()
        .await
        .latest_job_wait_checkpoint(&attempt.attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert!(saved.observation_finished);
    let restored = reopened
        .lock()
        .await
        .turn_transcript(&attempt.attempt_id)
        .await
        .unwrap();
    assert!(
        matches!(restored[0].message,TurnTranscriptMessage::ModelResponse { ref provider_extra,.. } if provider_extra.contains_key(REPLAY_ORIGIN))
    );
    assert_eq!(restored.iter().filter(|r| matches!(&r.message,TurnTranscriptMessage::ToolResult {name,..} if name == "steering_effect")).count(),1);
}

#[tokio::test]
async fn responses_origin_refusal_precedes_steering_promotion_and_telemetry() {
    let wire = Wire::new(ModelApi::Responses).await;
    let (_instance, session) = crate::test_support::fresh_session().await;
    let id = session
        .lock()
        .await
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "question".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let attempt = session.lock().await.start_turn_attempt(id).await.unwrap();
    let db = session.lock().await.database().clone();
    publish(
        &*session.lock().await,
        &attempt.attempt_id,
        "ffffffff-ffff-4fff-8fff-ffffffffffff",
        "not sent",
    )
    .await;
    let record = recorder(session.clone(), &attempt, db.snapshot().await.unwrap(), 0);
    let manager = wire.backend(
        ModelApi::Responses,
        crate::test_support::empty_secrets().await,
    );
    let initial = vec![
        RuntimeMessage::User("prior".into()),
        RuntimeMessage::AssistantWithExtra {
            exchange_id: None,
            content: Some("visible".into()),
            provider_extra: serde_json::Map::from_iter([(
                OUTPUT_ITEMS.into(),
                json!([{"type":"future_action","id":"unknown"}]),
            )]),
        },
    ];
    let result = runtime::execute_with_recorder(
        Some("stub"),
        initial,
        &manager,
        &permissive_security(),
        &tool_context(session.clone(), Arc::new(ToolRegistry::new())),
        &ToolPolicyRegistry::empty(),
        Some(record),
        None,
        None,
        ModelCallScope::default(),
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .contains("Cannot convert Responses history")
    );
    assert!(
        session
            .lock()
            .await
            .turn_inputs()
            .await
            .unwrap()
            .iter()
            .all(|i| !i.incorporated),
        "invalid origin/output cannot promote unsent input"
    );
    let txn = db.new_transaction().await.unwrap();
    assert!(
        txn.get_store::<eidetica::store::Table<Value>>("call_telemetry")
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap()
            .is_empty(),
        "invalid native replay creates no telemetry"
    );
    assert!(
        session
            .lock()
            .await
            .turn_transcript(&attempt.attempt_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn responses_steering_silence_preserves_native_call_and_empty_output() {
    let mut wire = Wire::new(ModelApi::Responses).await;
    let (_instance, session) = crate::test_support::fresh_session().await;
    let id = session
        .lock()
        .await
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "question".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let attempt = session.lock().await.start_turn_attempt(id).await.unwrap();
    let db = session.lock().await.database().clone();
    let record = recorder(session.clone(), &attempt, db.snapshot().await.unwrap(), 0);
    let manager = wire.backend(
        ModelApi::Responses,
        crate::test_support::empty_secrets().await,
    );
    let mut ctx = tool_context(session.clone(), Arc::new(ToolRegistry::new()));
    ctx.allow_no_reply = true;
    let task = tokio::spawn({
        let manager = manager.clone();
        async move {
            runtime::execute_with_recorder(
                Some("stub"),
                vec![RuntimeMessage::User("question".into())],
                &manager,
                &permissive_security(),
                &ctx,
                &ToolPolicyRegistry::empty(),
                Some(record),
                None,
                None,
                ModelCallScope::default(),
            )
            .await
        }
    });
    let (_, reply) = wire.next().await;
    publish(
        &*session.lock().await,
        &attempt.attempt_id,
        "ffffffff-ffff-4fff-8fff-ffffffffffff",
        "reply instead",
    )
    .await;
    let items = vec![
        json!({"type":"reasoning","id":"silent-reasoning","summary":[],"encrypted_content":"opaque-silent"}),
        json!({"type":"function_call","id":"silent-item","call_id":"silent-call","name":"no_reply","arguments":"{}"}),
    ];
    reply.send(response(ModelApi::Responses, &items)).unwrap();
    let (body, reply) = wire.next().await;
    let sent = input(&body, ModelApi::Responses);
    assert_eq!(
        &sent[1..3],
        items,
        "native silence is not replaceable by lossy text"
    );
    assert_eq!(
        sent[3],
        json!({"type":"function_call_output","call_id":"silent-call","output":""})
    );
    assert_eq!(
        user_text(&sent[4], ModelApi::Responses),
        Some("reply instead")
    );
    reply
        .send(response(
            ModelApi::Responses,
            &text_items("final", "replied"),
        ))
        .unwrap();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.body, "replied");
    session
        .lock()
        .await
        .complete_turn_attempt(
            &attempt,
            Some(SessionEntry {
                sender: "test-agent".into(),
                content: result.body,
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: result.metadata,
                routing: None,
            }),
        )
        .await
        .unwrap();
    let reopened = Session::new(session.lock().await.conversation_id.clone(), db).await;
    let (entries, history) = reopened.context_with_tool_history().await.unwrap();
    let context = crate::context::ContextBuilder::new(
        &entries,
        "test-agent",
        "policy",
        &crate::config::ContextConfig {
            max_context_tokens: 8192,
            reserved_output_tokens: 256,
        },
    )
    .with_tool_history(&history)
    .build()
    .await;
    let later = tokio::spawn(async move {
        manager
            .chat_with_tools_for_model(Some("stub"), &context.messages, &[], "stub")
            .await
    });
    let (body, reply) = wire.next().await;
    let sent = input(&body, ModelApi::Responses);
    assert_eq!(&sent[2..4], items);
    assert_eq!(
        sent[4],
        json!({"type":"function_call_output","call_id":"silent-call","output":""})
    );
    assert_eq!(
        user_text(&sent[5], ModelApi::Responses),
        Some("reply instead")
    );
    reply
        .send(response(ModelApi::Responses, &text_items("later", "later")))
        .unwrap();
    later.await.unwrap().unwrap();
    let records = session
        .lock()
        .await
        .turn_transcript(&attempt.attempt_id)
        .await
        .unwrap();
    assert!(
        matches!(&records[0].message,TurnTranscriptMessage::ModelResponse {tool_calls,provider_extra,terminal:false,..} if tool_calls.len() == 1 && provider_extra[OUTPUT_ITEMS] == json!(items))
    );
    assert!(
        matches!(&records[1].message,TurnTranscriptMessage::ToolResult {call_id,output,..} if call_id == "silent-call" && output.is_empty())
    );
}

#[tokio::test]
async fn responses_steering_cannot_move_before_native_assistant_output() {
    let (_instance, session) = crate::test_support::fresh_session().await;
    let id = session
        .lock()
        .await
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "question".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let attempt = session.lock().await.start_turn_attempt(id).await.unwrap();
    let db = session.lock().await.database().clone();
    publish(
        &*session.lock().await,
        &attempt.attempt_id,
        "ffffffff-ffff-4fff-8fff-ffffffffffff",
        "must follow output",
    )
    .await;
    crate::session::steering::input_boundary(&db, &attempt.attempt_id, 1, false)
        .await
        .unwrap();
    let assistant = RuntimeMessage::AssistantWithExtra {
        exchange_id: None,
        content: Some("provisional".into()),
        provider_extra: serde_json::Map::from_iter([(
            OUTPUT_ITEMS.into(),
            json!(text_items("prior", "provisional")),
        )]),
    };
    let user = RuntimeMessage::User("must follow output".into());
    let baseline = vec![assistant.clone(), user.clone()];
    let reordered = vec![user, assistant];
    assert!(
        crate::session::steering::dispatch_inputs(
            &db,
            &attempt.attempt_id,
            1,
            &reordered,
            &baseline
        )
        .await
        .is_err(),
        "native assistant variant cannot hide reordered steering"
    );
    assert!(
        session
            .lock()
            .await
            .turn_inputs()
            .await
            .unwrap()
            .iter()
            .all(|i| !i.incorporated)
    );
}
