use super::*;
use crate::{
    backends::BackendManager,
    cache::{CacheAnchor, CacheOptions, CacheTtl},
    config::ContextConfig,
    context::{AssembledContext, ContextBuilder},
    extension::{
        Extension, ExtensionInstance, ExtensionRef, HookKind, Scope, ScopeCtx,
        caps::{CapabilityKind, ContextTail, ContextTailCall},
        durable_context::{
            ContextContribution, DurableContextCall, DurableContextContributor, DurableContextGrant,
        },
        manifest::ExtensionManifest,
    },
    runtime::{self, ModelCallScope, RuntimeMessage},
    session::{EntryType, Session, SessionEntry, TurnRequestId},
    test_support::*,
    tool::{ToolPolicyRegistry, ToolRegistry},
};
use chrono::{TimeZone, Utc};
use eidetica::store::{DocStore, Table};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Synthetic {
    mode: &'static str,
    selections: AtomicUsize,
    recalls: AtomicUsize,
    tail: Mutex<String>,
    declares: bool,
}
impl Synthetic {
    fn new(mode: &'static str) -> Arc<Self> {
        Arc::new(Self {
            mode,
            selections: AtomicUsize::new(0),
            recalls: AtomicUsize::new(0),
            tail: Mutex::new(String::new()),
            declares: true,
        })
    }
}
struct Ext(Arc<Synthetic>);
struct Inst {
    manifest: ExtensionManifest,
    endpoint: Arc<Synthetic>,
}
impl Extension for Ext {
    fn name(&self) -> &'static str {
        "synthetic_context"
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
    fn scopes(&self) -> &[Scope] {
        &[Scope::PerSession]
    }
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().into(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![],
            required_capabilities: vec![],
            requested_capabilities: vec![],
            provides_capabilities: if self.0.declares {
                vec![
                    CapabilityKind::ContextStrategy,
                    CapabilityKind::ContextTail,
                    CapabilityKind::DurableContext,
                ]
            } else {
                vec![]
            },
        }
    }
    fn instantiate<'a>(
        &'a self,
        _: ScopeCtx<'a>,
    ) -> crate::extension::instance::InstantiateFuture<'a> {
        Box::pin(async {
            Ok(Arc::new(Inst {
                manifest: self.manifest(),
                endpoint: self.0.clone(),
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}
impl ExtensionInstance for Inst {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
    fn context_strategy(&self) -> Option<Arc<dyn ContextStrategy>> {
        Some(self.endpoint.clone())
    }
    fn context_tail(&self) -> Option<Arc<dyn ContextTail>> {
        Some(self.endpoint.clone())
    }
    fn durable_context_contributor(&self) -> Option<Arc<dyn DurableContextContributor>> {
        Some(self.endpoint.clone())
    }
}
impl ContextStrategy for Synthetic {
    fn select<'a>(&'a self, call: &'a ContextStrategyCall<'a>) -> CapFuture<'a, ContextPlan> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match self.mode {
                "error" => anyhow::bail!("synthetic selection failure"),
                "panic" => panic!("synthetic selection panic"),
                "timeout" => std::future::pending().await,
                _ => {}
            }
            // A real replacement using only the public scoped read/cost API.
            // It deliberately chooses newest-only, not the hidden old algorithm.
            let latest = call.context.entries().next_back().unwrap().0;
            let hydrated = call.context.hydrate(&ContextItem::Entry(latest))?;
            assert!(matches!(hydrated[0].0, RuntimeMessage::User(_)));
            let mut plan = ContextPlan {
                items: vec![ContextItem::Instructions, ContextItem::Entry(latest)],
                cache: CacheOptions {
                    anchors: vec![CacheAnchor::System, CacheAnchor::Message(1)],
                    ttl: CacheTtl::OneHour,
                },
                truncated: true,
            };
            match self.mode {
                "duplicate" => plan.items.push(ContextItem::Entry(latest)),
                "foreign" => plan.items.push(ContextItem::Entry(usize::MAX)),
                "baseline" | "tail-error" | "durable" => {
                    plan = crate::extensions::context::BaselineContext
                        .select(call)
                        .await?
                }
                _ => {}
            }
            Ok(plan)
        })
    }
}
impl ContextTail for Synthetic {
    fn context_tail<'a>(&'a self, call: &'a ContextTailCall<'a>) -> CapFuture<'a, Option<String>> {
        self.recalls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert_eq!(call.agent_name, "test-agent");
            assert!(call.session_db_id.is_some());
            if self.mode == "tail-error" {
                anyhow::bail!("synthetic optional recall failure");
            }
            let text = self.tail.lock().unwrap().clone();
            Ok((!text.is_empty()).then_some(text))
        })
    }
}
impl DurableContextContributor for Synthetic {
    fn contribute<'a>(
        &'a self,
        call: &'a DurableContextCall<'a>,
    ) -> CapFuture<'a, ContextContribution> {
        Box::pin(async move {
            // Explicit synthetic retention only. Dedup the memory key across
            // genuinely new requests as well as the host's same-request receipt.
            Ok(ContextContribution {
                messages: if call.state.is_some() {
                    vec![]
                } else {
                    vec!["synthetic-recall:key=fixture-only <data>".into()]
                },
                state: Some(json!({"seen": ["fixture-only"]})),
            })
        })
    }
}

