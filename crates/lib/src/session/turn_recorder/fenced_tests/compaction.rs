use super::*;
use crate::{
    runtime::RuntimeRecorder,
    session::conversation::staging::{
        CompactionIntent, CompactionSettlement, Observation, Staging, stage_observation,
    },
};

const QUESTION: &str = "Current question stays verbatim.";
const FOLLOWUP: &str = "New question after reopen.";
const CORRECTION: &str = "Unrelated sibling survives.";
const FOLD: &str = "Durably summarized old evidence.";

async fn enable_recovery(db: &Database) {
    crate::extension::append_event(
        db,
        crate::extension::ExtensionEvent::Activated {
            name: "context_prune".into(),
            extension_ref: crate::extension::ExtensionRef::builtin("context_prune"),
            timestamp: Utc::now(),
        },
    )
    .await
    .unwrap();
}

fn completions(message: Value, finish: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id":"compact-fixture-response", "model":MODEL,
        "choices":[{"index":0,"message":message,"finish_reason":finish}],
        "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}
    }))
}

fn text_response(text: &str) -> ResponseTemplate {
    completions(json!({"role":"assistant", "content":text}), "stop")
}

fn contents(request: &Value, text: &str) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|c| c.contains(text)))
}

fn asks_summary(body: &Value) -> bool {
    body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("You are a context summarization assistant.")
}

impl Fixture {
    async fn advance(&mut self, branch: &str, input: &str, reopen: bool) {
        if reopen {
            self.db = self.reopened().await;
            self.session = Arc::new(Mutex::new(
                Session::new(
                    crate::types::ConversationId("fenced".into()),
                    self.db.clone(),
                )
                .await,
            ));
        }
        self.recorder = Arc::new(
            Session::start_compacting_fixture(
                self.session.clone(),
                branch.into(),
                AGENT.into(),
                message("user", input),
            )
            .await
            .unwrap(),
        );
        self.input = self.db.snapshot().await.unwrap();
        self.context.session = self.session.clone();
        self.context.turn_request_id = Some(self.recorder.attempt.request_id.clone());
        self.context.turn_attempt = Some(crate::tool::ToolAttemptIdentity {
            attempt_id: self.recorder.attempt.attempt_id.clone(),
            execution_id: Some(branch.into()),
        });
    }

    async fn enable_automatic(&mut self) {
        use crate::extension::{ExtensionEvent, ExtensionRef, append_event, update_settings};
        use crate::extensions::autocompact::NAME;
        self.backend.insert_model_window(MODEL.into(), 8192);
        self.context.active_extensions.insert(NAME.into());
        update_settings(&self.db, NAME, false, |settings| {
            *settings = json!({"enabled":true,"trigger_tokens":500,"keep_recent_tokens":1,"summary_max_tokens":128});
            Ok(())
        }).await.unwrap();
        append_event(
            &self.db,
            ExtensionEvent::Activated {
                name: NAME.into(),
                extension_ref: ExtensionRef::builtin(NAME),
                timestamp: Utc::now(),
            },
        )
        .await
        .unwrap();
    }

    async fn operation_log(&self) -> crate::session::ConversationInspection {
        let read = Session::history(&self.db).await.unwrap();
        let inspected = read.inspect_conversation().await.unwrap().unwrap();
        assert!(inspected.issues.is_empty(), "{:?}", inspected.issues);
        inspected.clone()
    }
}

