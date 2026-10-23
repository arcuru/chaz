//! Native-loop autocompaction tests. Every model call goes to a scripted
//! stub; no provider or CLI is reached. Token counts are Chaz's local
//! estimates, not provider-reported usage.
use super::autocompact::{SUMMARY_PREFIX, Status};
use super::*;
use crate::backends::BackendDispatch;
use crate::test_support::{RecordingRuntime, empty_secrets, fresh_session, tool_context};
use crate::tool::{Tool, ToolRegistry};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Stub {
    main: StdMutex<VecDeque<Result<LLMResponse, LlmError>>>,
    main_calls: StdMutex<Vec<Vec<RuntimeMessage>>>,
    main_tools: StdMutex<Vec<ToolDefinition>>,
    summaries: StdMutex<VecDeque<Result<LLMResponse, LlmError>>>,
    summary_calls: StdMutex<Vec<(Vec<RuntimeMessage>, u32)>>,
    /// Hold summary responses until this many main calls were recorded.
    release_after_main: StdMutex<Option<usize>>,
    released: tokio::sync::Notify,
    /// Latency of each summary response.
    summary_delay: StdMutex<Duration>,
    main_delay: StdMutex<Duration>,
}

impl Stub {
    fn tool_call(&self, id: &str) {
        self.main
            .lock()
            .unwrap()
            .push_back(Ok(LLMResponse::ToolCalls {
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: id.into(),
                    name: "work".into(),
                    arguments: format!("{{\"step\":\"{id}\"}}"),
                }],
                provider_extra: serde_json::Map::from_iter([(
                    "reasoning_details".to_string(),
                    json!([{"opaque": id}]),
                )]),
                metadata: None,
            }));
    }
    fn text(&self, body: &str) {
        self.main.lock().unwrap().push_back(Ok(LLMResponse::Text {
            content: body.into(),
            provider_extra: Default::default(),
            metadata: None,
        }));
    }
    fn summary(&self, response: Result<LLMResponse, LlmError>) {
        self.summaries.lock().unwrap().push_back(response);
    }
    fn summary_text(&self, body: &str) {
        self.summary(Ok(LLMResponse::Text {
            content: body.into(),
            provider_extra: Default::default(),
            metadata: Some(ResponseMetadata {
                model: "stub".into(),
                usage: TokenUsage {
                    prompt_tokens: 111,
                    completion_tokens: 22,
                    total_tokens: 133,
                    ..Default::default()
                },
                ..Default::default()
            }),
        }));
    }
    fn mains(&self) -> Vec<Vec<RuntimeMessage>> {
        self.main_calls.lock().unwrap().clone()
    }
    fn summary_requests(&self) -> Vec<(Vec<RuntimeMessage>, u32)> {
        self.summary_calls.lock().unwrap().clone()
    }
}

impl BackendDispatch for Stub {
    fn execute<'a>(
        &'a self,
        _: &'a crate::backends::ChatContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, LlmError>> + Send + 'a>> {
        Box::pin(async { panic!("autocompaction never uses the legacy execute path") })
    }
    fn supports_tools(&self) -> bool {
        true
    }
    fn chat_with_tools<'a>(
        &'a self,
        messages: &'a [RuntimeMessage],
        tools: &'a [ToolDefinition],
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LLMResponse, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            *self.main_tools.lock().unwrap() = tools.to_vec();
            let count = {
                let mut calls = self.main_calls.lock().unwrap();
                calls.push(messages.to_vec());
                calls.len()
            };
            if *self.release_after_main.lock().unwrap() == Some(count) {
                self.released.notify_waiters();
                *self.release_after_main.lock().unwrap() = None;
            }
            let delay = *self.main_delay.lock().unwrap();
            tokio::time::sleep(delay).await;
            self.main.lock().unwrap().pop_front().unwrap_or_else(|| {
                Err(LlmError::Configuration {
                    message: "stub: main script empty".into(),
                })
            })
        })
    }
    fn complete_bounded<'a>(
        &'a self,
        messages: &'a [RuntimeMessage],
        _: &'a str,
        max_output_tokens: u32,
    ) -> Pin<Box<dyn Future<Output = Result<LLMResponse, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            self.summary_calls
                .lock()
                .unwrap()
                .push((messages.to_vec(), max_output_tokens));
            let held = self.release_after_main.lock().unwrap().is_some();
            if held {
                self.released.notified().await;
            }
            let delay = *self.summary_delay.lock().unwrap();
            tokio::time::sleep(delay).await;
            self.summaries
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Err(LlmError::Configuration {
                        message: "stub: summary script empty".into(),
                    })
                })
        })
    }
}

