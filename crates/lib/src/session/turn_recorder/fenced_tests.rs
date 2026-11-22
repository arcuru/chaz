//! Use the public runtime with the real Chat Completions serializer, one
//! loopback backend, a pure hook and an in-process tool. No external transport
//! or production writer can be selected by this fixture.
use super::*;
use crate::{
    backends::BackendManager,
    commands::{Command, CommandContext, CommandOutcome, command_fixture, dispatch},
    config::{Backend, BackendType, ContextConfig},
    context::ContextBuilder,
    extension::{ExtensionHub, counting_ext},
    runtime::{ModelCallScope, RuntimeOutcome, execute_with_recorder},
    security::SecretStore,
    test_support::{MockHost, context_hub, permissive_security, tool_context_with_host},
    tool::{
        ApprovalRequirement, Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy,
        ToolPolicyRegistry, ToolRegistry,
    },
};
use eidetica::{
    Instance,
    store::{DocStore, Table},
};
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const AGENT: &str = "test-agent";
const MODEL: &str = "fenced-fixture";
const BRANCH: &str = "fixture-execution";
const TOOL: &str = "fake_add";
const BODY: &str = "Four.";

struct Add(Arc<AtomicUsize>);
impl Tool for Add {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: TOOL.into(),
            description: "Add two fixture numbers".into(),
            parameters: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}),
        }
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        assert_eq!(args, json!({"a": 2, "b": 2}));
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("4".into()) })
    }
}

fn stub_response(tools: bool, request: &Request) -> ResponseTemplate {
    let body: Value = request.body_json().unwrap();
    let continued = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|msg| msg["role"] == "tool");
    let (message, finish) = if tools && !continued {
        (
            json!({"role":"assistant", "content":null, "reasoning_content":"fixture reasoning", "tool_calls":[{
                "id":"wire-1", "type":"function", "function":{"name":TOOL, "arguments":"{\"a\":2,\"b\":2}"}
            }]}),
            "tool_calls",
        )
    } else {
        (json!({"role":"assistant", "content":BODY}), "stop")
    };
    ResponseTemplate::new(200).set_body_json(json!({
        "id":"fake-response", "model":MODEL, "choices":[{"index":0,"message":message,"finish_reason":finish}],
        "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}
    }))
}

struct Fixture {
    instance: Instance,
    session: Arc<Mutex<Session>>,
    db: Database,
    recorder: Arc<TurnRecorder>,
    backend: BackendManager,
    mock: MockServer,
    host: Arc<MockHost>,
    tool: Arc<AtomicUsize>,
    hook: Arc<AtomicUsize>,
    hub: ExtensionHub,
    context: ToolContext,
    policies: ToolPolicyRegistry,
    input: Snapshot,
}

impl Fixture {
    async fn new(with_tool: bool, fail: Option<fenced::FailCommit>) -> Self {
        let (instance, _account, db) = crate::session::test_helpers::test_session_db().await;
        let session = Arc::new(Mutex::new(
            Session::try_new(crate::types::ConversationId("fenced".into()), db.clone())
                .await
                .unwrap(),
        ));
        let recorder = Arc::new(
            Session::start_fenced_turn(
                session.clone(),
                BRANCH.into(),
                AGENT.into(),
                message("user", "2 + 2"),
                fail,
            )
            .await
            .unwrap(),
        );
        let input = db.snapshot().await.unwrap();
        let mock = MockServer::start().await;
        assert!(mock.address().ip().is_loopback());
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(move |req: &Request| stub_response(with_tool, req))
            .mount(&mock)
            .await;
        let mut config = Backend::new(BackendType::OpenAICompatible);
        config.api_base = Some(mock.uri());
        config.api_key = Some("fixture-only".into());
        config.max_retries = Some(0);
        let backend = BackendManager::new(&Some(vec![config]), SecretStore::new(db.clone()).await);
        let tools = Arc::new(ToolRegistry::new());
        let tool = Arc::new(AtomicUsize::new(0));
        let hook = Arc::new(AtomicUsize::new(0));
        let mut hub = context_hub().await;
        let host = Arc::new(MockHost::new());
        if with_tool {
            tools.register(Add(tool.clone()));
            hub.install_all(vec![counting_ext("fenced-hook", hook.clone())])
                .await
                .unwrap();
        }
        let mut context = tool_context_with_host(session.clone(), tools, host.clone());
        context.turn_request_id = Some(recorder.attempt.request_id.clone());
        context.turn_attempt = Some(crate::tool::ToolAttemptIdentity {
            attempt_id: recorder.attempt.attempt_id.clone(),
            execution_id: Some(BRANCH.into()),
        });
        context.active_extensions.insert("fenced-hook".into());
        let policies = ToolPolicyRegistry::new(
            [(
                TOOL.into(),
                ToolPolicy {
                    approval: ApprovalRequirement::Never,
                    ..Default::default()
                }
                .into(),
            )]
            .into(),
        );
        assert_eq!(db.snapshot().await.unwrap(), input);
        Self {
            instance,
            session,
            db,
            recorder,
            backend,
            mock,
            host,
            tool,
            hook,
            hub,
            context,
            policies,
            input,
        }
    }