#[tokio::test]
async fn fenced_v1_real_compact_tool_default_stack_reuses_signed_replacement() {
    let fx = Fixture::build(
        true,
        None,
        Some("Old source stays in the signed log."),
        |req: &Request| {
            let body: Value = req.body_json().unwrap();
            let continued = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["tool_call_id"] == "compact-now");
            if contents(&body, QUESTION) && !contents(&body, FOLLOWUP) && !continued {
                completions(
                    json!({"role":"assistant", "content":null, "tool_calls":[
                        {"id":"compact-now", "type":"function", "function":{"name":"compact", "arguments":json!({"summary":FOLD}).to_string()}},
                        {"id":"add-now", "type":"function", "function":{"name":TOOL, "arguments":"{\"a\":2,\"b\":2}"}}
                    ]}),
                    "tool_calls",
                )
            } else {
                text_response(BODY)
            }
        },
    );
    let mut fx = Box::pin(fx).await;
    let first = Box::pin(fx.run()).await.unwrap();
    Box::pin(fx.complete(&first.body)).await.unwrap();
    let before = fx.db.snapshot().await.unwrap();
    Box::pin(fx.advance("compact-turn", QUESTION, false)).await;
    let output = Box::pin(fx.run()).await.unwrap();
    Box::pin(fx.complete(&output.body)).await.unwrap();
    let inspect = Box::pin(fx.operation_log()).await;
    assert_eq!(
        inspect
            .operations
            .atoms
            .values()
            .filter(|a| a.contains("TransformationOutput"))
            .count(),
        1
    );
    assert!(
        inspect
            .provenance
            .atoms
            .values()
            .any(|a| a.contains("CompactionSettled") && a.contains("Applied"))
    );
    let recorded = Session::history(&fx.db)
        .await
        .unwrap()
        .recorded_call(&fx.call(1), &fx.recorder.attempt.attempt_id)
        .await
        .unwrap()
        .unwrap()
        .clone();
    let old_print = Session::history_at(&fx.db, &before)
        .await
        .unwrap()
        .display_rows()
        .await
        .unwrap();
    assert!(old_print.iter().any(|r| r.content.contains("Old source")));

    Box::pin(fx.advance("after-reopen", FOLLOWUP, true)).await;
    let next = Box::pin(fx.run()).await.unwrap();
    Box::pin(fx.complete(&next.body)).await.unwrap();
    let wire = Box::pin(fx.assert_effects(4, 1, 3)).await;
    assert!(contents(&wire[1], "Old source"));
    assert!(!contents(&wire[2], "Old source"));
    assert!(contents(&wire[2], FOLD) && contents(&wire[2], QUESTION));
    let slots = wire[2]["messages"].as_array().unwrap();
    let call = slots
        .iter()
        .position(|slot| slot["tool_calls"][0]["id"] == "compact-now")
        .unwrap();
    assert_eq!(slots[call + 1]["tool_call_id"], "compact-now");
    assert!(
        slots[call + 1]["content"]
            .as_str()
            .unwrap()
            .contains("Context compacted.")
    );
    assert_eq!(slots[call + 2]["tool_call_id"], "add-now");
    assert_eq!(recorded.messages.len(), slots.len());
    assert!(contents(&wire[3], FOLD) && contents(&wire[3], FOLLOWUP));
    assert!(!contents(&wire[3], "Old source"));
    assert!(Box::pin(print(&fx.db)).await.contains(FOLD));
    assert!(Session::require_legacy_history(&fx.db).await.is_err());
    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("cannot execute twice")
    );
}