/// A side-effecting tool: counts executions and yields so a background
/// preparation can make progress between model calls.
struct Work {
    calls: Arc<AtomicUsize>,
    size: usize,
}
impl Tool for Work {
    fn descriptor(&self) -> crate::tool::ToolDescriptor {
        crate::tool::ToolDescriptor {
            name: "work".into(),
            description: "counted work".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        _: &'a crate::tool::ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok(format!(
                "result {n} for {args}: {}",
                "alpha beta gamma delta ".repeat(self.size)
            ))
        })
    }
}

struct Run {
    stub: Arc<Stub>,
    calls: Arc<AtomicUsize>,
    recorder: Arc<RecordingRuntime>,
    result: Result<RuntimeOutcome, String>,
    status: Option<Status>,
}

async fn run(
    stub: Arc<Stub>,
    settings: Option<Value>,
    initial: Vec<RuntimeMessage>,
    sources: Vec<ContextSource>,
    budget: Option<usize>,
    tool_size: usize,
) -> Run {
    let (_instance, session) = fresh_session().await;
    let db = session.lock().await.database().clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(ToolRegistry::new());
    registry.register(Work {
        calls: calls.clone(),
        size: tool_size,
    });
    let mut ctx = tool_context(session, registry);
    if let Some(settings) = settings {
        ctx.active_extensions
            .insert(crate::extensions::autocompact::NAME.into());
        crate::extension::write_settings(&db, crate::extensions::autocompact::NAME, settings)
            .await
            .unwrap();
    }
    let backend = BackendManager::with_mock(stub.clone(), empty_secrets().await);
    let recorder = Arc::new(RecordingRuntime::default());
    let policies = ToolPolicyRegistry::new(std::collections::HashMap::from([(
        "work".to_string(),
        crate::tool::ToolPolicy {
            approval: crate::tool::ApprovalRequirement::Never,
            ..Default::default()
        },
    )]));
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        execute_with_recorder(
            None,
            initial,
            &backend,
            &crate::test_support::permissive_security(),
            &ctx,
            &policies,
            Some(recorder.clone()),
            None,
            None,
            ModelCallScope {
                request_budget_tokens: budget,
                sources,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("turn finished");
    let status = {
        let txn = db.new_transaction().await.unwrap();
        let store = txn
            .get_store::<eidetica::store::DocStore>("autocompact_status")
            .await
            .unwrap();
        store
            .get_string("test-agent")
            .await
            .ok()
            .map(|raw| serde_json::from_str(&raw).unwrap())
    };
    Run {
        stub,
        calls,
        recorder,
        result,
        status,
    }
}

fn on(trigger: u64, keep: u64) -> Option<Value> {
    Some(json!({"enabled": true, "trigger_tokens": trigger, "keep_recent_tokens": keep}))
}

fn prompt() -> Vec<RuntimeMessage> {
    vec![
        RuntimeMessage::System("You are the test agent.".into()),
        RuntimeMessage::User("do the task".into()),
    ]
}

fn summary_count(messages: &[RuntimeMessage]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, RuntimeMessage::User(t) if t.starts_with(SUMMARY_PREFIX)))
        .count()
}

/// Every tool result directly follows its own call group, whole and in order.
fn assert_complete_groups(messages: &[RuntimeMessage]) {
    let mut expected: VecDeque<String> = VecDeque::new();
    for message in messages {
        match message {
            RuntimeMessage::ToolResult { call_id, .. } => {
                assert_eq!(
                    expected.pop_front().as_ref(),
                    Some(call_id),
                    "orphan result"
                )
            }
            other => {
                assert!(expected.is_empty(), "incomplete group before {other:?}");
                if let RuntimeMessage::AssistantToolCalls { tool_calls, .. } = other {
                    expected = tool_calls.iter().map(|c| c.id.clone()).collect();
                }
            }
        }
    }
    assert!(expected.is_empty(), "trailing incomplete group");
}

fn tokens(messages: &[RuntimeMessage]) -> usize {
    crate::context::estimate_request_tokens(messages, &[])
}

