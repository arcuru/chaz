//! Use the public runtime with the real Chat Completions serializer, one
//! loopback backend, a pure hook and an in-process tool. No external transport
//! or production writer can be selected by this fixture.
use super::*;

mod compaction;
use crate::{
    backends::BackendManager,
    commands::{Command, CommandContext, CommandOutcome, command_fixture, dispatch},
    config::{Backend, BackendType, ContextConfig},
    context::ContextBuilder,
    extension::{ExtensionHub, counting_ext},
    runtime::{ModelCallScope, RuntimeOutcome, TurnCancel, execute_cancellable_with_recorder},
    security::SecretStore,
    test_support::{MockHost, context_hub, permissive_security, tool_context_with_host},
    tool::{
        ApprovalRequirement, Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy,
        ToolPolicyRegistry, ToolRegistry,
    },
};
use eidetica::{
    Instance, NewAccount,
    backend::database::InMemory,
    entry::{Entry, ID},
    store::{DocStore, Table},
};
use serde_json::{Value, json};
use std::{sync::atomic::AtomicUsize, time::Duration};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const AGENT: &str = "test-agent";
const MODEL: &str = "fenced-fixture";
const BRANCH: &str = "fixture-execution";
const TOOL: &str = "fake_add";
const BODY: &str = "Four.";
const CORRECTION: &str = "Correction: use 2 + 3 instead.";

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
    budget: Option<usize>,
    _disk: Option<tempfile::TempDir>,
    cancel: TurnCancel,
}

impl Fixture {
    async fn new(with_tool: bool, fail: Option<fenced::FailCommit>) -> Self {
        Self::build(with_tool, fail, None, move |req: &Request| {
            stub_response(with_tool, req)
        })
        .await
    }

    async fn build(
        with_tool: bool,
        fail: Option<fenced::FailCommit>,
        historical_input: Option<&str>,
        responder: impl wiremock::Respond + 'static,
    ) -> Self {
        let disk = historical_input.map(|_| tempfile::tempdir().unwrap());
        let (instance, db) = match &disk {
            Some(dir) => Box::pin(sqlite_fixture_db(dir)).await,
            None => {
                let (instance, _account, db) =
                    crate::session::test_helpers::test_session_db().await;
                (instance, db)
            }
        };
        let session = Arc::new(Mutex::new(
            Session::try_new(crate::types::ConversationId("fenced".into()), db.clone())
                .await
                .unwrap(),
        ));
        let cancel = TurnCancel::new();
        let recorder = Arc::new(match historical_input {
            Some(text) => Session::start_compacting_fixture(
                session.clone(),
                BRANCH.into(),
                AGENT.into(),
                message("user", text),
            )
            .await
            .unwrap(),
            None => Session::start_fenced_turn(
                session.clone(),
                BRANCH.into(),
                AGENT.into(),
                message("user", "2 + 2"),
                cancel.clone(),
                fail,
            )
            .await
            .unwrap(),
        });
        let input = db.snapshot().await.unwrap();
        let mock = MockServer::start().await;
        assert!(mock.address().ip().is_loopback());
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(responder)
            .mount(&mock)
            .await;
        let mut config = Backend::new(BackendType::OpenAICompatible);
        config.api_base = Some(mock.uri());
        config.api_key = Some("fixture-only".into());
        config.max_retries = Some(0);
        let backend = BackendManager::new(&Some(vec![config]), SecretStore::new(db.clone()).await);
        let tools = Arc::new(ToolRegistry::new());
        if historical_input.is_some() {
            tools.register(crate::tools::Compact);
        }
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
            [TOOL, "compact"]
                .map(|name| {
                    (
                        name.into(),
                        ToolPolicy {
                            approval: ApprovalRequirement::Never,
                            ..Default::default()
                        }
                        .into(),
                    )
                })
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
            budget: None,
            _disk: disk,
            cancel,
        }
    }

    fn call(&self, model_sequence: u64) -> crate::session::CallId {
        crate::session::CallId {
            request: self.recorder.attempt.request_id.as_str().into(),
            branch: self.recorder.identity.execution_id.clone().unwrap(),
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
        execute_cancellable_with_recorder(
            Some(MODEL),
            context.messages,
            &self.backend,
            &permissive_security(),
            &self.context,
            &self.policies,
            Some(self.recorder.clone()),
            Some(&self.hub),
            (!self.recorder.fenced_compaction).then_some(&self.cancel),
            None,
            ModelCallScope {
                sources: context.sources,
                request_budget_tokens: self.budget.or(Some(context.request_budget_tokens)),
                attempt_id: Some(self.recorder.attempt.attempt_id.clone()),
                ..Default::default()
            },
        )
        .await
        .map_err(|err| err.to_string())
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

async fn sqlite_fixture_db(dir: &tempfile::TempDir) -> (Instance, Database) {
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("fenced.db").display()
    );
    let backend = eidetica::backend::database::Sqlite::connect(&url)
        .await
        .unwrap();
    let (instance, mut user) = Instance::create_backend(
        Box::new(backend),
        eidetica::NewAccount::with_password("test", "test-password"),
    )
    .await
    .unwrap();
    let key = user.get_default_key().unwrap();
    let db = user
        .create_database(eidetica::crdt::Doc::new(), &key)
        .await
        .unwrap();
    (instance, db)
}

/// Wait for the serializer's actual request (not just prepare/intent), then
/// hold back its response until the other writer commits. Wiremock handles
/// requests on its own thread, so this does not block the runtime under test.
async fn pause_first_call(
    fx: &Fixture,
    with_tool: bool,
) -> (
    tokio::sync::oneshot::Receiver<Value>,
    std::sync::mpsc::Sender<()>,
) {
    use std::sync::Mutex;

    let (seen, request) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let barrier = Mutex::new(Some((seen, wait)));
    fx.mock.reset().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |req: &Request| {
            if let Some((seen, wait)) = barrier.lock().unwrap().take()
                && (seen.send(req.body_json().unwrap()).is_err()
                    || wait.recv_timeout(Duration::from_secs(30)).is_err())
            {
                return ResponseTemplate::new(500);
            }
            stub_response(with_tool, req)
        })
        .mount(&fx.mock)
        .await;
    (request, release)
}