#[tokio::test]
async fn fenced_v1_background_summary_default_stack_survives_untargeted_sibling() {
    let old = "  Historical detail unique to original source 🦀. \n".repeat(500);
    let native = "  Assistant evidence 🦀 before a tool call. \n".repeat(500);
    let mock_user = old.clone();
    let mock_native = native.clone();
    let mut fx = Box::pin(Fixture::build(true, None, Some(&old), move |req: &Request| {
        let body: Value = req.body_json().unwrap();
        if asks_summary(&body) {
            text_response(FOLD)
        } else if contents(&body, &mock_user)
            && !body["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool")
        {
            completions(json!({"role":"assistant", "content":mock_native, "reasoning_content":"retained opaque fixture",
                "tool_calls":[{"id":"wire-1","type":"function","function":{"name":TOOL,"arguments":"{ \"a\": 2, \"b\": 2 }"}}]}), "tool_calls")
        } else {
            text_response(BODY)
        }
    }))
    .await;
    let seed = Box::pin(fx.run()).await.unwrap();
    Box::pin(fx.complete(&seed.body)).await.unwrap();
    let registry = Arc::new(ToolRegistry::new());
    registry.register(Add(fx.tool.clone(), Default::default()));
    registry.register_arc_owned(
        Arc::new(crate::extensions::pruner::ToolHistoryQuery),
        Some("context_prune"),
    );
    fx.context.tools = crate::tool::ScopedTools::new(registry.clone(), None);
    fx.context.active_extensions.insert("context_prune".into());
    Box::pin(enable_recovery(&fx.db)).await;
    Box::pin(fx.enable_automatic()).await;
    fx.backend.insert_model_window(MODEL.into(), 65_536);
    let before = fx.db.snapshot().await.unwrap();
    Box::pin(fx.advance("background-turn", QUESTION, false)).await;
    fx.budget = Some(500);
    // Second signed input on a sibling of the selected turn, not a latest
    // re-parenting of the attempt. This fixture never executes the sibling.
    let (sibling, _, _) = Box::pin(fenced::accept_fixture_input(
        &fx.db,
        &before,
        "independent-append",
        message("user", CORRECTION),
    ))
    .await
    .unwrap();
    let output = Box::pin(fx.run()).await.unwrap();
    Box::pin(fx.complete(&output.body)).await.unwrap();
    let wire = Box::pin(fx.assert_effects(4, 1, 2)).await;
    assert!(asks_summary(&wire[2]));
    assert!(contents(&wire[2], old.trim()));
    assert!(!contents(&wire[2], QUESTION) && !contents(&wire[2], CORRECTION));
    assert!(contents(&wire[3], FOLD) && contents(&wire[3], QUESTION));
    assert!(!contents(&wire[3], CORRECTION));
    let summary = wire[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["content"].as_str().is_some_and(|c| c.contains(FOLD)))
        .unwrap()
        .clone();
    let disk = fx._disk.take().unwrap();
    let root = fx.db.root_id().clone();
    let call = fx.call(0);
    let attempt = fx.recorder.attempt.attempt_id.clone();
    // Release every handle before reopening the same SQLite namespace.
    drop(fx);
    let url = format!(
        "sqlite://{}?mode=rwc",
        disk.path().join("fenced.db").display()
    );
    let fresh = Instance::connect(&url).await.unwrap();
    let account = fresh.login_account("test", "test-password").await.unwrap();
    let reopened = account.open_database(&root).await.unwrap();
    let joined = Session::history(&reopened).await.unwrap();
    let prepared = joined
        .recorded_call(&call, &attempt)
        .await
        .unwrap()
        .unwrap();
    let seen = Session::history_at(&reopened, &prepared.observed.frontier)
        .await
        .unwrap()
        .display_rows()
        .await
        .unwrap();
    assert!(seen.iter().any(|r| r.content.starts_with(FOLD)));
    assert!(!seen.iter().any(|r| r.content == CORRECTION));
    let isolated_append = Session::history_at(&reopened, &sibling)
        .await
        .unwrap()
        .display_rows()
        .await
        .unwrap();
    assert!(isolated_append.iter().any(|r| r.content == CORRECTION));
    assert!(!isolated_append.iter().any(|r| r.content.starts_with(FOLD)));
    let history = joined.context().await.unwrap();
    let context = ContextBuilder::from_history(&history, AGENT, "", &ContextConfig::default())
        .build()
        .await;
    assert!(
        context
            .messages
            .iter()
            .any(|m| matches!(m, runtime::RuntimeMessage::User(text) if text.contains(FOLD)))
    );
    let displayed = joined.display_rows().await.unwrap();
    assert_eq!(
        displayed.iter().filter(|r| r.content == CORRECTION).count(),
        1
    );
    assert_eq!(
        displayed
            .iter()
            .filter(|r| r.content.starts_with(FOLD))
            .count(),
        1
    );
    assert_eq!(
        displayed.iter().filter(|r| r.content == QUESTION).count(),
        1
    );
    assert!(!displayed.iter().any(|r| r.content == old));
    let inspect = joined.inspect_conversation().await.unwrap().unwrap();
    assert!(inspect.issues.is_empty(), "{:?}", inspect.issues);
    assert!(
        inspect
            .operations
            .atoms
            .values()
            .any(|a| a.contains("Historical detail unique"))
    );
    assert!(
        inspect
            .provenance
            .atoms
            .values()
            .any(|a| a.contains("CompactionSettled") && a.contains("Applied"))
    );

    // The model uses the host's citation, not an ID chosen by the summarizer.
    let rendered = summary["content"].as_str().unwrap();
    let (reference, _) = rendered
        .split_once("cr1:")
        .unwrap()
        .1
        .split_once(":<index>")
        .unwrap();
    let user_ref = format!("cr1:{reference}:0");
    let assistant_ref = format!("cr1:{reference}:1");
    let session = Arc::new(Mutex::new(
        Session::new(
            crate::types::ConversationId("fenced".into()),
            reopened.clone(),
        )
        .await,
    ));
    let mut reopened_context = tool_context_with_host(session, registry, Arc::new(MockHost::new()));
    reopened_context
        .active_extensions
        .insert("context_prune".into());
    let originals = &inspect.operations.atoms;
    for (reference, kind, text) in [
        (&user_ref, "Input", &old),
        (&assistant_ref, "NativeModelResponse", &native),
    ] {
        let actual = Box::pin(reassemble(&reopened_context, reference)).await;
        let expected = raw_fact(originals.values(), kind, text);
        assert_eq!(actual, expected);
    }
    let hidden = reopened_context
        .tools
        .narrow(Some(&["tool_history_query".into()]));
    let mut restricted = reopened_context.clone();
    restricted.tools = hidden;
    assert_eq!(
        Box::pin(query(&restricted, &assistant_ref, 0)).await["status"],
        "missing_or_not_authorized"
    );
    assert_eq!(
        Box::pin(query(&restricted, &user_ref, 0)).await["status"],
        "retained"
    );
    for forged in [
        format!("cr1:{}:0", "0".repeat(64)),
        format!("cr1:{reference}:999"),
    ] {
        assert_eq!(
            Box::pin(query(&reopened_context, &forged, 0)).await["status"],
            "missing_or_not_authorized"
        );
    }
    let (_peer, _account, foreign_db) = crate::session::test_helpers::test_session_db().await;
    Box::pin(enable_recovery(&foreign_db)).await;
    let mut foreign = reopened_context.clone();
    foreign.session = Arc::new(Mutex::new(
        Session::new(crate::types::ConversationId("foreign".into()), foreign_db).await,
    ));
    assert_eq!(
        Box::pin(query(&foreign, &user_ref, 0)).await["status"],
        "missing_or_not_authorized"
    );
}

