use super::*;
use crate::{
    backends::BackendManager,
    config::{Backend, BackendType, ContextConfig},
    context::{AssembledContext, ContextBuilder},
    extension::{
        ExtensionEvent, ExtensionHub, append_event,
        projection::{ContextProjectionGrant, ProjectionAuthority},
        write_settings,
    },
    runtime::{self, LLMResponse, ModelCallScope, RuntimeRecord, RuntimeRecorder, ToolCallRequest},
    session::{
        EntryType, Session, SessionCommand, SessionCommandOutcome, SessionCommandRequest,
        SessionEntry, TurnAttempt, TurnRequestId, TurnTranscriptMessage, TurnTranscriptRecord,
    },
    test_support::*,
    tool::{ScopedTools, ToolPolicyRegistry, ToolRegistry},
    types::ConversationId,
};
use chrono::Utc;
use eidetica::{
    Database, Instance,
    store::{DocStore, Table},
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;

fn capture(name: &str, value: &Value) {
    if let Ok(dir) = std::env::var("CHAZ_PRUNER_CAPTURE_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join(format!("{name}.json")),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }
}
fn entry(sender: &str, text: &str) -> SessionEntry {
    SessionEntry {
        sender: sender.into(),
        content: text.into(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    }
}
struct Inert {
    name: &'static str,
    output: String,
    count: Arc<AtomicUsize>,
}
impl Tool for Inert {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.into(),
            description: "Inert fixture".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(self.output.clone()) })
    }
}
struct Recorder {
    session: Arc<Mutex<Session>>,
    attempt: TurnAttempt,
    sequence: AtomicUsize,
}
impl RuntimeRecorder for Recorder {
    fn record<'a>(
        &'a self,
        message: RuntimeRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let message = match message {
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
                        sequence: self.sequence.fetch_add(1, Ordering::SeqCst) as u64,
                        timestamp: Utc::now(),
                        message,
                    },
                    vec![],
                )
                .await
                .map_err(|e| e.to_string())
        })
    }
}
struct Fixture {
    hub: Arc<ExtensionHub>,
    ctx: ToolContext,
    registry: Arc<ToolRegistry>,
    count: Arc<AtomicUsize>,
    db: Database,
}
impl Fixture {
    async fn new(
        db: Database,
        extra: Vec<Arc<dyn Extension>>,
        grants: Vec<ContextProjectionGrant>,
    ) -> Self {
        let mut hub = context_hub().await;
        let mut exts: Vec<Arc<dyn Extension>> = vec![Arc::new(ContextPruner)];
        exts.extend(extra);
        hub.install_all(exts).await.unwrap();
        hub.set_context_projection_grants(&grants).unwrap();
        hub.record_active(&db).await.unwrap();
        let registry = Arc::new(ToolRegistry::new());
        for (owner, _, tool) in hub.tools_for_registry() {
            registry.register_arc_owned(tool, Some(owner));
        }
        let count = Arc::new(AtomicUsize::new(0));
        for (name, output) in [
            ("inert", "é<inert>&\n".repeat(6000)),
            ("truncated", "inert-data ".repeat(12000)),
        ] {
            registry.register_arc_owned(
                Arc::new(Inert {
                    name,
                    output,
                    count: count.clone(),
                }),
                Some("context_prune"),
            );
        }
        let session = Arc::new(Mutex::new(
            Session::new(ConversationId(db.root_id().to_string()), db.clone()).await,
        ));
        let mut ctx = tool_context(session, registry.clone());
        ctx.active_extensions = crate::extension::read_active(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name().into())
            .collect();
        Self {
            hub: Arc::new(hub),
            ctx,
            registry,
            count,
            db,
        }
    }
    async fn memory() -> (Instance, Self) {
        let (instance, session) = fresh_session().await;
        let db = session.lock().await.database().clone();
        (
            instance,
            Self::new(db, vec![], vec![grant("context_prune", false)]).await,
        )
    }
    async fn settings(&self, enabled: bool) {
        write_settings(&self.db, "context_prune", json!({"enabled":enabled}))
            .await
            .unwrap();
    }
    async fn assembled(&self) -> AssembledContext {
        let session = self.ctx.session.lock().await;
        let view = session
            .context_view_at(self.db.snapshot().await.unwrap())
            .await
            .unwrap();
        ContextBuilder::new(
            &view.entries,
            "test-agent",
            "fixture instructions",
            &ContextConfig {
                max_context_tokens: 200000,
                reserved_output_tokens: 256,
            },
        )
        .with_context_view(&view)
        .with_tools(&self.ctx.definitions())
        .with_extension_hub(self.hub.clone())
        .with_session_db(&self.db)
        .with_invocation(
            &self.ctx.active_extensions,
            self.ctx.turn_request_id.as_ref().map(|r| r.as_str()),
        )
        .try_build()
        .await
        .unwrap()
    }
    async fn run(
        &self,
        mock: Arc<MockBackend>,
        budget: Option<usize>,
    ) -> Result<runtime::RuntimeOutcome, String> {
        let built = self.assembled().await;
        let manager = BackendManager::with_mock(mock, empty_secrets().await);
        runtime::execute_scoped(
            None,
            built.messages,
            &manager,
            &permissive_security(),
            &self.ctx,
            &ToolPolicyRegistry::empty(),
            None,
            Some(&self.hub),
            ModelCallScope {
                context_strategy: built.strategy,
                cache: built.cache,
                sources: built.sources,
                request_budget_tokens: Some(budget.unwrap_or(built.request_budget_tokens)),
                ..Default::default()
            },
        )
        .await
    }
    async fn seed(&mut self) -> TurnAttempt {
        let request = self
            .ctx
            .session
            .lock()
            .await
            .add_entry(entry("human", "record inert evidence"))
            .await
            .unwrap();
        let attempt = self
            .ctx
            .session
            .lock()
            .await
            .start_turn_attempt(request.clone())
            .await
            .unwrap();
        self.ctx.turn_request_id = Some(request);
        let mock = Arc::new(MockBackend::new());
        mock.push_response(Ok(LLMResponse::ToolCalls {
            content: Some("opaque call frame".into()),
            tool_calls: vec![
                ToolCallRequest {
                    id: "old-0".into(),
                    name: "inert".into(),
                    arguments: "{\"keep\":\"args\"}".into(),
                },
                ToolCallRequest {
                    id: "old-1".into(),
                    name: "truncated".into(),
                    arguments: "{}".into(),
                },
            ],
            provider_extra:
                json!({"reasoning_details":[{"type":"encrypted","data":"opaque-untouched"}]})
                    .as_object()
                    .unwrap()
                    .clone(),
            metadata: None,
        }));
        mock.push_tool_calls([("old-2".into(), "inert".into(), "{}".into())]);
        mock.push_text("recorded");
        let built = self.assembled().await;
        let manager = BackendManager::with_mock(mock.clone(), empty_secrets().await);
        let recorder = Arc::new(Recorder {
            session: self.ctx.session.clone(),
            attempt: attempt.clone(),
            sequence: AtomicUsize::new(0),
        });
        runtime::execute_with_recorder(
            None,
            built.messages,
            &manager,
            &permissive_security(),
            &self.ctx,
            &ToolPolicyRegistry::empty(),
            Some(recorder),
            Some(&self.hub),
            None,
            ModelCallScope {
                context_strategy: built.strategy,
                sources: built.sources,
                cache: built.cache,
                attempt_id: Some(attempt.attempt_id.clone()),
                request_budget_tokens: Some(built.request_budget_tokens),
            },
        )
        .await
        .unwrap();
        self.ctx
            .session
            .lock()
            .await
            .complete_turn_attempt(&attempt, Some(entry("test-agent", "recorded")))
            .await
            .unwrap();
        self.ctx.turn_request_id = Some(
            self.ctx
                .session
                .lock()
                .await
                .add_entry(entry("human", "current input"))
                .await
                .unwrap(),
        );
        assert_eq!(self.count.load(Ordering::SeqCst), 3);
        attempt
    }
    async fn hash(&self) -> String {
        let txn = self.db.new_transaction().await.unwrap();
        let mut rows = txn
            .get_store::<Table<TurnTranscriptRecord>>("turn_transcript")
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let bytes = serde_json::to_vec(&rows).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        if let Ok(dir) = std::env::var("CHAZ_PRUNER_CAPTURE_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                std::path::Path::new(&dir).join(format!("retained-records-{hash}.json")),
                bytes,
            )
            .unwrap();
        }
        hash
    }
    async fn reference(&self, call_id: &str) -> String {
        let built = self.assembled().await;
        for (message, source) in built.messages.iter().zip(&built.sources) {
            if let RuntimeMessage::AssistantToolCalls { tool_calls, .. } = message
                && let Some(call) = tool_calls.iter().find(|c| c.id == call_id)
            {
                return self
                    .ctx
                    .history()
                    .await
                    .selected_result(source, call)
                    .await
                    .unwrap()
                    .unwrap()
                    .reference;
            }
        }
        panic!("fixture call missing")
    }
    async fn pages(&self, reference: &str, label: &str) -> String {
        let mut offset = 0;
        let mut output = String::new();
        let mut pages = vec![];
        loop {
            let page: Value = serde_json::from_str(
                &ToolHistoryQuery
                    .execute(
                        json!({"reference":reference,"offset":offset,"max_bytes":4096}),
                        &self.ctx,
                    )
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert!(page["content"].as_str().unwrap().len() <= 4096);
            output.push_str(page["content"].as_str().unwrap());
            let next = page["next_offset"].as_u64();
            pages.push(page);
            if let Some(next) = next {
                assert!(next as usize > offset);
                offset = next as usize;
            } else {
                break;
            }
        }
        assert!(pages.len() > 1);
        capture(label, &json!(pages));
        output
    }
}
fn grant(name: &str, required: bool) -> ContextProjectionGrant {
    ContextProjectionGrant {
        extension: name.into(),
        authority: ProjectionAuthority::Conversation,
        required,
    }
}
fn text_mock() -> Arc<MockBackend> {
    let mock = Arc::new(MockBackend::new());
    mock.push_text("ok");
    mock
}
fn result(messages: &[RuntimeMessage], id: &str) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            RuntimeMessage::ToolResult { call_id, content } if call_id == id => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn pruner_recorded_consumer_shrinks_preserves_frames_and_active_exchanges() {
    let (_instance, mut fx) = Fixture::memory().await;
    fx.seed().await;
    let before = fx.hash().await;
    let baseline = fx.assembled().await;
    fx.settings(true).await;
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls([("current-0".into(), "inert".into(), "{}".into())]);
    mock.push_text("done");
    fx.run(mock.clone(), None).await.unwrap();
    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 2);
    for (old, new) in baseline.messages.iter().zip(&calls[0].messages) {
        if let RuntimeMessage::ToolResult { call_id, .. } = old {
            assert!(result(&calls[0].messages, call_id).contains("tool_history_query"));
        } else {
            assert_eq!(old, new);
        }
    }
    assert_eq!(calls[0].messages.len(), baseline.messages.len());
    assert!(
        format!("{:?}", calls[0].messages).len() < format!("{:?}", baseline.messages).len() / 10
    );
    assert!(result(&calls[1].messages, "current-0").contains("é&lt;inert&gt;&"));
    assert!(!result(&calls[1].messages, "current-0").contains("Retained tool result omitted"));
    assert_eq!(
        &calls[1].messages[..calls[0].messages.len()],
        calls[0].messages.as_slice()
    );
    assert_eq!(before, fx.hash().await);
    capture(
        "runtime-active-loop",
        &json!({"before_hash":before,"after_hash":fx.hash().await,"baseline":format!("{:?}",baseline.messages),"calls":format!("{calls:?}")}),
    );
    println!(
        "consumer: {} -> {} debug bytes; retained hash {}",
        format!("{:?}", baseline.messages).len(),
        format!("{:?}", calls[0].messages).len(),
        before
    );
}