#[tokio::test]
async fn below_threshold_and_disabled_requests_are_untouched() {
    for settings in [
        on(10_000_000, 100),
        Some(json!({"enabled": false, "trigger_tokens": 1})),
        None,
    ] {
        let stub = Arc::new(Stub::default());
        stub.tool_call("c1");
        stub.tool_call("c2");
        stub.text("done");
        let baseline = {
            let stub = Arc::new(Stub::default());
            stub.tool_call("c1");
            stub.tool_call("c2");
            stub.text("done");
            run(stub, None, prompt(), vec![], Some(1_000_000), 200).await
        };
        let r = run(
            stub,
            settings.clone(),
            prompt(),
            vec![],
            Some(1_000_000),
            200,
        )
        .await;
        assert_eq!(r.result.unwrap().body, "done");
        assert!(r.stub.summary_requests().is_empty(), "{settings:?}");
        assert_eq!(r.stub.mains(), baseline.stub.mains(), "{settings:?}");
    }
}

#[tokio::test]
async fn long_tool_loop_compacts_its_completed_prefix_in_the_same_request() {
    let stub = Arc::new(Stub::default());
    for id in ["c1", "c2", "c3", "c4"] {
        stub.tool_call(id);
    }
    stub.text("done");
    stub.summary_text("## Progress So Far\n- c1 finished");
    stub.summary_text("## Progress So Far\n- c1 and c2 finished");
    stub.summary_text("## Progress So Far\n- c1, c2 and c3 finished");
    // Preparation starts when call 2 sees the threshold; hold it while
    // call 3 proceeds unchanged, then let it finish during the next tool.
    *stub.release_after_main.lock().unwrap() = Some(3);
    let r = run(stub, on(1_500, 600), prompt(), vec![], Some(1_000_000), 300).await;
    assert_eq!(r.result.as_ref().unwrap().body, "done");
    assert!(
        r.result.as_ref().unwrap().metadata.is_none(),
        "auxiliary usage must not enter main-turn totals"
    );
    // Mutation non-replay: four calls, four executions, never repeated.
    assert_eq!(r.calls.load(Ordering::SeqCst), 4);
    let mains = r.stub.mains();
    assert_eq!(mains.len(), 5);
    let summaries = r.stub.summary_requests();
    assert!((2..=3).contains(&summaries.len()), "{}", summaries.len());
    assert_eq!(summaries[0].1, 8_192, "default output ceiling");
    // Valid work continued while the first summary was pending.
    assert_eq!(summary_count(&mains[1]), 0);
    assert_eq!(summary_count(&mains[2]), 0);
    // Applied at the next safe boundary of the same request.
    for (applied, text) in [
        (&mains[3], "- c1 finished"),
        (&mains[4], "c1 and c2 finished"),
    ] {
        assert_eq!(summary_count(applied), 1);
        assert_eq!(applied[0], prompt()[0]);
        assert_eq!(applied[1], prompt()[1], "current input stays verbatim");
        assert!(matches!(&applied[2], RuntimeMessage::User(t) if t.contains(text)));
        assert_complete_groups(applied);
    }
    let mut uncompacted = mains[2].clone();
    uncompacted.extend_from_slice(&mains[3][mains[3].len() - 2..]);
    assert!(tokens(&mains[3]) < tokens(&uncompacted));
    let source = |i: usize| match &summaries[i].0[1] {
        RuntimeMessage::User(text) => text.clone(),
        other => panic!("unexpected {other:?}"),
    };
    // The summarizer saw the covered tool bodies, not the user's request.
    let first = source(0);
    assert!(first.contains("[Assistant tool calls]: work({\"step\":\"c1\"})"));
    assert!(first.contains("[Tool result]: "));
    assert!(first.contains("result 0 for"));
    assert!(!first.contains("c2"));
    assert!(!first.contains("do the task"));
    assert!(first.contains("earlier work from the request that is still in progress"));
    // Pi update semantics: the next summary extends the previous one with
    // only the newly covered messages.
    let second = source(1);
    assert!(second.contains("<previous-summary>\n## Progress So Far\n- c1 finished"));
    assert!(second.contains("{\"step\":\"c2\"}"));
    assert!(!second.contains("{\"step\":\"c1\"}"));
    // The recent tail keeps its opaque provider data byte-identical.
    let last_call = mains[4]
        .iter()
        .rev()
        .find(|m| matches!(m, RuntimeMessage::AssistantToolCalls { .. }))
        .unwrap();
    assert!(
        matches!(last_call, RuntimeMessage::AssistantToolCalls { provider_extra, .. }
        if provider_extra["reasoning_details"][0]["opaque"] == "c4")
    );
    // The authoritative record is untouched: every result recorded once and
    // model sequences continue without restart.
    let records = r.recorder.records.lock().unwrap();
    let sequences: Vec<u64> = records
        .iter()
        .filter_map(|r| match r {
            RuntimeRecord::ModelResponse { model_sequence, .. } => Some(*model_sequence),
            _ => None,
        })
        .collect();
    assert_eq!(sequences, vec![0, 1, 2, 3, 4]);
    let results = records
        .iter()
        .filter(|r| matches!(r, RuntimeRecord::ToolResult { .. }))
        .count();
    assert_eq!(results, 4);
}