async fn query(ctx: &ToolContext, reference: &str, offset: usize) -> Value {
    let result = crate::extensions::pruner::ToolHistoryQuery
        .execute(json!({"reference":reference,"offset":offset}), ctx)
        .await
        .unwrap();
    serde_json::from_str(&result).unwrap()
}

async fn reassemble(ctx: &ToolContext, reference: &str) -> String {
    let mut full = String::new();
    for i in 0..8 {
        let page = Box::pin(query(ctx, reference, full.len())).await;
        assert_eq!(page["status"], "retained");
        let text = page["content"].as_str().unwrap();
        assert!(!text.is_empty() && text.len() <= 16 * 1024);
        full.push_str(text);
        if page["next_offset"].is_null() {
            assert!(i > 0, "fixture must exercise multiple pages");
            assert_eq!(page["stored_bytes"], full.len());
            use sha2::{Digest, Sha256};
            assert_eq!(
                page["sha256"],
                format!("{:x}", Sha256::digest(full.as_bytes()))
            );
            return full;
        }
        assert_eq!(page["next_offset"], full.len());
    }
    panic!("fixture exceeded its paging bound");
}

fn raw_fact<'a>(atoms: impl Iterator<Item = &'a String>, kind: &str, text: &str) -> &'a str {
    #[derive(serde::Deserialize)]
    struct Facts<'a> {
        #[serde(borrow)]
        facts: Vec<&'a serde_json::value::RawValue>,
    }
    atoms
        .flat_map(|atom| serde_json::from_str::<Facts<'_>>(atom).unwrap().facts)
        .map(serde_json::value::RawValue::get)
        .find(|raw| {
            let fact: Value = serde_json::from_str(raw).unwrap();
            fact["payload"]["kind"] == kind
                && (fact["payload"]["text"] == text
                    || fact["payload"]["response"]["content"] == text)
        })
        .unwrap()
}