fn row(sender: &str, text: &str, kind: EntryType) -> SessionEntry {
    SessionEntry {
        sender: sender.into(),
        content: text.into(),
        timestamp: Utc
            .timestamp_opt(1_700_000_000 + i64::from(text == "current-input"), 0)
            .unwrap(),
        entry_type: kind,
        metadata: None,
        routing: None,
    }
}
async fn assembled(
    hub: Arc<ExtensionHub>,
    session: &Session,
    prompt: &str,
    request: Option<&str>,
) -> anyhow::Result<AssembledContext> {
    let view = session
        .context_view_at(session.database().snapshot().await?)
        .await?;
    let active = crate::extension::read_active(session.database())
        .await?
        .into_iter()
        .map(|r| r.name().to_string())
        .collect();
    ContextBuilder::new(
        &view.entries,
        "test-agent",
        prompt,
        &ContextConfig {
            max_context_tokens: 4096,
            reserved_output_tokens: 256,
        },
    )
    .with_context_view(&view)
    .with_extension_hub(hub)
    .with_session_db(session.database())
    .with_invocation(&active, request)
    .try_build()
    .await
}
async fn send(
    hub: &ExtensionHub,
    session: Arc<tokio::sync::Mutex<Session>>,
    built: AssembledContext,
    mock: Arc<MockBackend>,
) -> Result<(), String> {
    let mut ctx = tool_context(session, Arc::new(ToolRegistry::new()));
    let db = ctx.session.lock().await.database().clone();
    ctx.active_extensions = crate::extension::read_active(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.name().to_string())
        .collect();
    let backend = BackendManager::with_mock(mock, empty_secrets().await);
    runtime::execute_scoped(
        None,
        built.messages,
        &backend,
        &permissive_security(),
        &ctx,
        &ToolPolicyRegistry::empty(),
        None,
        Some(hub),
        ModelCallScope {
            cache: built.cache,
            context_strategy: built.strategy,
            sources: built.sources,
            request_budget_tokens: Some(built.request_budget_tokens),
            ..Default::default()
        },
    )
    .await
    .map(|_| ())
}