    fn call(&self, model_sequence: u64) -> crate::session::CallId {
        crate::session::CallId {
            request: self.recorder.attempt.request_id.as_str().into(),
            branch: BRANCH.into(),
            model_sequence,
        }
    }

    async fn run(&self) -> Result<RuntimeOutcome, String> {
        let view = Session::history_at(&self.db, &self.input)
            .await
            .unwrap()
            .context()
            .await
            .unwrap();
        let config = ContextConfig::default();
        let context = ContextBuilder::from_history(&view, AGENT, "Fixture instructions", &config)
            .build()
            .await;
        execute_with_recorder(
            Some(MODEL),
            context.messages,
            &self.backend,
            &permissive_security(),
            &self.context,
            &self.policies,
            Some(self.recorder.clone()),
            Some(&self.hub),
            None,
            ModelCallScope {
                sources: context.sources,
                request_budget_tokens: Some(context.request_budget_tokens),
                attempt_id: Some(self.recorder.attempt.attempt_id.clone()),
                ..Default::default()
            },
        )
        .await
    }

    async fn complete(&self, text: &str) -> anyhow::Result<()> {
        let mut session = self.session.lock().await;
        self.recorder
            .complete(&mut session, Some(message(AGENT, text)), None)
            .await
    }

    async fn assert_effects(&self, models: usize, tool: usize, hook: usize) -> Vec<Value> {
        assert_eq!(self.tool.load(Ordering::SeqCst), tool);
        assert_eq!(self.hook.load(Ordering::SeqCst), hook);
        assert!(
            self.host.recorded_calls().is_empty(),
            "only the in-process tool is admitted"
        );
        let requests = self.mock.received_requests().await.unwrap();
        assert_eq!(requests.len(), models);
        requests
            .iter()
            .map(|req| {
                assert_eq!(req.url.path(), "/chat/completions");
                assert_eq!(req.headers["authorization"], "Bearer fixture-only");
                req.body_json().unwrap()
            })
            .collect()
    }

    async fn reopened(&self) -> Database {
        let account = self
            .instance
            .login_account("test", "test-password")
            .await
            .unwrap();
        account.open_database(self.db.root_id()).await.unwrap()
    }
}