async fn append_correction(db: &Database, at: &Snapshot) -> ID {
    use crate::session::conversation::staging::{MessageRole, Payload, Staging, View};

    let txn = db.new_transaction_at(at).await.unwrap();
    Staging::at(db, at, &txn, "independent-user")
        .await
        .unwrap()
        .append(vec![(
            Payload::Input {
                sender: "user".into(),
                text: CORRECTION.into(),
            },
            View::Message(MessageRole::User),
        )])
        .await
        .unwrap();
    txn.commit().await.unwrap()
}

#[tokio::test]
async fn fenced_v1_reply_keeps_its_intent_when_the_same_store_moves() {
    Box::pin(concurrent_reply()).await;
}

async fn concurrent_reply() {
    let fx = Box::pin(Fixture::new(true, None)).await;
    let (request, release) = pause_first_call(&fx, true).await;
    let (result, (intent, correction)) = tokio::join!(Box::pin(fx.run()), async {
        let request = tokio::time::timeout(Duration::from_secs(30), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request["messages"][1]["content"], "2 + 2");
        assert!(!request.to_string().contains(CORRECTION));
        let [intent]: [ID; 1] = fx
            .db
            .snapshot()
            .await
            .unwrap()
            .into_tips()
            .try_into()
            .unwrap();
        let writer = fx.reopened().await;
        let correction = Box::pin(append_correction(&writer, &fx.input)).await;
        assert_eq!(
            writer.snapshot().await.unwrap(),
            Snapshot::from([intent.clone(), correction.clone()])
        );
        release.send(()).unwrap();
        (intent, correction)
    });
    let output = result.unwrap();
    assert_eq!(output.body, BODY);
    Box::pin(fx.complete(&output.body)).await.unwrap();
    let reopened = fx.reopened().await;
    let joined = reopened.snapshot().await.unwrap();
    let reply = joined
        .tips()
        .iter()
        .find(|tip| **tip != correction)
        .unwrap();
    let reply_at = Snapshot::from([reply.clone()]);
    let correction_at = Snapshot::from([correction.clone()]);
    assert_eq!(joined, Snapshot::from([reply.clone(), correction.clone()]));
    assert_eq!(
        reopened
            .get_entry(&correction)
            .await
            .unwrap()
            .parents()
            .unwrap(),
        fx.input.tips()
    );

    let chain = linear_entries(&reopened, reply).await;
    assert!(chain.iter().all(|entry| entry.id_ref() != &correction));
    let response = chain
        .windows(2)
        .find(|pair| pair[0].id_ref() == &intent)
        .map(|pair| &pair[1])
        .unwrap();
    assert_eq!(response.parents().unwrap(), [intent]);
    assert!(response.in_subtree("conversation_v1"));
    assert!(response.in_subtree("conversation_provenance_v1"));

    let wire = Box::pin(fx.assert_effects(2, 1, 1)).await;
    assert!(
        wire.iter()
            .all(|request| !request.to_string().contains(CORRECTION))
    );
    Box::pin(check_readback(&fx, &reopened, &reply_at, &wire)).await;
    Box::pin(check_readback(&fx, &reopened, &joined, &wire)).await;
    let corrected = Session::history_at(&reopened, &correction_at)
        .await
        .unwrap();
    assert!(
        corrected
            .recorded_call(&fx.call(0), &fx.recorder.attempt.attempt_id)
            .await
            .unwrap()
            .is_none()
    );

    let on_reply = Box::pin(prepare_c(&reopened, &reply_at)).await;
    let on_correction = Box::pin(prepare_c(&reopened, &correction_at)).await;
    let on_both = Box::pin(prepare_c(&reopened, &joined)).await;
    assert!(!on_reply.to_string().contains(CORRECTION));
    assert!(!on_correction.to_string().contains(BODY));
    let messages = on_both["messages"].as_array().unwrap();
    for text in ["2 + 2", CORRECTION, BODY] {
        assert_eq!(
            messages.iter().filter(|msg| msg["content"] == text).count(),
            1
        );
    }
    let native = messages
        .iter()
        .position(|msg| !msg["tool_calls"].is_null())
        .unwrap();
    assert_eq!(messages[native]["tool_calls"][0]["id"], "wire-1");
    assert_eq!(messages[native + 1]["tool_call_id"], "wire-1");
    assert!(
        messages[native + 1]["content"]
            .as_str()
            .unwrap()
            .contains("\n4\n")
    );
    let rendered = Box::pin(print(&reopened)).await;
    assert_eq!(
        rendered,
        format!("user: 2 + 2\nuser: {CORRECTION}\nassistant: {BODY}\n")
    );

    // Keep the same signed bytes; the second peer receives the full reply
    // lineage first and the correction last. Verify rather than asserting trust.
    let (peer, _) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::passwordless("reverse-arrival"),
    )
    .await
    .unwrap();
    let written = chain.len();
    for entry in chain {
        peer.backend().put(entry).await.unwrap();
    }
    let reversed = Database::open(&peer, reopened.root_id()).await.unwrap();
    let report = reversed.verify().await.unwrap();
    assert_eq!(
        (report.verified, report.failed, report.still_unverified),
        (written, 0, 0)
    );
    assert_eq!(reversed.snapshot().await.unwrap(), reply_at);
    reversed
        .insert_raw(reopened.get_entry(&correction).await.unwrap())
        .await
        .unwrap();
    let report = reversed.verify().await.unwrap();
    assert_eq!(
        (report.verified, report.failed, report.still_unverified),
        (1, 0, 0)
    );
    assert_eq!(reversed.snapshot().await.unwrap(), joined);
    assert_eq!(Box::pin(print(&reversed)).await, rendered);
    assert_eq!(Box::pin(prepare_c(&reversed, &joined)).await, on_both);
    Box::pin(check_readback(&fx, &reversed, &joined, &wire)).await;

    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("cannot execute twice")
    );
    assert!(Box::pin(fx.complete(BODY)).await.is_err());
    Box::pin(fx.assert_effects(2, 1, 1)).await;
    assert_eq!(reopened.snapshot().await.unwrap(), joined);
    assert_eq!(reversed.snapshot().await.unwrap(), joined);

    println!("Joined /print:\n{rendered}Prepared C (not dispatched):\n{on_both}");
}