#[tokio::test]
async fn hard_budget_waits_for_the_pending_summary() {
    let stub = Arc::new(Stub::default());
    stub.tool_call("c1");
    stub.tool_call("c2");
    stub.text("done");
    stub.summary_text("checkpoint: c1 done");
    // Preparation starts at the same boundary that is over the hard limit,
    // and is slow: call 2 must wait for it.
    *stub.summary_delay.lock().unwrap() = Duration::from_millis(300);
    let budget = 850;
    let r = run(stub, on(800, 400), prompt(), vec![], Some(budget), 100).await;
    assert_eq!(r.result.as_ref().unwrap().body, "done");
    let mains = r.stub.mains();
    for request in &mains {
        let tools = r.stub.main_tools.lock().unwrap().clone();
        assert_eq!(tools.len(), 1);
        assert!(
            crate::context::estimate_request_tokens(request, &tools) <= budget,
            "a request over the hard budget was sent"
        );
    }
    assert_eq!(r.stub.summary_requests().len(), 1);
    assert_eq!(mains.len(), 3);
    assert_eq!(summary_count(&mains[1]), 0);
    assert_eq!(summary_count(&mains[2]), 1, "applied after waiting");
    assert_eq!(r.calls.load(Ordering::SeqCst), 2);
    let status = r.status.unwrap();
    assert_eq!(status.state, "applied");
    assert_eq!(status.summary_usage[0].usage.prompt_tokens, 111);
    assert!(status.estimated_after < status.estimated_before);
}

#[tokio::test]
async fn history_summary_keeps_current_and_queued_input() {
    let mut initial = vec![RuntimeMessage::System("sys".into())];
    let mut sources = vec![ContextSource::Instructions];
    for i in 0..6 {
        initial.push(RuntimeMessage::User(format!(
            "old question {i} {}",
            "lorem ipsum ".repeat(120)
        )));
        initial.push(RuntimeMessage::Assistant(format!(
            "old answer {i} {}",
            "dolor sit ".repeat(120)
        )));
        sources.push(ContextSource::PersistedSessionEntry {
            id: format!("q{i}"),
        });
        sources.push(ContextSource::PersistedSessionEntry {
            id: format!("a{i}"),
        });
    }
    // Earlier queued request, then the current one, then an extension tail.
    initial.push(RuntimeMessage::User("queued: also check the logs".into()));
    initial.push(RuntimeMessage::User("current: fix the build".into()));
    initial.push(RuntimeMessage::User("[tail] context report".into()));
    sources.push(ContextSource::PersistedSessionEntry { id: "q6".into() });
    sources.push(ContextSource::PersistedSessionEntry { id: "q7".into() });
    sources.push(ContextSource::ContextTail);
    let stub = Arc::new(Stub::default());
    stub.text("done");
    stub.summary_text("## Goal\n- earlier questions");
    let total = tokens(&initial);
    let r = run(
        stub,
        on(1_000, 300),
        initial.clone(),
        sources,
        Some(total - 200),
        10,
    )
    .await;
    assert_eq!(r.result.as_ref().unwrap().body, "done");
    let mains = r.stub.mains();
    assert_eq!(mains.len(), 1);
    let sent = &mains[0];
    assert_eq!(summary_count(sent), 1);
    for kept in &initial[initial.len() - 3..] {
        assert!(sent.contains(kept), "lost {kept:?}");
    }
    assert_eq!(sent[0], initial[0]);
    // The history prompt (not the request-prefix one) summarized old turns.
    let summaries = r.stub.summary_requests();
    let RuntimeMessage::User(source) = &summaries[0].0[1] else {
        panic!()
    };
    assert!(source.contains("old question 0"));
    assert!(!source.contains("current: fix the build"));
    assert!(!source.contains("queued: also check"));
    assert!(source.contains("## Goal"));
}