fn message(sender: &str, text: &str) -> SessionEntry {
    SessionEntry {
        sender: sender.into(),
        content: text.into(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    }
}

async fn print(db: &Database) -> String {
    let (_instance, server, _registry, secrets, backend, _id, _db) = command_fixture().await;
    let root = db.root_id().to_string();
    let ctx = CommandContext {
        server: &server,
        secrets: &secrets,
        backend: &backend,
        session_db_id: &root,
        session_db: db,
        current_agent: AGENT,
        session_name: None,
    };
    match dispatch(Command::Print, &ctx).await {
        CommandOutcome::Text(text) => text,
        CommandOutcome::Error(error) => panic!("/print failed: {error}"),
        _ => panic!("unexpected /print outcome"),
    }
}

async fn success(with_tool: bool) {
    // Keep setup and readback futures off the native stack while polling ReAct.
    let fx = Box::pin(Fixture::new(with_tool, None)).await;
    let output = Box::pin(fx.run()).await.unwrap();
    assert_eq!(output.body, BODY);
    let observed = fx.db.snapshot().await.unwrap();
    assert_eq!(
        Box::pin(print(&fx.db)).await,
        "user: 2 + 2\n",
        "no publication before settlement"
    );
    Box::pin(fx.complete(&output.body)).await.unwrap();
    let head = fx.db.snapshot().await.unwrap();
    assert_ne!(head, observed);
    // Merge a harmless signed sibling. Readback must still be scoped to the
    // original branch at each frontier, not recomputed using the new head.
    let fork = fx.db.new_transaction_at(&fx.input).await.unwrap();
    fork.get_store::<DocStore>("fixture_metadata")
        .await
        .unwrap()
        .set("sibling", true)
        .await
        .unwrap();
    let sibling = fork.commit().await.unwrap();
    let merge = fx
        .db
        .new_transaction_at(&Snapshot::from([head.tips()[0].clone(), sibling]))
        .await
        .unwrap();
    merge
        .get_store::<DocStore>("fixture_metadata")
        .await
        .unwrap()
        .set("merged", true)
        .await
        .unwrap();
    merge.commit().await.unwrap();
    let reopened = fx.reopened().await;
    let merged = reopened.snapshot().await.unwrap();
    assert_eq!(
        Box::pin(print(&reopened)).await,
        "user: 2 + 2\nassistant: Four.\n"
    );
    let original_calls = Box::pin(fx.assert_effects(
        1 + usize::from(with_tool),
        usize::from(with_tool),
        usize::from(with_tool),
    ))
    .await;
    Box::pin(check_readback(&fx, &reopened, &head, &original_calls)).await;
    Box::pin(check_readback(&fx, &reopened, &merged, &original_calls)).await;
    assert!(Session::require_legacy_history(&reopened).await.is_err());
    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("cannot execute twice")
    );
    assert!(Box::pin(fx.complete(BODY)).await.is_err());
    Box::pin(fx.assert_effects(
        original_calls.len(),
        usize::from(with_tool),
        usize::from(with_tool),
    ))
    .await;
    assert_eq!(reopened.snapshot().await.unwrap(), merged);
}

async fn check_readback(fx: &Fixture, db: &Database, at: &Snapshot, wire: &[Value]) {
    use crate::session::{CallMessageBinding, Fidelity, TURN_ATTEMPTS_STORE, TurnAttemptStatus};
    let read = Session::history_at(db, at).await.unwrap();
    let inspected = read.inspect_conversation().await.unwrap().unwrap();
    assert!(inspected.issues.is_empty(), "{:?}", inspected.issues);
    let mut original_additions = Vec::new();
    for (sequence, request) in wire.iter().enumerate() {
        let call = read
            .recorded_call(&fx.call(sequence as u64), &fx.recorder.attempt.attempt_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call.requested_model, request["model"].as_str().unwrap());
        assert_eq!(call.api, "chat-completions");
        assert_eq!(call.fidelity, Fidelity::Approximate);
        assert_eq!(
            call.messages.len(),
            request["messages"].as_array().unwrap().len()
        );
        assert_eq!(
            call.tools.len(),
            request["tools"].as_array().map_or(0, Vec::len)
        );
        assert!(
            matches!(&call.messages[1], CallMessageBinding::Representation(id) if call.representations.first() == Some(id))
        );
        if sequence == 0 {
            original_additions.clone_from(&call.accepted_additions);
        } else {
            assert_eq!(
                call.accepted_additions, original_additions,
                "hook and tool definitions are not copied per call"
            );
        }
        let observed = Session::history_at(db, &call.observed.frontier)
            .await
            .unwrap();
        assert!(
            observed
                .recorded_call(&call.call, &call.attempt)
                .await
                .unwrap()
                .is_none()
        );
        assert!(observed.context().await.is_ok());
    }
    let first = wire[0]["messages"].as_array().unwrap();
    assert_eq!(first[0]["content"], "Fixture instructions");
    assert_eq!(first[1]["content"], "2 + 2");
    if wire.len() == 2 {
        assert_eq!(first[2]["content"], "injected");
        let last = wire[1]["messages"].as_array().unwrap();
        assert_eq!(last[3]["tool_calls"][0]["id"], "wire-1");
        assert_eq!(last[3]["reasoning_content"], "fixture reasoning");
        assert_eq!(last[4]["tool_call_id"], "wire-1");
        assert!(last[4]["content"].as_str().unwrap().contains("\n4\n"));
    }
    let telemetry = usage::telemetry_at_snapshot(db, at, &[]).await.unwrap();
    assert_eq!(telemetry.totals.calls as usize, wire.len());
    assert_eq!(telemetry.totals.prompt_tokens as usize, 12 * wire.len());
    assert_eq!(telemetry.totals.completion_tokens as usize, 3 * wire.len());
    let context = read.context().await.unwrap();
    let built = ContextBuilder::from_history(&context, AGENT, "", &ContextConfig::default())
        .build()
        .await;
    assert_eq!(
        built
            .messages
            .iter()
            .filter(|m| matches!(m, runtime::RuntimeMessage::Assistant(t) if t == BODY))
            .count(),
        1
    );
    assert_eq!(
        inspected
            .operations
            .atoms
            .values()
            .filter(|atom| atom.contains(BODY))
            .count(),
        1,
        "unchanged publication references the native body"
    );
    let started = fx.recorder.attempt.attempt_id.as_str();
    let txn = db.new_transaction_at(at).await.unwrap();
    let attempt = txn
        .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
        .await
        .unwrap()
        .get(started)
        .await
        .unwrap();
    assert_eq!(attempt.status, TurnAttemptStatus::Completed);
    assert_eq!(attempt.request_id, fx.recorder.attempt.request_id);
    for store in [
        "entries",
        "turn_transcript",
        "execution_transcript_v1",
        "call_telemetry",
    ] {
        assert!(
            crate::session::execution::store_entries_at(db, store, at)
                .await
                .unwrap()
                .is_empty(),
            "shadow write: {store}"
        );
    }
}

