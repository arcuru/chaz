//! Disposable executor restart (kill-window) tests for checkpointed `job_wait`.
//!
//! Each "process" is a separate executor connected to one Eidetica service;
//! `shutdown` aborts its turns mid-flight exactly where a crash would leave
//! durable state. Counters and the shared service prove which effects ran.

use super::*;
use crate::session::jobs::JobState;
use crate::session::wait_resume::{JOB_WAIT_CHECKPOINTS_STORE, JobWaitCheckpoint};
use crate::test_support::MockBackend;
use eidetica::service::ServiceServer;
use std::time::Duration;
use tokio::sync::watch;

/// A countable stand-in for an effectful tool run before the wait.
struct Effect {
    calls: Arc<AtomicUsize>,
    sessions: Arc<std::sync::Mutex<Vec<std::sync::Weak<Mutex<Session>>>>>,
    retain: Arc<std::sync::Mutex<Option<Arc<Mutex<Session>>>>>,
    keep: Arc<AtomicBool>,
}

impl crate::tool::Tool for Effect {
    fn descriptor(&self) -> crate::tool::ToolDescriptor {
        crate::tool::ToolDescriptor {
            name: "effect".into(),
            description: "Count an external effect".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn execute<'a>(
        &'a self,
        _: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.sessions
                .lock()
                .unwrap()
                .push(Arc::downgrade(&ctx.session));
            if self.keep.load(Ordering::SeqCst) {
                *self.retain.lock().unwrap() = Some(ctx.session.clone());
            }
            Ok("effect ran".into())
        })
    }
}

#[derive(Clone)]
struct ResultCounter {
    count: Arc<AtomicUsize>,
    manifest: crate::extension::manifest::ExtensionManifest,
}

type WaitDeadlines = Arc<std::sync::Mutex<Vec<Option<chrono::DateTime<Utc>>>>>;

/// Observe the runtime's actual deadline without replacing the real wait tool.
struct ObservedJobWait {
    inner: crate::tools::JobWaitTool,
    deadlines: WaitDeadlines,
}

impl crate::tool::Tool for ObservedJobWait {
    fn descriptor(&self) -> crate::tool::ToolDescriptor {
        crate::tool::Tool::descriptor(&self.inner)
    }

    fn default_policy(&self) -> crate::tool::ToolPolicy {
        crate::tool::Tool::default_policy(&self.inner)
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>,
    > {
        self.deadlines
            .lock()
            .unwrap()
            .push(ctx.observation_deadline);
        crate::tool::Tool::execute(&self.inner, arguments, ctx)
    }
}

impl crate::extension::Extension for ResultCounter {
    fn name(&self) -> &'static str {
        "result-counter"
    }

    fn supported_hooks(&self) -> &[crate::extension::HookKind] {
        &self.manifest.supported_hooks
    }

    fn instantiate<'a>(
        &'a self,
        _: crate::extension::ScopeCtx<'a>,
    ) -> crate::extension::instance::InstantiateFuture<'a> {
        Box::pin(async move {
            Ok(Arc::new(self.clone()) as Arc<dyn crate::extension::ExtensionInstance>)
        })
    }
}

impl crate::extension::ExtensionInstance for ResultCounter {
    fn manifest(&self) -> &crate::extension::manifest::ExtensionManifest {
        &self.manifest
    }

    fn tool_result_hook(
        &self,
    ) -> Option<Arc<dyn crate::extension::handler::HookHandlerToolResult>> {
        Some(Arc::new(self.clone()))
    }
}

impl crate::extension::handler::HookHandlerToolResult for ResultCounter {
    fn on_tool_result<'a>(
        &'a self,
        _: &'a str,
        output: String,
    ) -> crate::extension::handler::HandlerFuture<'a, String> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            output
        })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    settings: crate::config::EideticaConfig,
    agent: DbEntry,
    effects: Arc<AtomicUsize>,
    sessions: Arc<std::sync::Mutex<Vec<std::sync::Weak<Mutex<Session>>>>>,
    retain: Arc<std::sync::Mutex<Option<Arc<Mutex<Session>>>>>,
    keep: Arc<AtomicBool>,
    wait_deadlines: Option<WaitDeadlines>,
    _shutdown: watch::Sender<()>,
    _service: tokio::task::JoinHandle<eidetica::Result<()>>,
}

struct Process {
    server: Arc<Server>,
    child_mock: Arc<MockBackend>,
    _slot: crate::instance::ServerSlot,
}

impl Fixture {
    async fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(format!("{name}.sock"));
        let (owner, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewAccount::passwordless(name))
                .await
                .unwrap();
        let (agent_db, pubkey) = create_agent_db(
            &mut user,
            "default",
            &AgentDbConfig::default(),
            &AgentMeta {
                display_name: Some("default".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let service = ServiceServer::bind(owner, &socket).await.unwrap();
        let (shutdown, receiver) = watch::channel(());
        let service = tokio::spawn(service.run(receiver));
        Self {
            _dir: dir,
            settings: crate::config::EideticaConfig {
                connection: format!("unix://{}", socket.display()),
                login: crate::config::EideticaLoginConfig {
                    username: name.into(),
                    password: None,
                    passwordless: true,
                },
                max_frame_bytes: None,
                sync: None,
            },
            agent: DbEntry {
                db_id: agent_db.id(),
                display_name: "default".into(),
                pubkey,
            },
            effects: Arc::new(AtomicUsize::new(0)),
            sessions: Default::default(),
            retain: Default::default(),
            keep: Default::default(),
            wait_deadlines: None,
            _shutdown: shutdown,
            _service: service,
        }
    }

    /// Start one executor process. Its default backend runs Agent jobs
    /// and adopted sessions, and first answers "finished".
    async fn start(&self) -> Process {
        let mock = Arc::new(MockBackend::new());
        mock.push_text("finished");
        self.start_with(mock).await
    }

    async fn start_counting_results(&self, results: Arc<AtomicUsize>) -> Process {
        let mut hub = crate::test_support::context_hub().await;
        hub.install_all(vec![Arc::new(ResultCounter {
            count: results,
            manifest: crate::extension::manifest::ExtensionManifest {
                name: "result-counter".into(),
                extension_ref: crate::extension::ExtensionRef::builtin("result-counter"),
                supported_hooks: vec![crate::extension::HookKind::ToolResult],
                required_capabilities: vec![],
                requested_capabilities: vec![],
                provides_capabilities: vec![],
            },
        })])
        .await
        .unwrap();
        let mock = Arc::new(MockBackend::new());
        mock.push_text("finished");
        self.start_with_hub(mock, Arc::new(hub)).await
    }

    async fn start_with(&self, mock: Arc<MockBackend>) -> Process {
        self.start_with_hub(mock, Arc::new(crate::test_support::context_hub().await))
            .await
    }

    async fn start_with_hub(
        &self,
        mock: Arc<MockBackend>,
        hub: Arc<crate::extension::ExtensionHub>,
    ) -> Process {
        let (server, child_mock) = published_executor_using_mock_and_hub(
            &self.settings,
            Arc::new(AgentRegistry::with_default_agent()),
            self.agent.clone(),
            mock,
            hub,
            true,
        )
        .await;
        let slot = crate::instance::ServerSlot::default();
        slot.set(server.clone());
        let wait_tool = crate::tools::JobWaitTool {
            server: slot.clone(),
        };
        if let Some(deadlines) = &self.wait_deadlines {
            server.tools.register(ObservedJobWait {
                inner: wait_tool,
                deadlines: deadlines.clone(),
            });
        } else {
            server.tools.register(wait_tool);
        }
        server.tools.register(Effect {
            calls: self.effects.clone(),
            sessions: self.sessions.clone(),
            retain: self.retain.clone(),
            keep: self.keep.clone(),
        });
        Process {
            server,
            child_mock,
            _slot: slot,
        }
    }
}

impl Process {
    async fn register_parent(&self, db: &eidetica::Database, mock: &Arc<MockBackend>) {
        let secrets =
            crate::security::SecretStore::new(self.server.registry.chaz_peer().clone()).await;
        self.server
            .register_session(
                db,
                crate::backends::BackendManager::with_mock(mock.clone(), secrets),
                None,
                None,
            )
            .await
            .unwrap();
    }
}

async fn checkpoints(db: &eidetica::Database) -> Vec<JobWaitCheckpoint> {
    db.new_transaction()
        .await
        .unwrap()
        .get_store::<eidetica::store::Table<JobWaitCheckpoint>>(JOB_WAIT_CHECKPOINTS_STORE)
        .await
        .unwrap()
        .search(|_| true)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, checkpoint)| checkpoint)
        .collect()
}

fn finished_checkpoint(mut checkpoint: JobWaitCheckpoint) -> JobWaitCheckpoint {
    checkpoint.observation_finished = true;
    checkpoint
}

async fn job_count(server: &Server) -> usize {
    server
        .registry()
        .list_sessions()
        .await
        .unwrap()
        .iter()
        .filter(|row| {
            row.source
                .as_deref()
                .is_some_and(|source| source.starts_with("job-stage:"))
        })
        .count()
}