#[tokio::test(start_paused = true)]
async fn required_strategy_replacement_absence_failure_and_optional_recall() {
    for mode in [
        "replacement",
        "tail-error",
        "error",
        "panic",
        "timeout",
        "foreign",
        "duplicate",
        "absent",
        "inactive",
        "undeclared",
    ] {
        let (_i, session) = fresh_session().await;
        let db = session.lock().await.database().clone();
        session
            .lock()
            .await
            .add_entry(row("user", "old-input", EntryType::Message))
            .await
            .unwrap();
        session
            .lock()
            .await
            .add_entry(row("user", "current-input", EntryType::Message))
            .await
            .unwrap();
        let endpoint = if mode == "undeclared" {
            Arc::new(Synthetic {
                declares: false,
                ..Arc::try_unwrap(Synthetic::new(mode)).ok().unwrap()
            })
        } else {
            Synthetic::new(mode)
        };
        let mut hub = context_hub().await;
        if mode != "absent" {
            hub.install_all(vec![Arc::new(Ext(endpoint.clone()))])
                .await
                .unwrap();
        }
        hub.set_context_strategy_grant(&ContextStrategyGrant {
            extension: "synthetic_context".into(),
        })
        .unwrap();
        hub.record_active(&db).await.unwrap();
        if mode == "inactive" {
            crate::extension::append_event(
                &db,
                crate::extension::ExtensionEvent::Deactivated {
                    name: "synthetic_context".into(),
                    timestamp: Utc::now() + chrono::Duration::seconds(1),
                },
            )
            .await
            .unwrap();
        }
        let hub = Arc::new(hub);
        let mock = Arc::new(MockBackend::new());
        mock.push_text("ok");
        let built = assembled(
            hub.clone(),
            &*session.lock().await,
            "policy",
            Some("current"),
        )
        .await;
        if matches!(mode, "replacement" | "tail-error") {
            send(&hub, session.clone(), built.unwrap(), mock.clone())
                .await
                .unwrap();
            let calls = mock.recorded_calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].messages[0],
                RuntimeMessage::System("policy".into())
            );
            assert_eq!(
                calls[0].messages.last().unwrap(),
                &RuntimeMessage::User("current-input".into())
            );
            assert_eq!(
                calls[0]
                    .messages
                    .iter()
                    .any(|m| *m == RuntimeMessage::User("old-input".into())),
                mode == "tail-error"
            );
            println!("STRATEGY {mode}: {calls:?}");
        } else {
            assert!(built.is_err(), "{mode}");
            assert!(mock.recorded_calls().is_empty());
            println!("REFUSAL {mode}: {}", built.err().unwrap());
        }
        if matches!(mode, "absent" | "inactive" | "undeclared") {
            assert_eq!(endpoint.selections.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn ephemeral_recall_reopen_settings_and_live_required_revocation() {
    let (_i, session) = fresh_session().await;
    let db = session.lock().await.database().clone();
    let endpoint = Synthetic::new("baseline");
    let mut hub = context_hub().await;
    hub.install_all(vec![Arc::new(Ext(endpoint.clone()))])
        .await
        .unwrap();
    hub.record_active(&db).await.unwrap();
    let hub = Arc::new(hub);
    session
        .lock()
        .await
        .add_entry(row("user", "current-input", EntryType::Message))
        .await
        .unwrap();
    for tail in ["", "recall-one", "recall-two"] {
        *endpoint.tail.lock().unwrap() = tail.into();
        let warm = assembled(
            hub.clone(),
            &*session.lock().await,
            "policy",
            Some("current"),
        )
        .await
        .unwrap();
        let reopened = Session::new(
            crate::types::ConversationId(db.root_id().to_string()),
            db.clone(),
        )
        .await;
        let cold = assembled(hub.clone(), &reopened, "policy", Some("current"))
            .await
            .unwrap();
        assert_eq!(warm.messages, cold.messages);
        assert_eq!(warm.sources, cold.sources);
        let mock = Arc::new(MockBackend::new());
        mock.push_text("ok");
        send(&hub, session.clone(), cold, mock.clone())
            .await
            .unwrap();
        assert_eq!(mock.recorded_calls()[0].messages, warm.messages);
        println!("EPHEMERAL {tail:?}: {:?}", mock.recorded_calls());
    }
    assert!(
        session
            .lock()
            .await
            .context_view_at(db.snapshot().await.unwrap())
            .await
            .unwrap()
            .contributions
            .is_empty()
    );
    crate::extension::write_settings(
        &db,
        "baseline_context",
        json!({"cache": {"anchors": ["system", {"message": 1}], "ttl": "one_hour"}}),
    )
    .await
    .unwrap();
    let selected = assembled(
        hub.clone(),
        &*session.lock().await,
        "policy-v2",
        Some("current"),
    )
    .await
    .unwrap();
    assert_eq!(selected.cache.ttl, CacheTtl::OneHour);
    assert_eq!(
        selected.cache.anchors,
        vec![CacheAnchor::System, CacheAnchor::Message(1)]
    );
    let txn = db.new_transaction().await.unwrap();
    let state = txn
        .get_store::<DocStore>("context_selections")
        .await
        .unwrap()
        .get_string(serde_json::to_string(&("test-agent", "baseline_context")).unwrap())
        .await
        .unwrap();
    println!("SELECTION_STATE {state}");
    assert!(state.contains("one_hour"));
    // A successful assembly is not a frozen permission to run the strategy.
    crate::extension::append_event(
        &db,
        crate::extension::ExtensionEvent::Deactivated {
            name: "baseline_context".into(),
            timestamp: Utc::now() + chrono::Duration::seconds(1),
        },
    )
    .await
    .unwrap();
    let mock = Arc::new(MockBackend::new());
    mock.push_text("must not send");
    assert!(
        send(&hub, session, selected, mock.clone())
            .await
            .unwrap_err()
            .contains("required context strategy")
    );
    assert!(mock.recorded_calls().is_empty());
}

#[tokio::test]
async fn explicit_synthetic_durable_recall_dedups_retry_reopen_and_new_requests() {
    let (_i, session) = fresh_session().await;
    let db = session.lock().await.database().clone();
    let endpoint = Synthetic::new("durable");
    let mut hub = context_hub().await;
    hub.install_all(vec![Arc::new(Ext(endpoint))])
        .await
        .unwrap();
    hub.set_durable_context_grants(&[DurableContextGrant {
        extension: "synthetic_context".into(),
        required: false,
    }])
    .unwrap();
    hub.record_active(&db).await.unwrap();
    let active = HashSet::from(["baseline_context".into(), "synthetic_context".into()]);
    let hub = Arc::new(hub);
    for request in ["request-one", "request-one", "request-two"] {
        let reopened = Session::new(
            crate::types::ConversationId(db.root_id().to_string()),
            db.clone(),
        )
        .await;
        hub.prepare_durable_context(
            &reopened,
            "test-agent",
            Some(&TurnRequestId::parse(request)),
            &active,
        )
        .await
        .unwrap();
        let built = assembled(hub.clone(), &reopened, "policy", Some(request))
            .await
            .unwrap();
        let mock = Arc::new(MockBackend::new());
        mock.push_text("ok");
        send(&hub, session.clone(), built, mock.clone())
            .await
            .unwrap();
        assert_eq!(
            mock.recorded_calls()[0]
                .messages
                .iter()
                .filter(|m| matches!(m, RuntimeMessage::User(s) if s.contains("synthetic-recall")))
                .count(),
            1
        );
        println!("DURABLE {request}: {:?}", mock.recorded_calls());
    }
    let view = session
        .lock()
        .await
        .context_view_at(db.snapshot().await.unwrap())
        .await
        .unwrap();
    assert_eq!(view.contributions.len(), 2);
    assert_eq!(
        view.contributions
            .iter()
            .flat_map(|r| &r.contribution.messages)
            .count(),
        1
    );
    println!("DURABLE_DATABASE {:?}", view.contributions);
}

#[tokio::test]
async fn extension_selected_cache_serializes_supported_and_unsupported_providers() {
    let (_i, session) = fresh_session().await;
    let db = session.lock().await.database().clone();
    let endpoint = Synthetic::new("replacement");
    let mut hub = context_hub().await;
    hub.install_all(vec![Arc::new(Ext(endpoint))])
        .await
        .unwrap();
    hub.set_context_strategy_grant(&ContextStrategyGrant {
        extension: "synthetic_context".into(),
    })
    .unwrap();
    hub.record_active(&db).await.unwrap();
    session
        .lock()
        .await
        .add_entry(row("user", "stable-input", EntryType::Message))
        .await
        .unwrap();
    let built = assembled(
        Arc::new(hub),
        &*session.lock().await,
        "policy",
        Some("current"),
    )
    .await
    .unwrap();
    let mut messages = built.messages;
    messages.push(RuntimeMessage::User("disposable-recall".into()));
    for (openrouter, model, supported) in [
        (true, "anthropic/claude-test", true),
        (true, "other/test", false),
        (false, "anthropic/claude-test", false),
    ] {
        let wire =
            crate::openai::serialize_cache_fixture(&messages, &[], &built.cache, openrouter, model)
                .await;
        assert_eq!(wire.to_string().contains("cache_control"), supported);
        if supported {
            assert_eq!(
                wire["messages"][1]["content"][0]["cache_control"]["ttl"],
                "1h"
            );
            assert!(wire["messages"][2]["content"].is_string());
        }
        println!("OPENAI_CACHE supported={supported} {wire}");
    }
    let wire = crate::anthropic::serialize_cache_fixture(&messages, &[], &built.cache);
    assert_eq!(
        wire["messages"][0]["content"][0]["cache_control"]["ttl"],
        "1h"
    );
    assert!(
        wire["messages"][0]["content"][1]
            .get("cache_control")
            .is_none()
    );
    println!("ANTHROPIC_CACHE {wire}");
    let invalid = CacheOptions {
        anchors: vec![CacheAnchor::Message(100)],
        ..Default::default()
    };
    assert!(invalid.validate(&messages).is_err());
}

#[tokio::test]
async fn fixed_cold_reconstruction_elapsed_and_logical_database_read_volume() {
    use crate::session::{
        TurnAttempt, TurnAttemptStatus, TurnTranscriptMessage, TurnTranscriptRecord,
    };
    for (label, n) in [("small", 8), ("medium", 128), ("large", 1024)] {
        let (_i, session) = fresh_session().await;
        let db = session.lock().await.database().clone();
        let txn = db.new_transaction().await.unwrap();
        let entries = txn
            .get_store::<Table<SessionEntry>>("entries")
            .await
            .unwrap();
        let attempts = txn
            .get_store::<Table<TurnAttempt>>("turn_attempts")
            .await
            .unwrap();
        let transcripts = txn
            .get_store::<Table<TurnTranscriptRecord>>("turn_transcript")
            .await
            .unwrap();
        for i in 0..n {
            let id = TurnRequestId::parse(format!("request-{i:06}"));
            let timestamp = Utc.timestamp_opt(1_700_000_000 + i as i64, 0).unwrap();
            let mut entry = row(
                "user",
                &format!(
                    "request {i:06}: fixed synthetic reconstruction input {}",
                    "words ".repeat(32)
                ),
                EntryType::Message,
            );
            entry.timestamp = timestamp;
            entries.set(id.as_str(), entry).await.unwrap();
            let attempt = TurnAttempt {
                attempt_id: format!("attempt-{i:06}"),
                request_id: id.clone(),
                started_at: timestamp,
                generation: 0,
                status: TurnAttemptStatus::Completed,
                completed_at: Some(timestamp),
            };
            attempts
                .set(&attempt.attempt_id, attempt.clone())
                .await
                .unwrap();
            let call = crate::runtime::ToolCallRequest {
                id: format!("call-{i:06}"),
                name: "lookup".into(),
                arguments: "{}".into(),
            };
            for (sequence, message) in [
                TurnTranscriptMessage::ModelResponse {
                    model_sequence: 0,
                    content: None,
                    tool_calls: vec![call.clone()],
                    provider_extra: Default::default(),
                    metadata: None,
                    terminal: false,
                },
                TurnTranscriptMessage::ToolResult {
                    model_sequence: 0,
                    call_index: 0,
                    call_id: call.id,
                    name: "lookup".into(),
                    output: "synthetic output ".repeat(64),
                    outcome: runtime::ToolResultOutcome::Success,
                },
                TurnTranscriptMessage::ModelResponse {
                    model_sequence: 1,
                    content: Some("done".into()),
                    tool_calls: vec![],
                    provider_extra: Default::default(),
                    metadata: None,
                    terminal: true,
                },
            ]
            .into_iter()
            .enumerate()
            {
                transcripts
                    .set(
                        format!("{i:06}:{sequence}"),
                        TurnTranscriptRecord {
                            request_id: id.clone(),
                            attempt_id: attempt.attempt_id.clone(),
                            sequence: sequence as u64,
                            timestamp,
                            message,
                        },
                    )
                    .await
                    .unwrap();
            }
        }
        txn.commit().await.unwrap();
        // Cold means no Session or strategy-derived view is reused. Underlying
        // Eidetica/OS caches are not flushed; no physical-I/O claim is made.
        for sample in 0..3 {
            let hub = Arc::new(context_hub().await);
            hub.record_active(&db).await.unwrap();
            let start = std::time::Instant::now();
            let reopened = Session::new(
                crate::types::ConversationId(db.root_id().to_string()),
                db.clone(),
            )
            .await;
            let view = reopened
                .context_view_at(db.snapshot().await.unwrap())
                .await
                .unwrap();
            let read_elapsed = start.elapsed();
            let strategy_start = std::time::Instant::now();
            let built = ContextBuilder::new(
                &view.entries,
                "test-agent",
                "policy",
                &ContextConfig {
                    max_context_tokens: 4096,
                    reserved_output_tokens: 256,
                },
            )
            .with_context_view(&view)
            .with_session_db(&db)
            .with_extension_hub(hub)
            .try_build()
            .await
            .unwrap();
            assert_eq!(view.entries.len(), n);
            assert_eq!(view.tool_history.iter().flatten().count(), 3 * n);
            assert!(built.messages.contains(&RuntimeMessage::User(
                view.entries.last().unwrap().content.clone()
            )));
            println!(
                "RECONSTRUCTION {}",
                json!({"fixture": label, "requests": n, "sample": sample, "entry_text_words": 32, "tool_output_repeats": 64, "context_budget": 3840, "database_view_us": read_elapsed.as_micros(), "strategy_us": strategy_start.elapsed().as_micros(), "total_us": start.elapsed().as_micros(), "selected_entries": built.entries_included, "logical_read_volume": view.read_volume})
            );
        }
    }
}

#[tokio::test]
async fn baseline_tool_rounds_retry_and_originals_keep_the_selected_prefix() {
    let (_i, session) = fresh_session().await;
    let db = session.lock().await.database().clone();
    let hub = Arc::new(context_hub().await);
    hub.record_active(&db).await.unwrap();
    session
        .lock()
        .await
        .add_entry(row("user", "current-input", EntryType::Message))
        .await
        .unwrap();
    let registry = Arc::new(ToolRegistry::new());
    registry.register(crate::tools::Calculate);
    let mut ctx = tool_context(session.clone(), registry);
    ctx.active_extensions.insert("baseline_context".into());
    let view = session
        .lock()
        .await
        .context_view_at(db.snapshot().await.unwrap())
        .await
        .unwrap();
    let defs = ctx.definitions();
    let built = ContextBuilder::new(&view.entries, "test-agent", "policy", &Default::default())
        .with_context_view(&view)
        .with_tools(&defs)
        .with_extension_hub(hub.clone())
        .with_session_db(&db)
        .try_build()
        .await
        .unwrap();
    let original = built.messages.clone();
    let mock = Arc::new(MockBackend::new());
    mock.push_err(crate::error::LlmError::Timeout);
    mock.push_tool_calls([(
        "call-one".into(),
        "calculate".into(),
        "{\"expression\":\"1+1\"}".into(),
    )]);
    mock.push_tool_calls([(
        "call-two".into(),
        "calculate".into(),
        "{\"expression\":\"2+2\"}".into(),
    )]);
    mock.push_text("done");
    let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
    runtime::execute_scoped(
        None,
        built.messages,
        &backend,
        &permissive_security(),
        &ctx,
        &ToolPolicyRegistry::empty(),
        None,
        Some(&hub),
        ModelCallScope {
            cache: built.cache.clone(),
            context_strategy: built.strategy,
            sources: built.sources,
            request_budget_tokens: Some(built.request_budget_tokens),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].messages, calls[1].messages);
    for call in &calls {
        assert_eq!(&call.messages[..original.len()], original);
        assert_eq!(call.cache, built.cache);
        assert_eq!(call.tools, defs);
    }
    assert_eq!(calls[2].messages.len(), original.len() + 2);
    assert_eq!(calls[3].messages.len(), original.len() + 4);
    let current = session
        .lock()
        .await
        .context_view_at(db.snapshot().await.unwrap())
        .await
        .unwrap();
    assert_eq!(current.entries.len(), view.entries.len());
    assert!(current.contributions.is_empty());
    println!("TOOL_LOOP_RETRY {calls:?}");
}
