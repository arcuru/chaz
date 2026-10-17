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
                RuntimeRecord::Steering {
                    model_sequence,
                    inputs,
                } => TurnTranscriptMessage::Steering {
                    model_sequence,
                    inputs,
                },
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
                        identity: None,
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
    context_budget: usize,
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
            context_budget: 200000,
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
                max_context_tokens: self.context_budget,
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
        let executions_before = self.count.load(Ordering::SeqCst);
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
        assert_eq!(self.count.load(Ordering::SeqCst), executions_before + 3);
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
        format!("{:x}", Sha256::digest(&bytes))
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
    async fn pages(&self, reference: &str) -> String {
        let mut offset = 0;
        let mut output = String::new();
        let mut page_count = 0;
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
            page_count += 1;
            if let Some(next) = next {
                assert!(next as usize > offset);
                offset = next as usize;
            } else {
                break;
            }
        }
        assert!(page_count > 1);
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
            eidetica::NewAccount::passwordless("history"),
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
        assert_eq!(fx.pages(&reference).await, raw);
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
    let user = instance.login_account("history", None).await.unwrap();
    let db = user.open_database(&root).await.unwrap();
    let fx = Fixture::new(db, vec![], vec![grant("context_prune", false)]).await;
    assert_eq!(reference, fx.reference("old-0").await);
    assert_eq!(raw, fx.pages(&reference).await);
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
    let all_truncated = fx.pages(&trunc_ref).await;
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
    assert_eq!(raw, fx.pages(&reference).await);
    assert_eq!(hash, fx.hash().await);
    assert_eq!(
        fx.count.load(Ordering::SeqCst),
        0,
        "recovery executed original tools"
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
    for max_bytes in [0, 16385] {
        assert!(
            ToolHistoryQuery
                .execute(
                    json!({"reference":reference,"max_bytes":max_bytes}),
                    &fx.ctx
                )
                .await
                .is_err(),
            "invalid page size {max_bytes} was accepted"
        );
    }
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
    let expected = fx.pages(&reference).await;
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
        json!({"enabled":true,"min_savings_bytes":1000000}),
    )
    .await
    .unwrap();
    let insufficient_savings = text_mock();
    fx.run(insufficient_savings.clone(), None).await.unwrap();
    assert_eq!(
        result(&insufficient_savings.recorded_calls()[0].messages, "old-1"),
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
    fx.seed().await;
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
    // Current activation must attenuate, never replace, the tool scope's ceiling.
    let mut narrowed = fx.ctx.clone();
    narrowed.tools = narrowed
        .tools
        .with_active_extensions(Some(Default::default()));
    assert_eq!(
        narrowed
            .history()
            .await
            .page(&ref0, 0, 4096)
            .await
            .unwrap()
            .status,
        "missing_or_not_authorized"
    );
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
}

#[tokio::test]
async fn review_recovery_refuses_after_last_agent_detached() {
    use crate::session::test_helpers::{make_agent_entry, make_registry};
    let (_instance, registry) = make_registry().await;
    let (_, db) = registry
        .create_session(Some("review-fixture"))
        .await
        .unwrap();
    let session_id = db.root_id().to_string();
    let agent = make_agent_entry(&registry, "test-agent").await;
    registry
        .attach_agent_to_session(&session_id, &agent)
        .await
        .unwrap();
    let mut fx = Fixture::new(db.clone(), vec![], vec![grant("context_prune", false)]).await;
    fx.ctx.agent_identity = Some(agent.clone());
    fx.seed().await;
    let reference = fx.reference("old-0").await;
    let before = ToolHistoryQuery
        .execute(json!({"reference": reference}), &fx.ctx)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&before).unwrap()["status"],
        "retained"
    );
    registry
        .detach_agent_from_session(&session_id, &agent)
        .await
        .unwrap();
    let meta = crate::session::read_meta_from_db(&db).await;
    assert!(meta.agents.is_empty());
    let auth = db
        .get_settings()
        .await
        .unwrap()
        .get_auth_key(&agent.pubkey)
        .await
        .unwrap();
    println!("REVIEW_DETACH_AUTH {auth:?}; legacy={:?}", meta.agent_name);
    let after = ToolHistoryQuery
        .execute(json!({"reference": reference}), &fx.ctx)
        .await;
    match after {
        Err(error) => println!("REVIEW_DETACH_DENIED {error}"),
        Ok(text) => {
            let page: Value = serde_json::from_str(&text).unwrap();
            println!(
                "REVIEW_DETACH_RESULT status={} returned_bytes={} stored_bytes={}",
                page["status"],
                page["content"].as_str().unwrap().len(),
                page["stored_bytes"]
            );
            assert_eq!(
                page["status"], "missing_or_not_authorized",
                "detached agent recovered retained bytes"
            );
        }
    }
}