fn wait_call(child: &str, seconds: u64) -> (String, String, String) {
    (
        "wait".into(),
        "job_wait".into(),
        serde_json::json!({"session_db_id": child, "timeout_seconds": seconds}).to_string(),
    )
}

fn wait_result(call: &crate::test_support::RecordedCall) -> String {
    call.messages
        .iter()
        .find_map(|message| match message {
            runtime::RuntimeMessage::ToolResult { call_id, content } if call_id == "wait" => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("the continuation sees the wait's result")
}

/// Where the child is when process A dies.
#[derive(Clone, Copy, PartialEq)]
enum ChildAtCrash {
    /// Prepared but unpublished: it has no start, so nothing about it is
    /// ambiguous. A later process publishes it.
    Unpublished,
    /// Running behind a model gate: its own start becomes interrupted.
    Running,
    /// Rejected while the parent is down; it must never execute.
    Rejected,
}

struct Crashed {
    sid: String,
    db: eidetica::Database,
    child: String,
    checkpoint: JobWaitCheckpoint,
    first_mock: Arc<MockBackend>,
    /// Keeps the preparing client alive so a later step can publish.
    registry: Arc<crate::session::SessionRegistry>,
    prepared: Option<crate::session::PreparedAgentJob>,
}

/// Process A runs `effect`, then waits on the child and dies after
/// persisting the checkpoint.
async fn crash_while_waiting(fx: &Fixture, seconds: u64, mode: ChildAtCrash) -> Crashed {
    crash_while_waiting_with_input(fx, seconds, mode, false).await
}

async fn crash_while_waiting_with_input(
    fx: &Fixture,
    seconds: u64,
    mode: ChildAtCrash,
    steer: bool,
) -> Crashed {
    let a = fx.start().await;
    let registry = a.server.registry.clone();
    let (sid, db) = registry.create_session(Some("wait-parent")).await.unwrap();
    registry
        .attach_agent_to_session(&sid.0, &fx.agent)
        .await
        .unwrap();
    let (child, prepared) = match mode {
        ChildAtCrash::Unpublished | ChildAtCrash::Rejected => {
            let prepared = registry
                .prepare_agent_job(&sid.0, "default", "child task")
                .await
                .unwrap();
            (prepared.db.root_id().to_string(), Some(prepared))
        }
        ChildAtCrash::Running => {
            let gate = a.child_mock.block_next_call();
            let child = a
                .server
                .submit_agent_job(&sid.0, "default", "child task")
                .await
                .unwrap();
            tokio::time::timeout(ASYNC_TEST_TIMEOUT, gate.wait_started())
                .await
                .unwrap();
            (child, None)
        }
    };
    let parent_mock = Arc::new(MockBackend::new());
    parent_mock.push_tool_calls([("effect-call".into(), "effect".into(), "{}".into())]);
    parent_mock.push_tool_calls([wait_call(&child, seconds)]);
    let gate = steer.then(|| parent_mock.block_next_call());
    a.register_parent(&db, &parent_mock).await;
    Session::new(sid.clone(), db.clone())
        .await
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "observe the child".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Directive,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    if let Some(gate) = gate {
        tokio::time::timeout(ASYNC_TEST_TIMEOUT, gate.wait_started())
            .await
            .unwrap();
        let attempt = Session::steering_attempts(&db)
            .await
            .unwrap()
            .pop()
            .unwrap();
        Session::new(sid.clone(), db.clone())
            .await
            .submit_turn_input(crate::session::steering::JobInputRequest::new(
                attempt,
                "before checkpoint steering".into(),
            ))
            .await
            .unwrap();
        gate.release();
    }
    let checkpoint = tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        loop {
            if let Some(checkpoint) = checkpoints(&db).await.pop() {
                return checkpoint;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the wait must checkpoint before observing");
    // Observing, not finished: the checkpoint has no result yet.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(parent_mock.recorded_calls().len(), 2);
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    assert_eq!(checkpoint.child_session_db_id, child);
    assert_eq!(
        checkpoint.deadline - checkpoint.checkpointed_at,
        chrono::Duration::seconds(seconds as i64)
    );
    a.server.shutdown().await;
    drop(a);
    if mode == ChildAtCrash::Rejected {
        crate::session::jobs::reject_job(&prepared.as_ref().unwrap().db, "child refused")
            .await
            .unwrap();
    }
    Crashed {
        sid: sid.0,
        db,
        child,
        checkpoint,
        first_mock: parent_mock,
        registry,
        prepared,
    }
}

/// A restarted executor adopts the indexed parent on its own, with its
/// default backend: no frontend re-registration is involved.
async fn await_resumed(process: &Process, attempt_id: &str) {
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        while !process
            .server
            .live_attempts
            .lock()
            .await
            .contains(attempt_id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the checkpointed attempt itself must resume");
}

async fn await_calls(mock: &MockBackend, count: usize) {
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        while mock.recorded_calls().len() < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("model call did not arrive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_resumes_only_the_same_wait_and_survives_a_second_crash() {
    let fx = Fixture::new("wait-resume").await;
    let crashed = crash_while_waiting(&fx, 240, ChildAtCrash::Unpublished).await;
    let (sid, db, child, checkpoint) = (
        crashed.sid.clone(),
        crashed.db.clone(),
        crashed.child.clone(),
        crashed.checkpoint.clone(),
    );

    // Process B resumes observing the still-pending child.
    let b = fx.start().await;
    await_resumed(&b, &checkpoint.attempt_id).await;
    // Duplicate wakeups cannot start a second continuation or attempt.
    for _ in 0..3 {
        assert_eq!(b.server.process_session(&sid).await.unwrap(), None);
        b.server
            .notify_tx
            .send(ProcessingCommand::Wake(sid.clone()))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(b.child_mock.recorded_calls().is_empty(), "still observing");
    // Crash again during the resumed wait (cancellation of the observation).
    b.server.shutdown().await;
    let b_mock = b.child_mock.clone();
    drop(b);

    // Process C resumes the same wait again; then the child is published,
    // runs once and finishes, and the parent continues.
    let c = fx.start().await;
    await_resumed(&c, &checkpoint.attempt_id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(c.child_mock.recorded_calls().is_empty());
    c.child_mock.push_text("parent done");
    crashed
        .registry
        .publish_agent_job(crashed.prepared.unwrap())
        .await
        .unwrap();
    await_calls(&c.child_mock, 2).await;
    await_no_processing(&c.server, &sid).await;

    assert_eq!(
        c.server.job_status(&child).await.unwrap().state,
        JobState::Succeeded {
            text: Some("finished".into())
        }
    );
    let calls = c.child_mock.recorded_calls();
    assert_eq!(calls.len(), 2, "one child turn and one parent continuation");
    let result = wait_result(&calls[1]);
    assert!(
        result.contains("Succeeded") && result.contains("finished"),
        "{result}"
    );
    assert_eq!(crashed.first_mock.recorded_calls().len(), 2);
    assert!(b_mock.recorded_calls().is_empty());
    assert_eq!(
        fx.effects.load(Ordering::SeqCst),
        1,
        "no earlier effect replay"
    );
    assert_eq!(job_count(&c.server).await, 1, "no second child");
    assert_eq!(
        checkpoints(&db).await,
        [finished_checkpoint(checkpoint.clone())],
        "deadline unchanged"
    );
    let reply = Session::new(ConversationId(sid.clone()), db.clone()).await;
    assert_eq!(
        reply.latest_entry().map(|entry| entry.content.as_str()),
        Some("parent done")
    );

    // One attempt, completed under its original identity, with a strictly
    // paired transcript that ordinary context replay accepts.
    let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
    let attempts = session.attempts_for_test().await;
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.attempt_id == checkpoint.attempt_id)
    );
    assert!(
        attempts
            .iter()
            .any(|attempt| attempt.status == crate::session::TurnAttemptStatus::Completed)
    );
    let shape: Vec<String> = session
        .turn_transcript(&checkpoint.attempt_id)
        .await
        .unwrap()
        .iter()
        .map(|record| match &record.message {
            TurnTranscriptMessage::ModelResponse {
                model_sequence,
                terminal,
                ..
            } => format!("{}:model:{model_sequence}:{terminal}", record.sequence),
            TurnTranscriptMessage::ToolResult {
                model_sequence,
                call_id,
                ..
            } => format!("{}:result:{model_sequence}:{call_id}", record.sequence),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "0:model:0:false",
            "1:result:0:effect-call",
            "2:model:1:false",
            "3:result:1:wait",
            "4:model:2:true",
        ]
    );
    let (_, history) = session.context_with_tool_history().await.unwrap();
    assert_eq!(
        history.iter().map(Vec::len).max(),
        Some(5),
        "completed attempt replays as one paired group"
    );
    c.server.shutdown().await;
}

/// Capture authoritative signed history, not heartbeats or extension-status
/// observations that a restarted executor may legitimately publish.
async fn wait_history(db: &eidetica::Database) -> std::collections::BTreeMap<String, Vec<u8>> {
    let snapshot = db.snapshot().await.unwrap();
    let mut entries = std::collections::BTreeMap::new();
    for store in [
        "entries",
        "turn_attempts",
        "turn_transcript",
        JOB_WAIT_CHECKPOINTS_STORE,
        "execution_starts_v1",
        "execution_transcript_v1",
        "execution_outcomes_v1",
    ] {
        for entry in crate::session::execution::store_entries_at(db, store, &snapshot)
            .await
            .unwrap()
        {
            entries.insert(entry.id().to_string(), entry.to_dagcbor().unwrap());
        }
    }
    entries
}

/// Raw rows go through the same typed Table reads as restart adoption. A
/// successful continuation is the positive control for all three held forms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persisted_checkpoint_versions_gate_restart_recovery() {
    use eidetica::store::Table;
    use serde_json::{Value, json};

    for (case, version, decode_error) in [
        ("supported", Some(json!(1)), None),
        ("future", Some(json!(2)), None),
        ("missing", None, Some("missing field `version`")),
        ("wrong-type", Some(json!("1")), Some("invalid type")),
    ] {
        let fx = Fixture::new(case).await;
        let mut crashed = crash_while_waiting(&fx, 240, ChildAtCrash::Unpublished).await;
        let checkpoint = &crashed.checkpoint;
        let original = Session::new(ConversationId(crashed.sid.clone()), crashed.db.clone()).await;
        let attempts = original.attempts_for_test().await;
        let transcript = original
            .turn_transcript(&checkpoint.attempt_id)
            .await
            .unwrap();
        assert_eq!(
            transcript.len(),
            3,
            "effect and its result, then pending wait"
        );
        let mut raw = serde_json::to_value(checkpoint).unwrap();
        raw.as_object_mut().unwrap().remove("version");
        if let Some(version) = version {
            raw["version"] = version;
        }
        let txn = crashed.db.new_transaction().await.unwrap();
        txn.get_store::<Table<Value>>(JOB_WAIT_CHECKPOINTS_STORE)
            .await
            .unwrap()
            .set(checkpoint.key(), raw.clone())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let history = wait_history(&crashed.db).await;
        assert!(!history.is_empty());
        let results = Arc::new(AtomicUsize::new(0));

        // Both negative reloads use fresh service connections and fresh server
        // reconciliation caches; no serialized fixture or planner-only shortcut.
        for _ in 0..if case == "supported" { 1 } else { 2 } {
            let process = fx.start_counting_results(results.clone()).await;
            let (sid, db) = process
                .server
                .registry
                .open_session(&crashed.sid)
                .await
                .unwrap();
            let session = Session::new(sid, db.clone()).await;
            let loaded = session
                .latest_job_wait_checkpoint(&checkpoint.attempt_id)
                .await;
            if let Some(message) = decode_error {
                let error = format!("{:#}", loaded.unwrap_err());
                assert!(
                    error.contains(message),
                    "{case}: typed Table decode: {error}"
                );
            } else {
                let loaded = loaded.unwrap().unwrap();
                assert_eq!(serde_json::to_value(&loaded).unwrap(), raw);
            }
            let secrets =
                crate::security::SecretStore::new(process.server.registry.chaz_peer().clone())
                    .await;
            let registration = process
                .server
                .register_session(
                    &db,
                    crate::backends::BackendManager::with_mock(process.child_mock.clone(), secrets),
                    None,
                    None,
                )
                .await;
            // watch_session's initial checkpoint scan also decodes the typed
            // row. Adoption may already have installed that watcher, so a
            // repeated registration can instead return Ok without rescanning.
            if let Err(error) = registration {
                let error = format!("{error:#}");
                assert!(
                    decode_error.is_some_and(|message| error.contains(message)),
                    "{case}: {error}"
                );
            }

            if case == "supported" {
                await_resumed(&process, &checkpoint.attempt_id).await;
                assert!(
                    process.child_mock.recorded_calls().is_empty(),
                    "same pending child"
                );
                assert_eq!(
                    results.load(Ordering::SeqCst),
                    0,
                    "no result hook before observation"
                );
                process.child_mock.push_text("version continuation");
                crashed
                    .registry
                    .publish_agent_job(crashed.prepared.take().unwrap())
                    .await
                    .unwrap();
                await_calls(&process.child_mock, 2).await;
                await_no_processing(&process.server, &crashed.sid).await;
                let calls = process.child_mock.recorded_calls();
                assert_eq!(calls.len(), 2, "one child and one parent continuation");
                assert!(wait_result(&calls[1]).contains("Succeeded"));
                assert_eq!(
                    results.load(Ordering::SeqCst),
                    1,
                    "the wait result hook really runs"
                );
                assert_eq!(job_count(&process.server).await, 1, "no replacement child");
                assert_eq!(
                    checkpoints(&db).await,
                    [finished_checkpoint(checkpoint.clone())],
                    "same request/attempt/call/child/deadline"
                );
                let completed = Session::new(ConversationId(crashed.sid.clone()), db.clone()).await;
                assert_eq!(
                    completed.latest_entry().unwrap().content,
                    "version continuation"
                );
                let records = completed
                    .turn_transcript(&checkpoint.attempt_id)
                    .await
                    .unwrap();
                assert_eq!(
                    &records[..3],
                    transcript.as_slice(),
                    "prior transcript untouched"
                );
                assert_eq!(records.len(), 5);
                assert!(
                    matches!(&records[3].message, TurnTranscriptMessage::ToolResult { model_sequence: 1, call_index: 0, call_id, name, .. } if call_id == &checkpoint.call_id && name == "job_wait")
                );
                assert!(matches!(
                    &records[4].message,
                    TurnTranscriptMessage::ModelResponse {
                        model_sequence: 2,
                        terminal: true,
                        ..
                    }
                ));
                let attempts = completed.attempts_for_test().await;
                assert!(
                    attempts
                        .iter()
                        .all(|attempt| attempt.attempt_id == checkpoint.attempt_id
                            && attempt.request_id == checkpoint.request_id)
                );
                assert!(
                    attempts
                        .iter()
                        .any(|attempt| attempt.status
                            == crate::session::TurnAttemptStatus::Completed)
                );
                let (_, paired) = completed.context_with_tool_history().await.unwrap();
                assert_eq!(paired.iter().map(Vec::len).max(), Some(5));
                let after = wait_history(&db).await;
                for (id, bytes) in &history {
                    assert_eq!(after.get(id), Some(bytes), "original Entry {id} preserved");
                }
            } else {
                for _ in 0..3 {
                    assert_eq!(
                        process.server.process_session(&crashed.sid).await.unwrap(),
                        None
                    );
                }
                assert_eq!(
                    process
                        .server
                        .interrupted_turns(&crashed.sid)
                        .await
                        .unwrap(),
                    vec![InterruptedTurn {
                        request_id: checkpoint.request_id.clone(),
                        attempt_id: checkpoint.attempt_id.clone(),
                    }],
                    "{case} stays explicitly interrupted, not live or completed"
                );
                assert!(
                    !process
                        .server
                        .live_attempts
                        .lock()
                        .await
                        .contains(&checkpoint.attempt_id)
                );
                assert!(
                    process.child_mock.recorded_calls().is_empty(),
                    "{case}: no continuation/model replay"
                );
                assert_eq!(results.load(Ordering::SeqCst), 0, "{case}: no result hook");
                assert_eq!(
                    job_count(&process.server).await,
                    0,
                    "{case}: no child publication"
                );
                assert_eq!(
                    session.attempts_for_test().await,
                    attempts,
                    "{case}: no replacement/completion"
                );
                assert_eq!(
                    session
                        .turn_transcript(&checkpoint.attempt_id)
                        .await
                        .unwrap(),
                    transcript
                );
                let txn = db.new_transaction().await.unwrap();
                assert_eq!(
                    txn.get_store::<Table<Value>>(JOB_WAIT_CHECKPOINTS_STORE)
                        .await
                        .unwrap()
                        .get(checkpoint.key())
                        .await
                        .unwrap(),
                    raw,
                    "{case}: raw checkpoint unchanged"
                );
                assert_eq!(
                    wait_history(&db).await,
                    history,
                    "{case}: no new or rewritten authoritative Entry/branch/receipt"
                );
            }
            assert_eq!(
                crashed.first_mock.recorded_calls().len(),
                2,
                "no earlier model replay"
            );
            assert_eq!(
                fx.effects.load(Ordering::SeqCst),
                1,
                "no earlier tool effect replay"
            );
            process.server.shutdown().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downtime_counts_against_the_original_deadline() {
    let fx = Fixture::new("wait-deadline").await;
    let crashed = crash_while_waiting(&fx, 5, ChildAtCrash::Unpublished).await;
    let remaining = (crashed.checkpoint.deadline - Utc::now())
        .to_std()
        .unwrap_or_default();
    tokio::time::sleep(remaining + Duration::from_millis(300)).await;

    // A reset deadline would observe for another five seconds.
    let started = std::time::Instant::now();
    let b = fx.start().await;
    await_calls(&b.child_mock, 1).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "an expired deadline must not restart the wait: {:?}",
        started.elapsed()
    );
    await_no_processing(&b.server, &crashed.sid).await;
    let result = wait_result(&b.child_mock.recorded_calls()[0]);
    assert!(
        result.contains("Pending"),
        "timeout returns current state: {result}"
    );
    assert_eq!(
        checkpoints(&crashed.db).await,
        [finished_checkpoint(crashed.checkpoint)]
    );
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        job_count(&b.server).await,
        0,
        "resumption publishes no child"
    );
    b.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_and_rejected_children_are_reported_without_rerunning() {
    for (label, mode) in [
        ("Interrupted", ChildAtCrash::Running),
        ("Rejected", ChildAtCrash::Rejected),
    ] {
        let fx = Fixture::new(label).await;
        let crashed = crash_while_waiting(&fx, 240, mode).await;

        let b = fx.start().await;
        await_calls(&b.child_mock, 1).await;
        await_no_processing(&b.server, &crashed.sid).await;
        let result = wait_result(&b.child_mock.recorded_calls()[0]);
        assert!(result.contains(label), "{result}");
        // The child's uncheckpointed start is neither replayed nor respawned:
        // the only model call was the parent's continuation.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(b.child_mock.recorded_calls().len(), 1);
        let state = b.server.job_status(&crashed.child).await.unwrap().state;
        if mode == ChildAtCrash::Running {
            assert!(matches!(state, JobState::Interrupted { .. }));
            assert_eq!(
                b.server
                    .interrupted_turns(&crashed.child)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        } else {
            assert_eq!(
                state,
                JobState::Rejected {
                    message: "child refused".into()
                }
            );
        }
        assert_eq!(
            job_count(&b.server).await,
            usize::from(mode == ChildAtCrash::Running)
        );
        assert_eq!(
            checkpoints(&crashed.db).await,
            [finished_checkpoint(crashed.checkpoint)]
        );
        b.server.shutdown().await;
    }
}

/// Crash before the checkpoint, and after the wait's result, both leave
/// ambiguous effects: those attempts stay held. A control with only the
/// checkpoint pending resumes, so the held cases are not vacuous.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_before_checkpoint_or_after_result_stays_held() {
    use crate::session::{TurnTranscriptMessage as Message, TurnTranscriptRecord};

    let fx = Fixture::new("wait-held").await;
    let b = fx.start().await;
    let registry = b.server.registry.clone();
    let mut parents = Vec::new();
    for case in ["before-checkpoint", "after-result", "control"] {
        let (sid, db) = registry.create_session(Some(case)).await.unwrap();
        registry
            .attach_agent_to_session(&sid.0, &fx.agent)
            .await
            .unwrap();
        let mut session = Session::new(sid.clone(), db.clone()).await;
        let request = session
            .add_entry(SessionEntry {
                sender: "user".into(),
                content: format!("{case} turn"),
                timestamp: Utc::now(),
                entry_type: EntryType::Directive,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        let snapshot = db.snapshot().await.unwrap();
        let attempt = session.start_turn_attempt(request.clone()).await.unwrap();
        let (_, _, arguments) = wait_call("missing-child", 30);
        let record = |sequence, message| TurnTranscriptRecord {
            identity: None,
            request_id: request.clone(),
            attempt_id: attempt.attempt_id.clone(),
            sequence,
            timestamp: Utc::now(),
            message,
        };
        session
            .append_turn_transcript(
                record(
                    0,
                    Message::ModelResponse {
                        model_sequence: 0,
                        content: None,
                        tool_calls: vec![runtime::ToolCallRequest {
                            id: "wait".into(),
                            name: "job_wait".into(),
                            arguments: arguments.clone(),
                        }],
                        provider_extra: Default::default(),
                        metadata: None,
                        terminal: false,
                    },
                ),
                Vec::new(),
            )
            .await
            .unwrap();
        if case != "before-checkpoint" {
            let now = Utc::now();
            session
                .record_job_wait_checkpoint(&JobWaitCheckpoint::new(
                    request.clone(),
                    attempt.attempt_id.clone(),
                    "default".into(),
                    snapshot,
                    runtime::JobWaitIntent {
                        model_sequence: 0,
                        call_index: 0,
                        call_id: "wait".into(),
                        arguments,
                        child_session_db_id: "missing-child".into(),
                        checkpointed_at: now,
                        deadline: now + chrono::Duration::seconds(30),
                        policy_timeout_seconds: 300,
                    },
                ))
                .await
                .unwrap();
        }
        if case == "after-result" {
            session
                .append_turn_transcript(
                    record(
                        1,
                        Message::ToolResult {
                            model_sequence: 0,
                            call_index: 0,
                            call_id: "wait".into(),
                            name: "job_wait".into(),
                            output: "observed".into(),
                            outcome: runtime::ToolResultOutcome::Success,
                        },
                    ),
                    Vec::new(),
                )
                .await
                .unwrap();
        }
        parents.push((case, sid.0, db, attempt));
    }
    // Every parent shares the process default backend, so its one call
    // can only be the control's continuation.
    for (_, _, db, _) in &parents {
        b.register_parent(db, &b.child_mock).await;
    }
    let control = &parents[2];
    await_calls(&b.child_mock, 1).await;
    await_no_processing(&b.server, &control.1).await;
    assert!(wait_result(&b.child_mock.recorded_calls()[0]).contains("Tool error"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        b.child_mock.recorded_calls().len(),
        1,
        "held cases must not resume"
    );
    for (case, sid, _, attempt) in &parents[..2] {
        assert_eq!(
            b.server.interrupted_turns(sid).await.unwrap(),
            vec![InterruptedTurn {
                request_id: attempt.request_id.clone(),
                attempt_id: attempt.attempt_id.clone(),
            }],
            "{case} stays held for explicit retry"
        );
    }
    b.server.shutdown().await;
}

/// A failed result commit must not authorize repeating result-hook effects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_after_observation_before_result_stays_held_after_restart() {
    let fx = Fixture::new("wait-result-failure").await;
    let crashed = crash_while_waiting(&fx, 240, ChildAtCrash::Unpublished).await;
    let results = Arc::new(AtomicUsize::new(0));
    let b = fx.start_counting_results(results.clone()).await;
    await_resumed(&b, &crashed.checkpoint.attempt_id).await;
    // Fail the next record: the wait result, after a read-only observation.
    *b.server.transcript_fail_after.lock().unwrap() = Some(0);
    crate::session::jobs::reject_job(&crashed.prepared.unwrap().db, "child refused")
        .await
        .unwrap();
    await_no_processing(&b.server, &crashed.sid).await;
    let session = Session::new(ConversationId(crashed.sid.clone()), crashed.db.clone()).await;
    assert_eq!(
        session
            .turn_transcript(&crashed.checkpoint.attempt_id)
            .await
            .unwrap()
            .len(),
        3
    );
    b.server.shutdown().await;
    drop(b);

    assert_eq!(
        results.load(Ordering::SeqCst),
        1,
        "hook ran before failed result commit"
    );
    let c = fx.start_counting_results(results.clone()).await;
    for _ in 0..3 {
        c.server.process_session(&crashed.sid).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        results.load(Ordering::SeqCst),
        1,
        "interrupted result hook is never repeated"
    );
    assert!(
        c.child_mock.recorded_calls().is_empty(),
        "no automatic continuation after the result boundary"
    );
    assert_eq!(
        checkpoints(&crashed.db).await,
        [finished_checkpoint(crashed.checkpoint.clone())]
    );
    assert_eq!(
        c.server.interrupted_turns(&crashed.sid).await.unwrap(),
        vec![InterruptedTurn {
            request_id: crashed.checkpoint.request_id,
            attempt_id: crashed.checkpoint.attempt_id,
        }]
    );
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    c.server.shutdown().await;
}

struct JobParentCrash {
    parent_job: String,
    parent_db: eidetica::Database,
    grandchild: crate::session::PreparedAgentJob,
    checkpoint: JobWaitCheckpoint,
    first_tools: Vec<String>,
    registry: Arc<crate::session::SessionRegistry>,
}

fn tool_names(call: &crate::test_support::RecordedCall) -> Vec<String> {
    let mut names: Vec<_> = call.tools.iter().map(|tool| tool.name.clone()).collect();
    names.sort();
    names
}

/// Process A runs an accepted Agent job J that waits on its own unpublished
/// child G, then dies with J's checkpoint persisted.
async fn crash_job_parent_while_waiting(fx: &Fixture) -> JobParentCrash {
    let a_mock = Arc::new(MockBackend::new());
    let a = fx.start_with(a_mock.clone()).await;
    let registry = a.server.registry.clone();
    let (root, _) = registry.create_session(Some("root")).await.unwrap();
    registry
        .attach_agent_to_session(&root.0, &fx.agent)
        .await
        .unwrap();
    let parent = registry
        .prepare_agent_job(&root.0, "default", "orchestrate")
        .await
        .unwrap();
    let parent_job = parent.db.root_id().to_string();
    let parent_db = parent.db.clone();
    let grandchild = registry
        .prepare_agent_job(&parent_job, "default", "grandchild task")
        .await
        .unwrap();
    a_mock.push_tool_calls([wait_call(&grandchild.db.root_id().to_string(), 240)]);
    registry.publish_agent_job(parent).await.unwrap();
    let checkpoint = tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        loop {
            if let Some(checkpoint) = checkpoints(&parent_db).await.pop() {
                return checkpoint;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the job's wait must checkpoint");
    let calls = a_mock.recorded_calls();
    assert_eq!(calls.len(), 1);
    let first_tools = tool_names(&calls[0]);
    assert!(first_tools.contains(&"job_wait".to_string()));
    assert!(
        !first_tools.contains(&"spawn_worker".to_string()),
        "the job runs under its accepted, narrowed scope"
    );
    a.server.shutdown().await;
    drop(a);
    JobParentCrash {
        parent_job,
        parent_db,
        grandchild,
        checkpoint,
        first_tools,
        registry,
    }
}

async fn await_branches(db: &eidetica::Database, count: usize) {
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        while crate::session::execution::ordered_starts(db)
            .await
            .unwrap()
            .len()
            < count
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("resumed execution branch is recorded");
}

/// With no claim to lapse, the restarted executor resumes the same wait at
/// once as an exact continuation of the crashed branch, keeping its scope.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_parent_resumes_as_exact_continuation_and_keeps_its_scope() {
    let fx = Fixture::new("wait-job-parent").await;
    let crashed = crash_job_parent_while_waiting(&fx).await;

    let b_mock = Arc::new(MockBackend::new());
    b_mock.push_text("grandchild finished");
    b_mock.push_text("orchestrated");
    let b = fx.start_with(b_mock.clone()).await;
    await_resumed(&b, &crashed.checkpoint.attempt_id).await;
    await_branches(&crashed.parent_db, 2).await;
    let starts = crate::session::execution::ordered_starts(&crashed.parent_db)
        .await
        .unwrap();
    assert_eq!(starts.len(), 2, "one branch per process run");
    assert!(
        starts
            .iter()
            .all(|s| s.start.attempt.attempt_id == crashed.checkpoint.attempt_id)
    );
    let resumed = starts[1]
        .start
        .continuation
        .as_ref()
        .expect("legitimate lineage");
    assert_eq!(resumed.predecessor, starts[0].start.id);
    assert_eq!(resumed.checkpoint, crashed.checkpoint.key());
    assert_eq!(
        starts[0].start.executor, starts[1].start.executor,
        "same executor, new incarnation"
    );
    assert_ne!(starts[0].start.incarnation, starts[1].start.incarnation);
    assert!(b_mock.recorded_calls().is_empty());
    let grandchild = crashed.grandchild.db.root_id().to_string();
    crashed
        .registry
        .publish_agent_job(crashed.grandchild)
        .await
        .unwrap();
    await_calls(&b_mock, 2).await;
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        while !b
            .server
            .job_status(&crashed.parent_job)
            .await
            .unwrap()
            .state
            .is_terminal()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        b.server
            .job_status(&crashed.parent_job)
            .await
            .unwrap()
            .state,
        JobState::Succeeded {
            text: Some("orchestrated".into())
        }
    );
    assert_eq!(
        b.server.job_status(&grandchild).await.unwrap().state,
        JobState::Succeeded {
            text: Some("grandchild finished".into())
        }
    );
    let calls = b_mock.recorded_calls();
    let continuation = &calls[1];
    assert!(wait_result(continuation).contains("grandchild finished"));
    assert_eq!(
        tool_names(continuation),
        crashed.first_tools,
        "the resumed job keeps its accepted tool scope"
    );
    assert_eq!(job_count(&b.server).await, 2, "no replacement job");
    let attempts = Session::new(
        ConversationId(crashed.parent_job.clone()),
        crashed.parent_db.clone(),
    )
    .await
    .attempts_for_test()
    .await;
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.attempt_id == crashed.checkpoint.attempt_id)
    );
    b.server.shutdown().await;
}

/// Accepted steering on an active resumed job follows the paired native wait
/// exchange and drains before terminal delivery, without widening its scope.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumed_job_wait_drains_accepted_steering_before_terminal_delivery() {
    use crate::session::steering::{JobInputRequest, JobInputState};

    let fx = Fixture::new("wait-resumed-steering").await;
    let crashed = crash_job_parent_while_waiting(&fx).await;
    let mock = Arc::new(MockBackend::new());
    mock.push_text("grandchild finished");
    mock.push_text("provisional parent result");
    mock.push_text("steered parent result");
    let child_gate = mock.block_next_call();
    let provisional_gate = mock.block_next_call();
    let final_gate = mock.block_next_call();
    let b = fx.start_with(mock.clone()).await;
    await_resumed(&b, &crashed.checkpoint.attempt_id).await;
    await_branches(&crashed.parent_db, 2).await;
    crashed
        .registry
        .publish_agent_job(crashed.grandchild)
        .await
        .unwrap();
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, child_gate.wait_started())
        .await
        .unwrap();
    child_gate.release();
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, provisional_gate.wait_started())
        .await
        .unwrap();
    let input = JobInputRequest::new(
        crashed.checkpoint.attempt_id.clone(),
        "steer the resumed job".into(),
    );
    b.server
        .submit_job_input(&crashed.parent_job, input.clone())
        .await
        .unwrap();
    b.server
        .submit_job_input(&crashed.parent_job, input.clone())
        .await
        .unwrap();
    assert!(
        crate::session::jobs::read_job_result(&crashed.parent_db)
            .await
            .unwrap()
            .is_none()
    );
    provisional_gate.release();
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, final_gate.wait_started())
        .await
        .unwrap();
    assert!(
        crate::session::jobs::read_job_result(&crashed.parent_db)
            .await
            .unwrap()
            .is_none(),
        "provisional answer cannot settle accepted input"
    );
    let calls = mock.recorded_calls();
    assert_eq!(
        calls.len(),
        3,
        "one child, one resumed call and one steering continuation"
    );
    assert!(wait_result(&calls[1]).contains("grandchild finished"));
    assert_eq!(tool_names(&calls[1]), crashed.first_tools);
    assert_eq!(tool_names(&calls[2]), crashed.first_tools);
    let messages = &calls[2].messages;
    let index = messages
        .iter()
        .position(|m| matches!(m, runtime::RuntimeMessage::User(text) if text == &input.text))
        .unwrap();
    assert!(messages[..index].iter().any(|m| matches!(m, runtime::RuntimeMessage::ToolResult { call_id, .. } if call_id == "wait")), "steering follows the completed native exchange");
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, runtime::RuntimeMessage::User(text) if text == &input.text))
            .count(),
        1
    );
    assert_eq!(
        checkpoints(&crashed.parent_db).await,
        [finished_checkpoint(crashed.checkpoint.clone())]
    );
    final_gate.release();
    await_no_processing(&b.server, &crashed.parent_job).await;
    assert_eq!(
        b.server
            .job_status(&crashed.parent_job)
            .await
            .unwrap()
            .state,
        JobState::Succeeded {
            text: Some("steered parent result".into())
        }
    );
    assert_eq!(
        b.server.job_inputs(&crashed.parent_job).await.unwrap()[0].state,
        JobInputState::Included { model_sequence: 2 }
    );
    assert_eq!(
        job_count(&b.server).await,
        2,
        "same parent and child; no respawn"
    );
    assert_eq!(
        crate::session::execution::ordered_starts(&crashed.parent_db)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        b.server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
    b.server.shutdown().await;
}

/// A later continuation of the resumed branch, observed while the wait
/// observes, stops it without a continuation; this process does not resume
/// it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observed_successor_during_resumed_wait_stops_without_continuing() {
    let fx = Fixture::new("wait-observed-yield").await;
    let crashed = crash_job_parent_while_waiting(&fx).await;
    let b_mock = Arc::new(MockBackend::new());
    b_mock.push_text("must not run");
    let b = fx.start_with(b_mock.clone()).await;
    await_resumed(&b, &crashed.checkpoint.attempt_id).await;
    await_branches(&crashed.parent_db, 2).await;
    let successor = crate::session::execution::test_supersede(&crashed.parent_db)
        .await
        .unwrap();
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        while b
            .server
            .live_attempts
            .lock()
            .await
            .contains(&crashed.checkpoint.attempt_id)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("an observed successor stops the resumed attempt");
    let yields = crate::session::execution::yields(&crashed.parent_db)
        .await
        .unwrap();
    assert_eq!(yields.len(), 1);
    assert_eq!(yields[0].earlier_execution, successor);
    assert!(yields[0].continuation_handover);
    // No elapsed-time takeover or revival: this process holds the attempt.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(b_mock.recorded_calls().is_empty(), "no continuation");
    assert!(
        !b.server
            .live_attempts
            .lock()
            .await
            .contains(&crashed.checkpoint.attempt_id)
    );
    assert!(matches!(
        b.server
            .job_status(&crashed.parent_job)
            .await
            .unwrap()
            .state,
        JobState::Interrupted { .. }
    ));
    assert_eq!(checkpoints(&crashed.parent_db).await, [crashed.checkpoint]);
    b.server.shutdown().await;
}