#[tokio::test]
async fn fenced_v1_public_no_tool_default_stack() {
    Box::pin(success(false)).await;
}

#[tokio::test]
async fn fenced_v1_public_tool_default_stack() {
    Box::pin(success(true)).await;
}

#[tokio::test]
async fn fenced_v1_dropped_response_does_not_commit_accounting_or_run_tool() {
    let fx = Box::pin(Fixture::new(true, Some(fenced::FailCommit::Response))).await;
    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("injected refusal after staging response")
    );
    let at = fx.db.snapshot().await.unwrap();
    let reopened = fx.reopened().await;
    let read = Session::history(&reopened).await.unwrap();
    let inspect = read.inspect_conversation().await.unwrap().unwrap();
    assert!(inspect.issues.is_empty());
    assert!(
        !inspect
            .operations
            .atoms
            .values()
            .any(|a| a.contains("NativeModelResponse"))
    );
    assert!(
        inspect
            .provenance
            .atoms
            .values()
            .any(|a| a.contains("CallDispatchIntent"))
    );
    assert!(
        !inspect
            .provenance
            .atoms
            .values()
            .any(|a| a.contains("CallOutcome"))
    );
    assert_eq!(
        usage::telemetry_at_snapshot(&reopened, &at, &[])
            .await
            .unwrap()
            .totals
            .calls,
        0
    );
    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("cannot execute twice")
    );
    assert!(Box::pin(fx.complete(BODY)).await.is_err());
    Box::pin(fx.assert_effects(1, 0, 1)).await;
    assert_eq!(Box::pin(print(&reopened)).await, "user: 2 + 2\n");
    assert_eq!(fx.db.snapshot().await.unwrap(), at);
}

#[tokio::test]
async fn fenced_v1_dropped_completion_does_not_publish_or_replay() {
    let fx = Box::pin(Fixture::new(false, Some(fenced::FailCommit::Completion))).await;
    let output = Box::pin(fx.run()).await.unwrap();
    let at = fx.db.snapshot().await.unwrap();
    assert!(
        Box::pin(fx.complete(&output.body))
            .await
            .unwrap_err()
            .to_string()
            .contains("injected refusal after staging completion")
    );
    let reopened = fx.reopened().await;
    assert_eq!(reopened.snapshot().await.unwrap(), at);
    let attempt = fx
        .session
        .lock()
        .await
        .read_turn_attempt(&fx.recorder.attempt.attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attempt.status, crate::session::TurnAttemptStatus::Started);
    assert_eq!(Box::pin(print(&reopened)).await, "user: 2 + 2\n");
    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("cannot execute twice")
    );
    assert!(Box::pin(fx.complete(BODY)).await.is_err());
    let reopened_session = Arc::new(Mutex::new(
        Session::new(
            crate::types::ConversationId("fenced".into()),
            reopened.clone(),
        )
        .await,
    ));
    assert!(
        Box::pin(Session::start_fenced_turn(
            reopened_session,
            BRANCH.into(),
            AGENT.into(),
            message("user", "replay"),
            None
        ))
        .await
        .is_err()
    );
    Box::pin(fx.assert_effects(1, 0, 0)).await;
    assert_eq!(reopened.snapshot().await.unwrap(), at);
}