#[tokio::test]
async fn pruner_toggle_stability_retry_no_tool_and_over_budget_reinflation() {
    let (_instance, mut fx) = Fixture::memory().await;
    fx.seed().await;
    let before = fx.hash().await;
    let plain = text_mock();
    fx.run(plain.clone(), None).await.unwrap();
    fx.settings(true).await;
    let pruned = text_mock();
    fx.run(pruned.clone(), None).await.unwrap();
    let messages = pruned.recorded_calls()[0].messages.clone();
    let budget = crate::context::estimate_request_tokens(&messages, &fx.ctx.definitions()) + 100;
    fx.settings(false).await;
    let disabled = text_mock();
    fx.run(disabled.clone(), None).await.unwrap();
    assert_eq!(
        disabled.recorded_calls()[0].messages,
        plain.recorded_calls()[0].messages
    );
    let oversize = text_mock();
    assert!(
        fx.run(oversize.clone(), Some(budget))
            .await
            .unwrap_err()
            .contains("over the")
    );
    assert!(oversize.recorded_calls().is_empty());
    fx.settings(true).await;
    let again = text_mock();
    fx.run(again.clone(), Some(budget)).await.unwrap();
    assert_eq!(again.recorded_calls()[0].messages, messages);
    let retry = Arc::new(MockBackend::new());
    retry.push_err(crate::error::LlmError::ServerError {
        status: 503,
        message: "inert retry".into(),
    });
    retry.push_text("ok");
    fx.run(retry.clone(), None).await.unwrap();
    assert_eq!(
        retry.recorded_calls()[0].messages,
        retry.recorded_calls()[1].messages
    );
    let no_tools = Arc::new(MockBackend::new().with_supports_tools(false));
    no_tools.push_text("ok");
    fx.run(no_tools.clone(), None).await.unwrap();
    assert_eq!(
        no_tools.recorded_calls()[0].messages,
        plain.recorded_calls()[0].messages
    );
    assert_eq!(before, fx.hash().await);
    capture(
        "toggle-retry-budget",
        &json!({"hash":before, "budget":budget,"disabled":format!("{:?}",disabled.recorded_calls()),"reenabled":format!("{:?}",again.recorded_calls()),"retry":format!("{:?}",retry.recorded_calls()),"no_tools":format!("{:?}",no_tools.recorded_calls())}),
    );
}