/// A resumed accepted job keeps the original monitor deadline and responds to
/// the existing user-stop event without running its child or replaying a call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_stop_during_resumed_job_wait_pairs_history_and_preserves_monitor_deadline() {
    let fx = Fixture::new("wait-resumed-user-stop").await;
    let crashed = crash_job_parent_while_waiting(&fx).await;
    let b = fx.start().await;
    await_resumed(&b, &crashed.checkpoint.attempt_id).await;
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        loop {
            let waits = crate::session::jobs::job_waits(&crashed.parent_db)
                .await
                .unwrap();
            if let [wait] = waits.as_slice() {
                assert_eq!(wait.attempt_id, crashed.checkpoint.attempt_id);
                assert_eq!(wait.child_id, crashed.checkpoint.child_session_db_id);
                assert_eq!(wait.deadline_at, crashed.checkpoint.deadline);
                assert!(!wait.finished);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the original wait activity remains visible after resume");
    let mut frontend = Session::new(
        ConversationId(crashed.parent_job.clone()),
        crashed.parent_db.clone(),
    )
    .await;
    assert!(frontend.request_stop("user").await.unwrap());
    await_no_processing(&b.server, &crashed.parent_job).await;
    let stopped = Session::new(
        ConversationId(crashed.parent_job.clone()),
        crashed.parent_db.clone(),
    )
    .await;
    let records = stopped
        .turn_transcript(&crashed.checkpoint.attempt_id)
        .await
        .unwrap();
    assert_eq!(records.len(), 3, "model, cancelled result, stop terminal");
    assert!(matches!(
        &records[1].message,
        TurnTranscriptMessage::ToolResult {
            model_sequence: 0,
            call_index: 0,
            call_id,
            outcome: runtime::ToolResultOutcome::Cancelled,
            ..
        } if call_id == "wait"
    ));
    assert!(matches!(
        records[2].message,
        TurnTranscriptMessage::Cancelled
    ));
    assert!(stopped.attempts_for_test().await.iter().any(|attempt| {
        attempt.attempt_id == crashed.checkpoint.attempt_id
            && attempt.status == crate::session::TurnAttemptStatus::Completed
    }));
    assert!(b.child_mock.recorded_calls().is_empty());
    assert_eq!(
        job_count(&b.server).await,
        1,
        "no replacement published job"
    );
    b.server.shutdown().await;
    drop(b);
    let c = fx.start().await;
    c.server.process_session(&crashed.parent_job).await.unwrap();
    assert!(c.child_mock.recorded_calls().is_empty(), "stop is terminal");
    assert!(
        c.server
            .interrupted_turns(&crashed.parent_job)
            .await
            .unwrap()
            .is_empty()
    );
    c.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checkpointed_wait_restores_prior_steering_without_model_or_effect_replay() {
    let fx = Fixture::new("wait-prior-steering").await;
    let crashed = crash_while_waiting_with_input(&fx, 240, ChildAtCrash::Unpublished, true).await;
    let b = fx.start().await;
    await_resumed(&b, &crashed.checkpoint.attempt_id).await;
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    assert!(
        b.child_mock.recorded_calls().is_empty(),
        "resumption only observes the pending wait"
    );
    b.child_mock.push_text("parent done");
    crashed
        .registry
        .publish_agent_job(crashed.prepared.unwrap())
        .await
        .unwrap();
    await_calls(&b.child_mock, 2).await;
    await_no_processing(&b.server, &crashed.sid).await;
    let calls = b.child_mock.recorded_calls();
    let messages = &calls[1].messages;
    let index = messages
        .iter()
        .position(
            |m| matches!(m, runtime::RuntimeMessage::User(t) if t == "before checkpoint steering"),
        )
        .unwrap();
    assert!(
        matches!(&messages[index-1], runtime::RuntimeMessage::ToolResult {call_id, ..} if call_id == "effect-call")
    );
    assert!(
        matches!(&messages[index+1], runtime::RuntimeMessage::AssistantToolCalls {tool_calls, ..} if tool_calls[0].name == "job_wait")
    );
    assert_eq!(messages.iter().filter(|m| matches!(m, runtime::RuntimeMessage::User(t) if t == "before checkpoint steering")).count(), 1);
    assert_eq!(crashed.first_mock.recorded_calls().len(), 2);
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        Session::new(ConversationId(crashed.sid), crashed.db)
            .await
            .attempts_for_test()
            .await
            .len(),
        1
    );
    b.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_job_steering_all_publications_follow_fifo_in_actual_request() {
    use crate::session::steering::JobInputRequest;
    let fx = Fixture::new("job-fifo-steering").await;
    let mock = Arc::new(MockBackend::new());
    mock.push_text("provisional");
    mock.push_text("finished");
    let first = mock.block_next_call();
    let next = mock.block_next_call();
    let process = fx.start_with(mock.clone()).await;
    let (parent, _) = process
        .server
        .registry
        .create_session(Some("tui"))
        .await
        .unwrap();
    process
        .server
        .registry
        .attach_agent_to_session(&parent.0, &fx.agent)
        .await
        .unwrap();
    let job = process
        .server
        .submit_agent_job(&parent.0, "default", "original job")
        .await
        .unwrap();
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, first.wait_started())
        .await
        .unwrap();
    let (_, db) = process
        .server
        .registry
        .open_job_session(&job)
        .await
        .unwrap();
    let attempt = Session::steering_attempts(&db)
        .await
        .unwrap()
        .pop()
        .unwrap();
    for (id, text) in [
        ("ffffffff-ffff-4fff-8fff-ffffffffffff", "first job steer"),
        ("00000000-0000-4000-8000-000000000000", "second job steer"),
    ] {
        process
            .server
            .submit_job_input(
                &job,
                JobInputRequest {
                    id: id.into(),
                    attempt_id: attempt.clone(),
                    text: text.into(),
                },
            )
            .await
            .unwrap();
    }
    first.release();
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, next.wait_started())
        .await
        .unwrap();
    let calls = mock.recorded_calls();
    let messages = &calls[1].messages;
    assert_eq!(
        &messages[messages.len() - 3..],
        &[
            runtime::RuntimeMessage::Assistant("provisional".into()),
            runtime::RuntimeMessage::User("first job steer".into()),
            runtime::RuntimeMessage::User("second job steer".into())
        ]
    );
    assert!(
        process
            .server
            .job_inputs(&job)
            .await
            .unwrap()
            .iter()
            .all(|input| input.incorporated)
    );
    assert!(
        crate::session::jobs::read_job_result(&db)
            .await
            .unwrap()
            .is_none()
    );
    next.release();
    assert!(
        process
            .server
            .wait_job(&job, ASYNC_TEST_TIMEOUT)
            .await
            .unwrap()
            .state
            .is_terminal()
    );
    assert_eq!(mock.recorded_calls().len(), 2);
    assert_eq!(
        Session::new(ConversationId(job), db)
            .await
            .attempts_for_test()
            .await
            .len(),
        1
    );
    process.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn origin_pending_refusal_remains_interrupted_at_executor() {
    use crate::session::{TurnTranscriptMessage, TurnTranscriptRecord};
    let fx = Fixture::new("native-origin-hold").await;
    let process = fx.start().await;
    let registry = process.server.registry.clone();
    let (sid, db) = registry.create_session(Some("origin-held")).await.unwrap();
    registry
        .attach_agent_to_session(&sid.0, &fx.agent)
        .await
        .unwrap();
    let mut session = Session::new(sid.clone(), db.clone()).await;
    let request = session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "wait".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Directive,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let attempt = session.start_turn_attempt(request.clone()).await.unwrap();
    let (_, _, arguments) = wait_call("missing-child", 30);
    session.append_turn_transcript(TurnTranscriptRecord { request_id: request.clone(),
        attempt_id: attempt.attempt_id.clone(), sequence: 0, timestamp: Utc::now(), identity: None,
        message: TurnTranscriptMessage::ModelResponse { model_sequence: 0, content: None,
            tool_calls: vec![runtime::ToolCallRequest { id: "wait".into(), name: "job_wait".into(), arguments: arguments.clone() }],
            provider_extra: serde_json::Map::from_iter([(crate::responses::OUTPUT_ITEMS.into(), serde_json::json!([
                {"type":"function_call","id":"native-wait","call_id":"wait","name":"job_wait","arguments":arguments}
            ]))]), metadata: None, terminal: false } }, vec![]).await.unwrap();
    let now = Utc::now();
    let checkpoint = JobWaitCheckpoint::new(
        request.clone(),
        attempt.attempt_id.clone(),
        "default".into(),
        db.snapshot().await.unwrap(),
        runtime::JobWaitIntent {
            model_sequence: 0,
            call_index: 0,
            call_id: "wait".into(),
            arguments,
            child_session_db_id: "missing-child".into(),
            checkpointed_at: now,
            deadline: now + chrono::Duration::seconds(30),
            policy_timeout_seconds: 300,
        },
    );
    session
        .record_job_wait_checkpoint(&checkpoint)
        .await
        .unwrap();
    process.register_parent(&db, &process.child_mock).await;
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        loop {
            if process
                .server
                .considered_wait_checkpoints
                .lock()
                .unwrap()
                .contains(&checkpoint.key())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    await_no_processing(&process.server, &sid.0).await;
    assert!(
        process.child_mock.recorded_calls().is_empty(),
        "no resumed model call"
    );
    assert_eq!(fx.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        process.server.interrupted_turns(&sid.0).await.unwrap(),
        vec![InterruptedTurn {
            request_id: request,
            attempt_id: attempt.attempt_id.clone(),
        }],
        "origin refusal must hold the existing attempt, not complete it as failed"
    );
    assert_eq!(checkpoints(&db).await, vec![checkpoint]);
    process.server.shutdown().await;
}
#[path = "timer_example.rs"]
mod timer_example;

struct WaitingParent {
    process: Process,
    sid: String,
    db: eidetica::Database,
    child: crate::session::PreparedAgentJob,
    mock: Arc<MockBackend>,
    checkpoint: JobWaitCheckpoint,
}

async fn waiting_parent(fx: &Fixture, seconds: u64, worker: bool, hints: bool) -> WaitingParent {
    let process = fx.start().await;
    process
        .server
        .suspended_wait_hints
        .store(hints, Ordering::SeqCst);
    let registry = process.server.registry.clone();
    let (sid, db) = registry
        .create_session(Some("unloaded-parent"))
        .await
        .unwrap();
    registry
        .attach_agent_to_session(&sid.0, &fx.agent)
        .await
        .unwrap();
    let child = registry
        .prepare_agent_job(&sid.0, "default", "child task")
        .await
        .unwrap();
    let mock = Arc::new(MockBackend::new());
    // One model response, with an earlier effect in the same tool batch.
    mock.push_tool_calls([
        ("effect-call".into(), "effect".into(), "{}".into()),
        wait_call(&child.db.root_id().to_string(), seconds),
    ]);
    mock.push_text("finished");
    process.register_parent(&db, &mock).await;
    if worker {
        let (tx, _rx) = mpsc::channel(1);
        process
            .server
            .sessions
            .lock()
            .await
            .get_mut(&sid.0)
            .unwrap()
            .completion_tx = Some(tx);
    }
    Session::new(sid.clone(), db.clone())
        .await
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "observe child".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Directive,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let checkpoint = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(checkpoint) = checkpoints(&db).await.pop() {
                break checkpoint;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    WaitingParent {
        process,
        sid: sid.0,
        db,
        child,
        mock,
        checkpoint,
    }
}

async fn await_unloaded(parent: &WaitingParent) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if parent
                .process
                .server
                .suspended_waits
                .lock()
                .await
                .contains_key(&parent.sid)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("wait must actually suspend in-process");
    assert!(
        parent
            .process
            .server
            .processing
            .lock()
            .await
            .contains(&parent.sid)
    );
    assert!(
        parent
            .process
            .server
            .live_attempts
            .lock()
            .await
            .contains(&parent.checkpoint.attempt_id)
    );
}

async fn await_pending_reconciliation(server: &Server) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.suspended_wait_reconciliations.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initial pending-state reconciliation must precede child writes");
}

fn assert_parent_dropped(fx: &Fixture) {
    let (count, retained) = {
        let weak = fx.sessions.lock().unwrap();
        (
            weak.len(),
            weak.first().is_some_and(|weak| weak.upgrade().is_some()),
        )
    };
    assert_eq!(count, 1, "the earlier tool ran once");
    assert!(!retained, "the live parent Session/context was retained");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_process_wait_unloads_parent_and_orders_new_turns() {
    let fx = Fixture::new("unload-parent").await;
    let parent = waiting_parent(&fx, 240, false, true).await;
    await_unloaded(&parent).await;
    assert_parent_dropped(&fx);
    assert_eq!(
        parent.process.server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
    assert_eq!(parent.mock.recorded_calls().len(), 1);
    let next_gate = parent.mock.block_next_call();
    parent.mock.push_text("newer turn");
    Session::new(ConversationId(parent.sid.clone()), parent.db.clone())
        .await
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "newer request".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Directive,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    for _ in 0..4 {
        parent
            .process
            .server
            .process_session(&parent.sid)
            .await
            .unwrap();
    }
    assert_eq!(
        parent.mock.recorded_calls().len(),
        1,
        "no turn overtakes the suspension"
    );
    await_pending_reconciliation(&parent.process.server).await;
    // Child writes are hints, not permission to advance a still-pending wait.
    let child_gate = parent.process.child_mock.block_next_call();
    let child_db = parent.child.db.clone();
    parent
        .process
        .server
        .registry
        .publish_agent_job(parent.child)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), child_gate.wait_started())
        .await
        .unwrap();
    let JobState::Running { attempt_id } = parent
        .process
        .server
        .job_status(&child_db.root_id().to_string())
        .await
        .unwrap()
        .state
    else {
        panic!("blocked child must be running");
    };
    for _ in 0..2 {
        Session::renew_turn_activity(&child_db, &attempt_id)
            .await
            .unwrap();
    }
    assert_eq!(parent.mock.recorded_calls().len(), 1);
    child_gate.release();
    tokio::time::timeout(Duration::from_secs(30), next_gate.wait_started())
        .await
        .unwrap();
    for _ in 0..2 {
        Session::renew_turn_activity(&child_db, &attempt_id)
            .await
            .unwrap();
    }
    for _ in 0..4 {
        parent
            .process
            .server
            .process_session(&parent.sid)
            .await
            .unwrap();
    }
    next_gate.release();
    await_calls(&parent.mock, 3).await;
    await_no_processing(&parent.process.server, &parent.sid).await;
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    assert_eq!(job_count(&parent.process.server).await, 1);
    assert_eq!(parent.process.child_mock.recorded_calls().len(), 1);
    assert_eq!(parent.mock.recorded_calls().len(), 3);
    assert!(wait_result(&parent.mock.recorded_calls()[1]).contains("Succeeded"));
    let s = Session::new(ConversationId(parent.sid.clone()), parent.db.clone()).await;
    let records = s
        .turn_transcript(&parent.checkpoint.attempt_id)
        .await
        .unwrap();
    assert_eq!(
        records.iter().map(|r| r.sequence).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(
        checkpoints(&parent.db).await,
        [finished_checkpoint(parent.checkpoint)]
    );
    parent.process.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unloaded_drop_probe_detects_a_retained_strong_reference() {
    let fx = Fixture::new("unload-control").await;
    fx.keep.store(true, Ordering::SeqCst);
    let parent = waiting_parent(&fx, 240, false, true).await;
    await_unloaded(&parent).await;
    // The exact same assertion must fail with the deliberate strong ref.
    assert!(std::panic::catch_unwind(|| assert_parent_dropped(&fx)).is_err());
    fx.retain.lock().unwrap().take();
    assert_parent_dropped(&fx);
    parent.process.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unloaded_deadline_expires_without_reset_or_child_cancellation() {
    let mut fx = Fixture::new("unload-deadline").await;
    let deadlines = WaitDeadlines::default();
    fx.wait_deadlines = Some(deadlines.clone());
    let parent = waiting_parent(&fx, 2, false, true).await;
    await_unloaded(&parent).await;
    assert_parent_dropped(&fx);
    let all_capacity = parent
        .process
        .server
        .semaphore
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
        .await
        .unwrap();
    let remaining = (parent.checkpoint.deadline - Utc::now())
        .to_std()
        .unwrap_or_default();
    tokio::time::sleep(remaining + Duration::from_millis(150)).await;
    assert_eq!(parent.mock.recorded_calls().len(), 1);
    assert!(deadlines.lock().unwrap().is_empty());
    assert_parent_dropped(&fx); // capacity waiter is also context-free
    drop(all_capacity);
    await_no_processing(&parent.process.server, &parent.sid).await;
    // Observe the deadline passed to the real tool, not the latency of admission,
    // database writes, and cleanup on a loaded runner.
    assert_eq!(
        *deadlines.lock().unwrap(),
        [Some(parent.checkpoint.deadline)],
        "deadline reset on readmission"
    );
    assert!(wait_result(&parent.mock.recorded_calls()[1]).contains("Pending"));
    assert!(parent.process.child_mock.recorded_calls().is_empty());
    assert_eq!(
        checkpoints(&parent.db).await,
        [finished_checkpoint(parent.checkpoint)]
    );
    parent
        .process
        .server
        .registry
        .publish_agent_job(parent.child)
        .await
        .unwrap();
    await_calls(&parent.process.child_mock, 1).await;
    parent.process.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_settles_unloaded_wait_without_readmitting_effects_or_capacity() {
    let fx = Fixture::new("unload-stop").await;
    let parent = waiting_parent(&fx, 240, false, true).await;
    await_unloaded(&parent).await;
    assert_parent_dropped(&fx);
    let all_capacity = parent
        .process
        .server
        .semaphore
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
        .await
        .unwrap();
    let mut client = Session::new(ConversationId(parent.sid.clone()), parent.db.clone()).await;
    assert!(client.request_stop("user").await.unwrap());
    tokio::time::timeout(
        Duration::from_secs(3),
        await_no_processing(&parent.process.server, &parent.sid),
    )
    .await
    .unwrap();
    assert_eq!(parent.mock.recorded_calls().len(), 1);
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    assert!(parent.process.child_mock.recorded_calls().is_empty());
    let session = Session::new(ConversationId(parent.sid.clone()), parent.db.clone()).await;
    let records = session
        .turn_transcript(&parent.checkpoint.attempt_id)
        .await
        .unwrap();
    assert!(matches!(
        records[2].message,
        TurnTranscriptMessage::ToolResult {
            outcome: runtime::ToolResultOutcome::Cancelled,
            ..
        }
    ));
    assert!(matches!(
        records[3].message,
        TurnTranscriptMessage::Cancelled
    ));
    assert!(
        session
            .entries()
            .iter()
            .any(|entry| entry.content == TURN_STOPPED_MESSAGE)
    );
    drop(all_capacity);
    parent
        .process
        .server
        .registry
        .publish_agent_job(parent.child)
        .await
        .unwrap();
    await_calls(&parent.process.child_mock, 1).await;
    for _ in 0..4 {
        parent
            .process
            .server
            .process_session(&parent.sid)
            .await
            .unwrap();
    }
    assert_eq!(
        parent.mock.recorded_calls().len(),
        1,
        "late hints cannot revive the stopped parent"
    );
    parent.process.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_completion_channel_keeps_checkpointed_wait_resident() {
    let fx = Fixture::new("unload-worker-fallback").await;
    let parent = waiting_parent(&fx, 240, true, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !parent
            .process
            .server
            .suspended_waits
            .lock()
            .await
            .contains_key(&parent.sid)
    );
    assert!(fx.sessions.lock().unwrap()[0].upgrade().is_some());
    assert_eq!(
        parent.process.server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
    parent
        .process
        .server
        .registry
        .publish_agent_job(parent.child)
        .await
        .unwrap();
    await_no_processing(&parent.process.server, &parent.sid).await;
    assert!(wait_result(&parent.mock.recorded_calls()[1]).contains("Succeeded"));
    parent.process.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unloaded_wait_reconciles_without_hints_and_matches_restart() {
    let mut outcomes = Vec::new();
    for restart in [false, true] {
        let fx = Fixture::new(if restart {
            "unload-match-restart"
        } else {
            "unload-no-hints"
        })
        .await;
        let parent = waiting_parent(&fx, 240, false, false).await;
        await_unloaded(&parent).await;
        assert_parent_dropped(&fx);
        await_pending_reconciliation(&parent.process.server).await;
        let resumed = if restart {
            parent.process.server.shutdown().await;
            let resumed = fx.start().await;
            await_resumed(&resumed, &parent.checkpoint.attempt_id).await;
            Some(resumed)
        } else {
            None
        };
        crate::session::jobs::reject_job(&parent.child.db, "child refused")
            .await
            .unwrap();
        let (server, mock) = match &resumed {
            Some(process) => (&process.server, &process.child_mock),
            None => (&parent.process.server, &parent.mock),
        };
        await_no_processing(server, &parent.sid).await;
        let calls = mock.recorded_calls();
        let continuation = calls.last().unwrap();
        let text =
            wait_result(continuation).replace(&parent.child.db.root_id().to_string(), "CHILD");
        outcomes.push(text);
        assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
        assert_eq!(
            parent.mock.recorded_calls().len(),
            if restart { 1 } else { 2 }
        );
        assert_eq!(calls.len(), if restart { 1 } else { 2 });
        assert!(
            server
                .interrupted_turns(&parent.sid)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            checkpoints(&parent.db).await,
            [finished_checkpoint(parent.checkpoint)]
        );
        server.shutdown().await;
    }
    assert_eq!(
        outcomes[0], outcomes[1],
        "in-process and restart loaders observe the same result"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_completed_before_suspension_registration_is_not_lost() {
    let fx = Fixture::new("unload-already-ready").await;
    let process = fx.start().await;
    let registry = process.server.registry.clone();
    let (sid, db) = registry
        .create_session(Some("already-ready-parent"))
        .await
        .unwrap();
    registry
        .attach_agent_to_session(&sid.0, &fx.agent)
        .await
        .unwrap();
    let child = process
        .server
        .submit_agent_job(&sid.0, "default", "child task")
        .await
        .unwrap();
    let result = process
        .server
        .wait_job(&child, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(matches!(result.state, JobState::Succeeded { .. }));
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls([
        ("effect-call".into(), "effect".into(), "{}".into()),
        wait_call(&child, 240),
    ]);
    mock.push_text("parent finished");
    process.register_parent(&db, &mock).await;
    Session::new(sid.clone(), db.clone())
        .await
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "observe already completed child".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Directive,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    await_calls(&mock, 2).await;
    await_no_processing(&process.server, &sid.0).await;
    assert_parent_dropped(&fx);
    assert!(wait_result(&mock.recorded_calls()[1]).contains("Succeeded"));
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    assert_eq!(process.child_mock.recorded_calls().len(), 1);
    assert_eq!(job_count(&process.server).await, 1);
    process.server.shutdown().await;
}

/// Publishes a preprepared child from an earlier parent tool call, counting
/// the actual durable submission. A replay would publish a second time.
struct SubmitChild {
    registry: Arc<SessionRegistry>,
    prepared: Mutex<Option<crate::session::PreparedAgentJob>>,
    count: Arc<AtomicUsize>,
}
impl crate::tool::Tool for SubmitChild {
    fn descriptor(&self) -> crate::tool::ToolDescriptor {
        crate::tool::ToolDescriptor {
            name: "submit_child".into(),
            description: "counted child submission".into(),
            parameters: serde_json::json!({"type":"object", "properties":{}}),
        }
    }
    fn execute<'a>(
        &'a self,
        _: serde_json::Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            let child = self
                .prepared
                .lock()
                .await
                .take()
                .ok_or_else(|| "submission replayed".to_string())?;
            self.registry
                .publish_agent_job(child)
                .await
                .map_err(|e| e.to_string())?;
            Ok("submitted child".into())
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unloaded_job_parent_continues_one_branch_without_resubmitting_child() {
    let fx = Fixture::new("unload-job-lineage").await;
    let mock = Arc::new(MockBackend::new());
    let process = fx.start_with(mock.clone()).await;
    let registry = process.server.registry.clone();
    let (root, _) = registry.create_session(Some("root")).await.unwrap();
    registry
        .attach_agent_to_session(&root.0, &fx.agent)
        .await
        .unwrap();
    let parent = registry
        .prepare_agent_job(&root.0, "default", "orchestrate")
        .await
        .unwrap();
    let sid = parent.db.root_id().to_string();
    let db = parent.db.clone();
    let child = registry
        .prepare_agent_job(&sid, "default", "child task")
        .await
        .unwrap();
    let child_id = child.db.root_id().to_string();
    let child_db = child.db.clone();
    let submissions = Arc::new(AtomicUsize::new(0));
    process.server.tools.register(SubmitChild {
        registry: registry.clone(),
        prepared: Mutex::new(Some(child)),
        count: submissions.clone(),
    });
    mock.push_tool_calls([
        ("effect-call".into(), "effect".into(), "{}".into()),
        ("submit-call".into(), "submit_child".into(), "{}".into()),
        wait_call(&child_id, 240),
    ]);
    mock.push_text("child finished");
    mock.push_text("parent finished");
    let parent_gate = mock.block_next_call();
    let child_gate = mock.block_next_call();
    registry.publish_agent_job(parent).await.unwrap();
    parent_gate.wait_started().await;
    parent_gate.release();
    tokio::time::timeout(Duration::from_secs(30), child_gate.wait_started())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !process
            .server
            .suspended_waits
            .lock()
            .await
            .contains_key(&sid)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_parent_dropped(&fx);
    assert_eq!(submissions.load(Ordering::SeqCst), 1);
    let checkpoint = checkpoints(&db).await.pop().unwrap();
    let waits = crate::session::jobs::job_waits(&db).await.unwrap();
    assert_eq!(
        waits.len(),
        1,
        "unloaded wait is still visible to the job monitor"
    );
    assert!(!waits[0].finished);
    child_gate.release();
    process
        .server
        .wait_job(&sid, Duration::from_secs(30))
        .await
        .unwrap();
    await_no_processing(&process.server, &sid).await;
    for _ in 0..4 {
        process.server.process_session(&sid).await.unwrap();
    }
    assert_eq!(submissions.load(Ordering::SeqCst), 1);
    assert_eq!(fx.effects.load(Ordering::SeqCst), 1);
    assert_eq!(mock.recorded_calls().len(), 3);
    assert_eq!(job_count(&process.server).await, 2);
    assert!(matches!(
        process.server.job_status(&child_id).await.unwrap().state,
        JobState::Succeeded { .. }
    ));
    let starts = crate::session::execution::ordered_starts(&db)
        .await
        .unwrap();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[1].start.attempt.attempt_id,
        starts[0].start.attempt.attempt_id
    );
    assert_eq!(starts[1].start.incarnation, starts[0].start.incarnation);
    let continuation = starts[1].start.continuation.as_ref().unwrap();
    assert_eq!(continuation.predecessor, starts[0].start.id);
    assert_eq!(continuation.checkpoint, checkpoint.key());
    assert!(crate::session::jobs::job_waits(&db).await.unwrap()[0].finished);
    assert!(
        crate::session::jobs::read_job_result(&child_db)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        tool_names(&mock.recorded_calls()[0]),
        tool_names(&mock.recorded_calls()[2])
    );
    process.server.shutdown().await;
}