async fn linear_entries(db: &Database, tip: &ID) -> Vec<Entry> {
    let mut tip = tip.clone();
    let mut entries = Vec::new();
    loop {
        let entry = db.get_entry(&tip).await.unwrap();
        let parents = entry.parents().unwrap();
        entries.push(entry);
        if tip == *db.root_id() {
            break;
        }
        [tip] = parents.try_into().unwrap();
    }
    entries.reverse();
    entries
}

async fn prepare_c(db: &Database, at: &Snapshot) -> Value {
    let history = Session::history_at(db, at)
        .await
        .unwrap()
        .context()
        .await
        .unwrap();
    let context = ContextBuilder::from_history(&history, AGENT, "", &ContextConfig::default())
        .build()
        .await;
    // The real Chat Completions serializer, without dispatching another call.
    crate::openai::serialize_cache_fixture(
        &context.messages,
        &[],
        &Default::default(),
        false,
        MODEL,
    )
    .await
}

#[tokio::test]
async fn fenced_v1_refuses_a_reply_after_writer_revocation() {
    Box::pin(refuse_withdrawn_authority(true)).await;
}

#[tokio::test]
async fn fenced_v1_refuses_a_reply_after_attempt_settled_elsewhere() {
    Box::pin(refuse_withdrawn_authority(false)).await;
}