#[tokio::test]
async fn fenced_v1_priced_idle_preparation_waits_and_missing_ttl_defers() {
    let old = "Prior source with useful material. ".repeat(300);
    let mut fx = Box::pin(Fixture::build(false, None, Some(&old), |req: &Request| {
        if asks_summary(&req.body_json().unwrap()) {
            text_response(FOLD)
        } else {
            text_response(BODY)
        }
    }))
    .await;
    let mut info = crate::backends::ModelInfo {
        id: MODEL.into(),
        price_input: Some(0.1),
        price_output: Some(0.1),
        price_cache_read: Some(0.1),
        price_cache_write: Some(3.0),
        ..Default::default()
    };
    fx.backend.insert_model_info(info.clone());
    Box::pin(fx.enable_automatic()).await;
    Box::pin(crate::extension::update_settings(
        &fx.db,
        "autocompact",
        false,
        |settings| {
            settings["cache_aware"] = json!(true);
            Ok(())
        },
    ))
    .await
    .unwrap();
    for (branch, input) in [(None, old.as_str()), (Some("unknown-ttl"), QUESTION)] {
        if let Some(branch) = branch {
            Box::pin(fx.advance(branch, input, false)).await;
        }
        let run = Box::pin(fx.run()).await.unwrap();
        // Use a deterministic completion time in the disposable signed Store
        // so the next boundary is expired without sleeps or model traffic.
        let mut entry = message(AGENT, &run.body);
        entry.timestamp = Utc::now() - chrono::Duration::minutes(10);
        Box::pin(
            fx.recorder
                .complete(&mut *fx.session.lock().await, Some(entry), None),
        )
        .await
        .unwrap();
    }
    let wire = Box::pin(fx.assert_effects(2, 0, 0)).await;
    assert!(
        wire.iter()
            .all(|req| !asks_summary(req) && contents(req, &old))
    );
    let originals = Box::pin(fx.operation_log()).await;
    assert!(
        !originals
            .provenance
            .atoms
            .values()
            .any(|a| a.contains("CompactionIntent"))
    );

    info.cache_lifetime_seconds = Some(300);
    fx.backend.insert_model_info(info);
    Box::pin(fx.advance("expired-priced", FOLLOWUP, true)).await;
    let run = Box::pin(fx.run()).await.unwrap();
    Box::pin(fx.complete(&run.body)).await.unwrap();
    let wire = Box::pin(fx.assert_effects(4, 0, 0)).await;
    assert!(asks_summary(&wire[2]));
    assert!(contents(&wire[3], FOLD) && !contents(&wire[3], &old));

    let slots = wire[3]["messages"].as_array().unwrap();
    let fold = slots
        .iter()
        .position(|m| m["content"].as_str().is_some_and(|s| s.contains(FOLD)))
        .unwrap();
    let stable_prefix = &slots[..=fold];
    for i in 0..3 {
        Box::pin(fx.advance(&format!("replay-{i}"), FOLLOWUP, i == 0)).await;
        let next = Box::pin(fx.run()).await.unwrap();
        Box::pin(fx.complete(&next.body)).await.unwrap();
    }
    let replay = Box::pin(fx.assert_effects(7, 0, 0)).await;
    assert_eq!(replay.iter().filter(|wire| asks_summary(wire)).count(), 1);
    for req in &replay[4..] {
        assert!(
            req["messages"]
                .as_array()
                .unwrap()
                .starts_with(stable_prefix)
        );
    }
}