#[tokio::test]
async fn pruner_exact_pages_true_sqlite_reopen_and_native_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("history.db").display()
    );
    let (root, reference, trunc_ref, hash, projected, raw) = {
        let (_instance, mut user) = Instance::create_backend(
            Box::new(
                eidetica::backend::database::Sqlite::connect(&url)
                    .await
                    .unwrap(),
            ),
            eidetica::NewUser::passwordless("history"),
        )
        .await
        .unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &user.get_default_key().unwrap())
            .await
            .unwrap();
        let mut fx = Fixture::new(db.clone(), vec![], vec![grant("context_prune", false)]).await;
        let attempt = fx.seed().await;
        let reference = fx.reference("old-0").await;
        let trunc_ref = fx.reference("old-1").await;
        let records = fx
            .ctx
            .session
            .lock()
            .await
            .turn_transcript(&attempt.attempt_id)
            .await
            .unwrap();
        let raw = records
            .iter()
            .find_map(|r| match &r.message {
                TurnTranscriptMessage::ToolResult {
                    call_id, output, ..
                } if call_id == "old-0" => Some(output.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(fx.pages(&reference, "pages-before-reopen").await, raw);
        fx.settings(true).await;
        let mock = text_mock();
        fx.run(mock.clone(), None).await.unwrap();
        (
            db.root_id().clone(),
            reference,
            trunc_ref,
            fx.hash().await,
            mock.recorded_calls()[0].messages.clone(),
            raw,
        )
        // All instance/user/db/session/tool/extension handles leave this scope.
    };
    let instance = Instance::connect(&url).await.unwrap();
    let user = instance.login_user("history", None).await.unwrap();
    let db = user.open_database(&root).await.unwrap();
    let fx = Fixture::new(db, vec![], vec![grant("context_prune", false)]).await;
    assert_eq!(reference, fx.reference("old-0").await);
    assert_eq!(raw, fx.pages(&reference, "pages-after-reopen").await);
    let mock = text_mock();
    fx.run(mock.clone(), None).await.unwrap();
    assert_eq!(mock.recorded_calls()[0].messages, projected);
    let trunc: Value = serde_json::from_str(
        &ToolHistoryQuery
            .execute(json!({"reference":trunc_ref}), &fx.ctx)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(trunc["stored_truncation_marker"], true);
    assert_eq!(trunc["stored_bytes"], 102400);
    let all_truncated = fx.pages(&trunc_ref, "truncated-pages").await;
    assert_eq!(all_truncated.len(), 102400);
    assert!(all_truncated.ends_with("[tool output truncated at 100 KB]"));
    let command_id = TurnRequestId::parse("fixture-compact");
    {
        let session = fx.ctx.session.lock().await;
        session
            .submit_command(SessionCommandRequest {
                command_id: command_id.clone(),
                sender: "human".into(),
                created_at: Utc::now(),
                command: SessionCommand::Compact {
                    source_snapshot: fx.db.snapshot().await.unwrap(),
                },
            })
            .await
            .unwrap();
        let attempt = session.start_turn_attempt(command_id).await.unwrap();
        session
            .complete_command_attempt(
                &attempt,
                SessionCommandOutcome::Compact {
                    summary: format!("retained evidence {reference}"),
                },
            )
            .await
            .unwrap();
    }
    let compact = fx.assembled().await;
    assert!(
        !compact
            .messages
            .iter()
            .any(|m| matches!(m, RuntimeMessage::ToolResult { .. }))
    );
    assert_eq!(raw, fx.pages(&reference, "pages-after-compaction").await);
    assert_eq!(hash, fx.hash().await);
    assert_eq!(
        fx.count.load(Ordering::SeqCst),
        0,
        "recovery executed original tools"
    );
    capture(
        "reopen-compaction-hashes",
        &json!({"before":hash,"after":fx.hash().await,"reference":reference,"compacted":format!("{:?}",compact.messages)}),
    );
}

#[tokio::test]
async fn pruner_recovery_denies_foreign_forged_revoked_and_missing_sources() {
    let (_instance, mut fx) = Fixture::memory().await;
    fx.seed().await;
    let reference = fx.reference("old-0").await;
    let (_foreign, foreign) = Fixture::memory().await;
    let missing = |s: String| {
        serde_json::from_str::<Value>(&s).unwrap()["status"] == "missing_or_not_authorized"
    };
    assert!(missing(
        ToolHistoryQuery
            .execute(json!({"reference":reference}), &foreign.ctx)
            .await
            .unwrap()
    ));
    assert!(missing(
        ToolHistoryQuery
            .execute(
                json!({"reference":format!("tr1:{}","0".repeat(64))}),
                &fx.ctx
            )
            .await
            .unwrap()
    ));
    assert!(
        ToolHistoryQuery
            .execute(json!({"reference":"not a ref"}), &fx.ctx)
            .await
            .is_err()
    );
    assert!(
        ToolHistoryQuery
            .execute(json!({"reference":reference,"max_bytes":16385}), &fx.ctx)
            .await
            .is_err()
    );
    assert!(
        ToolHistoryQuery
            .execute(json!({"reference":reference,"offset":1}), &fx.ctx)
            .await
            .is_err()
    );
    let mut revoked = fx.ctx.clone();
    revoked.tools = ScopedTools::new(fx.registry.clone(), Some(vec![RECOVERY_TOOL.into()]));
    assert!(missing(
        ToolHistoryQuery
            .execute(json!({"reference":reference}), &revoked)
            .await
            .unwrap()
    ));
    revoked.tools = ScopedTools::new(fx.registry.clone(), Some(vec!["inert".into()]));
    assert!(
        ToolHistoryQuery
            .execute(json!({"reference":reference}), &revoked)
            .await
            .is_err()
    );
    fx.settings(false).await;
    assert!(
        ToolHistoryQuery
            .execute(json!({"reference":reference}), &fx.ctx)
            .await
            .unwrap()
            .contains("retained")
    );
    append_event(
        &fx.db,
        ExtensionEvent::Deactivated {
            name: "context_prune".into(),
            timestamp: Utc::now() + chrono::Duration::seconds(1),
        },
    )
    .await
    .unwrap();
    assert!(
        ToolHistoryQuery
            .execute(json!({"reference":reference}), &fx.ctx)
            .await
            .is_err()
    );
    assert_eq!(fx.count.load(Ordering::SeqCst), 3);
}

struct Tag(&'static str);
impl Extension for Tag {
    fn name(&self) -> &'static str {
        self.0
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
    fn manifest(&self) -> ExtensionManifest {
        let mut m = ContextPruner.manifest();
        m.name = self.0.into();
        m.extension_ref = ExtensionRef::builtin(self.0);
        m.supported_hooks = vec![];
        m
    }
    fn instantiate<'a>(
        &'a self,
        _: ScopeCtx<'a>,
    ) -> crate::extension::instance::InstantiateFuture<'a> {
        Box::pin(async {
            Ok(Arc::new(TagInstance {
                manifest: self.manifest(),
                tag: self.0,
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}
struct TagInstance {
    manifest: ExtensionManifest,
    tag: &'static str,
}
impl ExtensionInstance for TagInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
    fn context_projector(&self) -> Option<Arc<dyn ContextProjector>> {
        Some(Arc::new(Tag(self.tag)))
    }
}
impl ContextProjector for Tag {
    fn project<'a>(
        &'a self,
        _: &'a ProjectionCall<'a>,
        mut request: ProjectedRequest,
    ) -> CapFuture<'a, ProjectedRequest> {
        Box::pin(async move {
            if self.0 == "required-edit" {
                for message in &mut request.messages {
                    if let RuntimeMessage::ToolResult { call_id, content } = message
                        && call_id == "old-0"
                    {
                        content.push_str("required result edit");
                    }
                }
            } else {
                request.messages.push(RuntimeMessage::User(self.0.into()));
            }
            Ok(request)
        })
    }
}
#[tokio::test]
async fn pruner_optional_source_and_settings_failure_keep_all_required_transforms() {
    for failure in ["source", "settings"] {
        let (_instance, session) = fresh_session().await;
        let db = session.lock().await.database().clone();
        let mut fx = Fixture::new(
            db,
            vec![
                Arc::new(Tag("required-before")),
                Arc::new(Tag("required-after")),
            ],
            vec![
                grant("required-before", true),
                grant("context_prune", false),
                grant("required-after", true),
            ],
        )
        .await;
        fx.seed().await;
        fx.settings(true).await;
        let built = fx.assembled().await;
        let txn = fx.db.new_transaction().await.unwrap();
        if failure == "source" {
            txn.get_store::<Table<Value>>("turn_transcript")
                .await
                .unwrap()
                .set("corrupt", json!({"invalid":"injected source read failure"}))
                .await
                .unwrap();
        } else {
            txn.get_store::<DocStore>("extension_settings")
                .await
                .unwrap()
                .set_string("context_prune", "invalid-json")
                .await
                .unwrap();
        }
        txn.commit().await.unwrap();
        let mock = text_mock();
        let manager = BackendManager::with_mock(mock.clone(), empty_secrets().await);
        runtime::execute_scoped(
            None,
            built.messages.clone(),
            &manager,
            &permissive_security(),
            &fx.ctx,
            &ToolPolicyRegistry::empty(),
            None,
            Some(&fx.hub),
            ModelCallScope {
                cache: built.cache,
                context_strategy: built.strategy,
                sources: built.sources,
                request_budget_tokens: Some(built.request_budget_tokens),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let sent = mock.recorded_calls()[0].messages.clone();
        assert_eq!(&sent[..built.messages.len()], built.messages.as_slice());
        assert_eq!(
            &sent[built.messages.len()..],
            &[
                RuntimeMessage::User("required-before".into()),
                RuntimeMessage::User("required-after".into())
            ]
        );
        capture(
            &format!("optional-{failure}-failure"),
            &json!({"sent":format!("{sent:?}")}),
        );
    }
}

#[tokio::test]
async fn pruner_provider_fixture_captures_actual_openai_request() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let (_instance, mut fx) = Fixture::memory().await;
    fx.seed().await;
    let hash = fx.hash().await;
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"inert-provider","object":"chat.completion","created":0,"model":"fixture-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]
    }))).mount(&server).await;
    let mut backend = Backend::new(BackendType::OpenAICompatible);
    backend.name = Some("fixture".into());
    backend.api_base = Some(format!("{}/v1", server.uri()));
    backend.api_key = Some("inert-fixture-key".into());
    let manager = BackendManager::new(&Some(vec![backend]), empty_secrets().await);
    for enabled in [false, true] {
        fx.settings(enabled).await;
        let built = fx.assembled().await;
        runtime::execute_scoped(
            Some("fixture-model"),
            built.messages,
            &manager,
            &permissive_security(),
            &fx.ctx,
            &ToolPolicyRegistry::empty(),
            None,
            Some(&fx.hub),
            ModelCallScope {
                cache: built.cache,
                context_strategy: built.strategy,
                sources: built.sources,
                request_budget_tokens: Some(built.request_budget_tokens),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let bodies: Vec<Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    capture(
        "provider-requests",
        &json!({"baseline":bodies[0],"pruned":bodies[1],"stored_hash":hash,"request_bytes":[requests[0].body.len(),requests[1].body.len()]}),
    );
    println!(
        "provider captured bytes {} -> {}; retained hash {}",
        requests[0].body.len(),
        requests[1].body.len(),
        hash
    );
    assert!(requests[1].body.len() < requests[0].body.len() / 10);
    for (old, new) in bodies[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .zip(bodies[1]["messages"].as_array().unwrap())
    {
        if old["role"] == "tool" {
            assert_eq!(old["tool_call_id"], new["tool_call_id"]);
            assert_eq!(old["role"], new["role"]);
            assert!(
                new["content"]
                    .as_str()
                    .unwrap()
                    .contains("tool_history_query")
            );
        } else {
            assert_eq!(old, new);
        }
    }
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
    assert_eq!(hash, fx.hash().await);
}

#[tokio::test]
async fn pruner_runtime_recovery_pages_remain_raw_in_current_and_historical_exchanges() {
    let (_instance, mut fx) = Fixture::memory().await;
    fx.seed().await;
    fx.settings(true).await;
    let reference = fx.reference("old-0").await;
    let expected = fx.pages(&reference, "runtime-recovery-expected").await;
    let mock = Arc::new(MockBackend::new());
    let mut offset = 0;
    let mut queries = vec![];
    loop {
        let page = fx
            .ctx
            .history()
            .await
            .page(&reference, offset, 16384)
            .await
            .unwrap();
        queries.push((
            format!("recovery-{}", queries.len()),
            RECOVERY_TOOL.into(),
            json!({"reference":reference,"offset":offset}).to_string(),
        ));
        if let Some(next) = page.next_offset {
            offset = next;
        } else {
            break;
        }
    }
    mock.push_tool_calls(queries);
    mock.push_text("recovered");
    let request = fx.ctx.turn_request_id.clone().unwrap();
    let attempt = fx
        .ctx
        .session
        .lock()
        .await
        .start_turn_attempt(request)
        .await
        .unwrap();
    let recorder = Arc::new(Recorder {
        session: fx.ctx.session.clone(),
        attempt: attempt.clone(),
        sequence: AtomicUsize::new(0),
    });
    let built = fx.assembled().await;
    let manager = BackendManager::with_mock(mock.clone(), empty_secrets().await);
    runtime::execute_with_recorder(
        None,
        built.messages,
        &manager,
        &permissive_security(),
        &fx.ctx,
        &ToolPolicyRegistry::empty(),
        Some(recorder),
        Some(&fx.hub),
        None,
        ModelCallScope {
            cache: built.cache,
            context_strategy: built.strategy,
            sources: built.sources,
            request_budget_tokens: Some(built.request_budget_tokens),
            attempt_id: Some(attempt.attempt_id.clone()),
        },
    )
    .await
    .unwrap();
    let records = fx
        .ctx
        .session
        .lock()
        .await
        .turn_transcript(&attempt.attempt_id)
        .await
        .unwrap();
    let pages: Vec<Value> = records
        .iter()
        .filter_map(|r| match &r.message {
            TurnTranscriptMessage::ToolResult { name, output, .. } if name == RECOVERY_TOOL => {
                Some(serde_json::from_str(output).unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        pages
            .iter()
            .map(|p| p["content"].as_str().unwrap())
            .collect::<String>(),
        expected
    );
    let current = mock.recorded_calls()[1].messages.clone();
    for i in 0..pages.len() {
        assert!(
            !result(&current, &format!("recovery-{i}")).contains("Retained tool result omitted")
        );
    }
    fx.ctx
        .session
        .lock()
        .await
        .complete_turn_attempt(&attempt, Some(entry("test-agent", "recovered")))
        .await
        .unwrap();
    fx.ctx.turn_request_id = Some(
        fx.ctx
            .session
            .lock()
            .await
            .add_entry(entry("human", "next input"))
            .await
            .unwrap(),
    );
    let next = text_mock();
    fx.run(next.clone(), None).await.unwrap();
    for i in 0..pages.len() {
        assert_eq!(
            result(&current, &format!("recovery-{i}")),
            result(&next.recorded_calls()[0].messages, &format!("recovery-{i}"))
        );
    }
    assert_eq!(
        fx.count.load(Ordering::SeqCst),
        3,
        "recovery re-executed source tools"
    );
    capture(
        "runtime-recovery-pages",
        &json!({"pages":pages,"calls":format!("{:?}",mock.recorded_calls()),"historical":format!("{:?}",next.recorded_calls())}),
    );
}

#[tokio::test]
async fn pruner_required_body_edit_exclusions_thresholds_and_current_membership() {
    let (_instance, session) = fresh_session().await;
    let db = session.lock().await.database().clone();
    let mut fx = Fixture::new(
        db,
        vec![Arc::new(Tag("required-edit"))],
        vec![grant("required-edit", true), grant("context_prune", false)],
    )
    .await;
    fx.seed().await;
    let baseline = fx.assembled().await;
    fx.settings(true).await;
    let mock = text_mock();
    fx.run(mock.clone(), None).await.unwrap();
    assert_eq!(
        result(&mock.recorded_calls()[0].messages, "old-0"),
        format!(
            "{}required result edit",
            result(&baseline.messages, "old-0")
        )
    );
    assert!(
        result(&mock.recorded_calls()[0].messages, "old-1")
            .contains("Retained tool result omitted")
    );
    write_settings(
        &fx.db,
        "context_prune",
        json!({"enabled":true,"min_bytes":1000000}),
    )
    .await
    .unwrap();
    let high = text_mock();
    fx.run(high.clone(), None).await.unwrap();
    assert_eq!(
        result(&high.recorded_calls()[0].messages, "old-1"),
        result(&baseline.messages, "old-1")
    );
    write_settings(
        &fx.db,
        "context_prune",
        json!({"enabled":true,"exclude_tools":["truncated"]}),
    )
    .await
    .unwrap();
    let excluded = text_mock();
    fx.run(excluded.clone(), None).await.unwrap();
    assert_eq!(
        result(&excluded.recorded_calls()[0].messages, "old-1"),
        result(&baseline.messages, "old-1")
    );
    let reference = fx.reference("old-0").await;
    crate::session::update_meta_on_db(&fx.db, |m| m.agent_name = Some("other-agent".into()))
        .await
        .unwrap();
    assert!(
        ToolHistoryQuery
            .execute(json!({"reference":reference}), &fx.ctx)
            .await
            .unwrap_err()
            .to_string()
            .contains("session participant")
    );
    assert_eq!(fx.count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn pruner_scoped_sources_ungranted_projection_and_incomplete_history() {
    let (_instance, mut fx) = Fixture::memory().await;
    let attempt = fx.seed().await;
    let built = fx.assembled().await;
    let ref0 = fx.reference("old-0").await;
    let ref2 = fx.reference("old-2").await;
    let selected: Vec<_> = built
        .messages
        .iter()
        .zip(&built.sources)
        .filter(|(_, s)| {
            matches!(
                s,
                ContextSource::ReplayedExchange {
                    model_sequence: 0,
                    ..
                }
            )
        })
        .collect();
    let baseline = ProjectedRequest {
        messages: selected.iter().map(|(m, _)| (*m).clone()).collect(),
        tools: fx.ctx.definitions(),
        cache: Default::default(),
    };
    let sources: Vec<_> = selected.iter().map(|(_, s)| (*s).clone()).collect();
    let scope = fx.ctx.history().await.for_projection(&baseline, &sources);
    assert_eq!(scope.page(&ref0, 0, 4096).await.unwrap().status, "retained");
    assert_eq!(
        scope.page(&ref2, 0, 4096).await.unwrap().status,
        "missing_or_not_authorized"
    );
    let RuntimeMessage::AssistantToolCalls { tool_calls, .. } = &baseline.messages[0] else {
        panic!("missing group");
    };
    let mut forged = tool_calls[0].clone();
    forged.arguments = "{\"forged\":true}".into();
    assert!(
        scope
            .selected_result(&sources[0], &forged)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        scope
            .selected_result(
                &ContextSource::CurrentAttempt { model_sequence: 0 },
                &tool_calls[0]
            )
            .await
            .unwrap()
            .is_none()
    );
    fx.settings(true).await;
    Arc::get_mut(&mut fx.hub)
        .unwrap()
        .set_context_projection_grants(&[])
        .unwrap();
    let mock = text_mock();
    fx.run(mock.clone(), None).await.unwrap();
    assert_eq!(mock.recorded_calls()[0].messages, built.messages);
    let txn = fx.db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<Table<TurnTranscriptRecord>>("turn_transcript")
        .await
        .unwrap();
    let (key,_) = store.search(|r| matches!(&r.message,TurnTranscriptMessage::ToolResult { call_id,.. } if call_id=="old-0")).await.unwrap().pop().unwrap();
    store.delete(&key).await.unwrap();
    txn.commit().await.unwrap();
    // Simulate a missing retained source in a disposable database, never real state.
    let missing = fx.ctx.history().await.page(&ref0, 0, 4096).await.unwrap();
    assert_eq!(missing.status, "missing_or_not_authorized");
    let history = fx
        .ctx
        .session
        .lock()
        .await
        .context_with_tool_history()
        .await
        .unwrap()
        .1;
    // Completed status alone cannot turn an incomplete exchange into a pointer.
    assert!(!history.is_empty());
    let mock = text_mock();
    fx.run(mock.clone(), None).await.unwrap();
    assert!(
        !mock.recorded_calls()[0]
            .messages
            .iter()
            .any(|m| matches!(m,RuntimeMessage::ToolResult { call_id,.. } if call_id=="old-0"))
    );
    assert_eq!(fx.count.load(Ordering::SeqCst), 3);
    capture(
        "source-scope-missing",
        &json!({"missing":missing,"attempt":attempt.attempt_id}),
    );
}