async fn refuse_withdrawn_authority(revoke_key: bool) {
    let fx = Box::pin(Fixture::new(true, None)).await;
    let (request, release) = pause_first_call(&fx, true).await;
    let (result, (latest, reason)) = tokio::join!(Box::pin(fx.run()), async {
        tokio::time::timeout(Duration::from_secs(30), request)
            .await
            .unwrap()
            .unwrap();
        let txn = fx.db.new_transaction().await.unwrap();
        let reason = if revoke_key {
            let writer = fx
                .db
                .auth_identity()
                .unwrap()
                .hint()
                .pubkey
                .as_ref()
                .unwrap();
            txn.get_settings()
                .unwrap()
                .revoke_auth_key(writer)
                .await
                .unwrap();
            "fenced writer"
        } else {
            let mut attempt = fx.recorder.attempt.clone();
            attempt.status = crate::session::TurnAttemptStatus::Completed;
            attempt.completed_at = Some(Utc::now());
            txn.get_store::<Table<TurnAttempt>>(crate::session::TURN_ATTEMPTS_STORE)
                .await
                .unwrap()
                .set(&attempt.attempt_id, attempt.clone())
                .await
                .unwrap();
            "no longer owns its attempt"
        };
        let latest = Snapshot::from([txn.commit().await.unwrap()]);
        assert_eq!(
            fx.db
                .current_permission()
                .await
                .is_ok_and(|p| p.can_write()),
            !revoke_key
        );
        release.send(()).unwrap();
        (latest, reason)
    });
    assert!(result.unwrap_err().contains(reason));
    assert_eq!(fx.db.snapshot().await.unwrap(), latest);
    Box::pin(fx.assert_effects(1, 0, 1)).await;
}

#[tokio::test]
async fn fenced_v1_cancelled_in_flight_does_not_record_or_run_tools() {
    let fx = Box::pin(Fixture::new(true, None)).await;
    let (request, release) = pause_first_call(&fx, true).await;
    let (result, before_stop) = tokio::join!(Box::pin(fx.run()), async {
        tokio::time::timeout(Duration::from_secs(30), request)
            .await
            .unwrap()
            .unwrap();
        let at = fx.db.snapshot().await.unwrap();
        assert!(fx.cancel.cancel());
        release.send(()).unwrap();
        at
    });
    assert!(result.unwrap_err().contains("cancelled"));
    assert!(Box::pin(fx.complete(BODY)).await.is_err());
    assert_eq!(fx.db.snapshot().await.unwrap(), before_stop);
    Box::pin(fx.assert_effects(1, 0, 1)).await;
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
            TurnCancel::new(),
            None
        ))
        .await
        .is_err()
    );
    Box::pin(fx.assert_effects(1, 0, 0)).await;
    assert_eq!(reopened.snapshot().await.unwrap(), at);
}