#[tokio::test]
async fn fenced_v1_failed_summary_is_unapplied_and_never_replayed() {
    let old = "Unchanged old evidence. ".repeat(250);
    let mut fx = Box::pin(Fixture::build(false, None, Some(&old), |req: &Request| {
        let body: Value = req.body_json().unwrap();
        if asks_summary(&body) {
            ResponseTemplate::new(500)
        } else {
            text_response(BODY)
        }
    }))
    .await;
    let first = Box::pin(fx.run()).await.unwrap();
    Box::pin(fx.complete(&first.body)).await.unwrap();
    Box::pin(fx.enable_automatic()).await;
    Box::pin(fx.advance("failed-summary", QUESTION, false)).await;
    fx.budget = Some(500);
    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("over the 500-token budget")
    );
    let inspect = Box::pin(fx.operation_log()).await;
    assert!(
        inspect
            .provenance
            .atoms
            .values()
            .any(|a| a.contains("CompactionSettled") && a.contains("Unapplied"))
    );
    assert!(
        !inspect
            .operations
            .atoms
            .values()
            .any(|a| a.contains("TransformationOutput"))
    );
    let reopened = fx.reopened().await;
    assert!(Box::pin(print(&reopened)).await.contains(&old));
    let reopened = Arc::new(Mutex::new(
        Session::new(crate::types::ConversationId("fenced".into()), reopened).await,
    ));
    assert!(
        Box::pin(Session::start_compacting_fixture(
            reopened,
            "restart".into(),
            AGENT.into(),
            message("user", "try again")
        ))
        .await
        .is_err()
    );
    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("cannot execute twice")
    );
    let wire = Box::pin(fx.assert_effects(2, 0, 0)).await;
    assert!(asks_summary(&wire[1]));
}

#[tokio::test]
async fn fenced_v1_overlapping_result_does_not_replace_current_coverage() {
    async fn exercise() {
        let mut fx = Box::pin(Fixture::build(
            false,
            None,
            Some(&"Original historical input. ".repeat(100)),
            |_: &Request| text_response(BODY),
        ))
        .await;
        let first = Box::pin(fx.run()).await.unwrap();
        Box::pin(fx.complete(&first.body)).await.unwrap();
        Box::pin(fx.advance("conflict", QUESTION, false)).await;
        // Reserve once and leave a competing signed replacement on a sibling
        // of this source while the already-dispatched work would be pending.
        fx.recorder.begin_turn(false).unwrap();
        let txn = fx.db.new_transaction_at(&fx.input).await.unwrap();
        let source = Staging::at(&fx.db, &fx.input, &txn, "conflict")
            .await
            .unwrap();
        let view = source.context().unwrap();
        let input = view
            .groups
            .last()
            .unwrap()
            .representation_id()
            .unwrap()
            .clone();
        let members = source.historical_members(&input).unwrap();
        let scope = fx.recorder.context_compaction().unwrap();
        let original_intent = scope.reserve(members.clone()).await.unwrap();
        let sibling = CompactionIntent {
            id: uuid::Uuid::new_v4().to_string(),
            input,
            observed: source.observed.clone(),
            targets: members.iter().map(|(_, id)| id.clone()).collect(),
            tool: None,
        };
        stage_observation(
            &txn,
            Observation::CompactionIntent {
                intent: sibling.clone(),
            },
        )
        .await
        .unwrap();
        let sibling_intent = Snapshot::from([txn.commit().await.unwrap()]);
        let sibling_txn = fx.db.new_transaction_at(&sibling_intent).await.unwrap();
        // The original source is opened at its original parent again; this
        // doesn't pick a new source from the latest effective view.
        let source_txn = fx.db.new_transaction_at(&fx.input).await.unwrap();
        let source = Staging::at(&fx.db, &fx.input, &source_txn, "conflict")
            .await
            .unwrap();
        source
            .replace_context(&sibling_txn, &sibling, FOLD.into())
            .await
            .unwrap()
            .unwrap();
        stage_observation(
            &sibling_txn,
            Observation::CompactionSettled {
                id: sibling.id,
                status: CompactionSettlement::Applied,
            },
        )
        .await
        .unwrap();
        sibling_txn.commit().await.unwrap();
        assert!(
            scope
                .settle(original_intent.clone(), Some("Stale result".into()))
                .await
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
        assert!(
            scope
                .settle(original_intent, Some("Retry forbidden".into()))
                .await
                .is_err()
        );
        assert!(scope.reserve(members).await.is_err());
        let inspected = Box::pin(fx.operation_log()).await;
        assert!(
            inspected
                .provenance
                .atoms
                .values()
                .any(|a| a.contains("CompactionSettled") && a.contains("Stale"))
        );
        assert_eq!(
            inspected
                .operations
                .atoms
                .values()
                .filter(|a| a.contains("TransformationOutput"))
                .count(),
            1
        );
        assert!(Box::pin(print(&fx.db)).await.contains(FOLD));
        Box::pin(fx.assert_effects(1, 0, 0)).await;
    }
    Box::pin(exercise()).await;
}