#[tokio::test]
async fn pruner_recovery_uses_live_write_key_not_stale_roster() {
    use crate::session::test_helpers::{make_agent_entry, make_registry};
    use eidetica::auth::types::{AuthKey, Permission};
    let (_instance, registry) = make_registry().await;
    let (_, db) = registry
        .create_session(Some("membership-fixture"))
        .await
        .unwrap();
    let session_id = db.root_id().to_string();
    let agent = make_agent_entry(&registry, "test-agent").await;
    registry
        .attach_agent_to_session(&session_id, &agent)
        .await
        .unwrap();
    let mut fx = Fixture::new(db.clone(), vec![], vec![grant("context_prune", false)]).await;
    fx.ctx.agent_identity = Some(agent.clone());
    fx.seed().await;
    let reference = fx.reference("old-0").await;
    let scope = fx.ctx.history().await; // captured while authorized
    let stored = fx.hash().await;
    assert_eq!(
        scope.page(&reference, 0, 4096).await.unwrap().status,
        "retained"
    );
    let roster = crate::session::read_meta_from_db(&db).await.agents;
    assert_eq!(roster.len(), 1);
    for key in [
        AuthKey::new(
            Some("agent:test-agent"),
            Permission::Write(10),
            eidetica::auth::types::KeyStatus::Revoked,
        ),
        AuthKey::active(Some("agent:test-agent"), Permission::Read),
        AuthKey::active(Some("agent:test-agent"), Permission::Admin(0)),
    ] {
        let txn = db.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(&agent.pubkey, key)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(crate::session::read_meta_from_db(&db).await.agents, roster);
        assert!(
            scope.page(&reference, 0, 4096).await.is_err(),
            "stale roster bypassed current Active Write membership"
        );
        assert!(
            ToolHistoryQuery
                .execute(json!({"reference":reference}), &fx.ctx)
                .await
                .is_err()
        );
    }
    registry
        .attach_agent_to_session(&session_id, &agent)
        .await
        .unwrap();
    assert_eq!(
        scope.page(&reference, 0, 4096).await.unwrap().status,
        "retained"
    );
    // The cache may lag in either direction; a current Write key is authority.
    crate::session::update_meta_on_db(&db, |m| m.agents.clear())
        .await
        .unwrap();
    assert_eq!(
        scope.page(&reference, 0, 4096).await.unwrap().status,
        "retained"
    );
    let mut unknown = fx.ctx.clone();
    unknown.agent_identity = None;
    assert!(
        unknown
            .history()
            .await
            .page(&reference, 0, 4096)
            .await
            .is_err()
    );
    registry
        .detach_agent_from_session(&session_id, &agent)
        .await
        .unwrap();
    // Even a matching legacy name must not reopen a revoked modern session.
    crate::session::update_meta_on_db(&db, |m| m.agent_name = Some("test-agent".into()))
        .await
        .unwrap();
    assert!(scope.page(&reference, 0, 4096).await.is_err());
    assert_eq!(stored, fx.hash().await);
    assert_eq!(fx.count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn pruner_recovery_preserves_unmarked_and_named_legacy_sessions() {
    let (_instance, mut fx) = Fixture::memory().await;
    fx.seed().await;
    let reference = fx.reference("old-0").await;
    let scope = fx.ctx.history().await;
    assert_eq!(
        scope.page(&reference, 0, 4096).await.unwrap().status,
        "retained"
    );
    crate::session::update_meta_on_db(&fx.db, |m| m.agent_name = Some("test-agent".into()))
        .await
        .unwrap();
    assert_eq!(
        scope.page(&reference, 0, 4096).await.unwrap().status,
        "retained"
    );
    crate::session::update_meta_on_db(&fx.db, |m| m.agent_name = Some("other-agent".into()))
        .await
        .unwrap();
    assert!(scope.page(&reference, 0, 4096).await.is_err());
}

#[tokio::test]
async fn pruner_repeated_call_ids_bind_each_pointer_to_its_source_occurrence() {
    let (_instance, mut fx) = Fixture::memory().await;
    fx.context_budget = 400000;
    fx.seed().await;
    fx.seed().await; // identical calls and outputs in a different retained turn
    let built = fx.assembled().await;
    let scope = fx.ctx.history().await;
    let mut expected = Vec::new();
    for (index, message) in built.messages.iter().enumerate() {
        if let RuntimeMessage::AssistantToolCalls { tool_calls, .. } = message {
            for (offset, made) in tool_calls.iter().enumerate() {
                let raw = scope
                    .selected_result(&built.sources[index + 1 + offset], made)
                    .await
                    .unwrap()
                    .unwrap();
                expected.push((index + 1 + offset, raw.reference));
            }
        }
    }
    assert_eq!(expected.len(), 6);
    assert_eq!(
        expected
            .iter()
            .map(|(_, r)| r)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        6
    );
    let stored = fx.hash().await;
    fx.settings(true).await;
    let mock = text_mock();
    fx.run(mock.clone(), None).await.unwrap();
    let sent = &mock.recorded_calls()[0].messages;
    for (position, reference) in expected {
        let RuntimeMessage::ToolResult { content, .. } = &sent[position] else {
            panic!("missing result");
        };
        assert!(
            content.contains(&reference),
            "pointer resolved another occurrence with the same provider call ID"
        );
    }
    assert_eq!(stored, fx.hash().await);
    assert_eq!(fx.count.load(Ordering::SeqCst), 6);
}