#[tokio::test]
async fn protected_input_that_cannot_fit_fails_visibly() {
    let stub = Arc::new(Stub::default());
    stub.text("never sent");
    let initial = vec![
        RuntimeMessage::System("sys".into()),
        RuntimeMessage::User("huge ".repeat(4_000)),
    ];
    let r = run(stub, on(500, 100), initial, vec![], Some(1_000), 10).await;
    let error = r.result.unwrap_err();
    assert!(error.contains("over the 1000-token budget"), "{error}");
    assert!(r.stub.mains().is_empty(), "nothing over budget was sent");
    assert!(r.stub.summary_requests().is_empty());
    assert_eq!(r.status.unwrap().state, "over-budget");
}

#[tokio::test]
async fn failed_truncated_or_tool_requesting_summaries_are_rejected_without_retry() {
    let failures: Vec<Result<LLMResponse, LlmError>> = vec![
        Err(LlmError::TruncatedOutput {
            reason: "length".into(),
        }),
        Ok(LLMResponse::Text {
            content: "  ".into(),
            provider_extra: Default::default(),
            metadata: None,
        }),
        Ok(LLMResponse::ToolCalls {
            content: None,
            tool_calls: vec![ToolCallRequest {
                id: "x".into(),
                name: "work".into(),
                arguments: "{}".into(),
            }],
            provider_extra: Default::default(),
            metadata: None,
        }),
        Err(LlmError::Timeout),
    ];
    for failure in failures {
        let stub = Arc::new(Stub::default());
        stub.tool_call("c1");
        stub.tool_call("c2");
        stub.tool_call("c3");
        stub.text("done");
        stub.summary(failure);
        let r = run(stub, on(800, 400), prompt(), vec![], Some(1_300), 100).await;
        let error = r.result.unwrap_err();
        assert!(error.contains("over the 1300-token budget"), "{error}");
        assert!(error.contains("the last summary was rejected"), "{error}");
        assert_eq!(r.stub.summary_requests().len(), 1, "zero automatic retries");
        assert!(r.stub.mains().iter().all(|m| summary_count(m) == 0));
        // The tool ran once per dispatched call; nothing was replayed.
        assert_eq!(r.calls.load(Ordering::SeqCst), r.stub.mains().len());
        assert_eq!(r.status.unwrap().state, "over-budget");
    }
}

#[tokio::test]
async fn non_shrinking_summary_is_not_applied() {
    let stub = Arc::new(Stub::default());
    stub.tool_call("c1");
    stub.tool_call("c2");
    stub.tool_call("c3");
    stub.text("done");
    stub.summary_text(&"bloated ".repeat(5_000));
    let r = run(stub, on(1_000, 400), prompt(), vec![], Some(1_000_000), 300).await;
    assert_eq!(r.result.as_ref().unwrap().body, "done");
    assert!(r.stub.mains().iter().all(|m| summary_count(m) == 0));
    assert_eq!(r.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn completed_but_unused_autocompact_usage_is_recorded_on_turn_exit() {
    let stub = Arc::new(Stub::default());
    stub.text("done");
    stub.summary_text("checkpoint");
    *stub.main_delay.lock().unwrap() = Duration::from_millis(80);
    let mut initial = prompt();
    initial.insert(1, RuntimeMessage::User("old question ".repeat(120)));
    initial.insert(2, RuntimeMessage::Assistant("old answer ".repeat(120)));
    let r = run(stub, on(300, 50), initial, vec![], Some(1000000), 10).await;
    assert_eq!(r.result.as_ref().unwrap().body, "done");
    assert_eq!(r.stub.summary_requests().len(), 1);
    let status = r.status.unwrap();
    assert_eq!(status.state, "unused");
    assert_eq!(status.summary_usage.len(), 1);
    assert_eq!(status.summary_usage[0].usage.prompt_tokens, 111);
    assert!(r.result.unwrap().metadata.is_none());
}
