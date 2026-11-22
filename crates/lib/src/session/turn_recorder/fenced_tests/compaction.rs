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
    let old = "Historical detail unique to original source. ".repeat(170);
    let mut fx = Box::pin(Fixture::build(false, None, Some(&old), |req: &Request| {
        let body: Value = req.body_json().unwrap();
        if asks_summary(&body) {
            text_response(FOLD)
        } else {
            text_response(BODY)
        }
    }))
    .await;
    let seed = Box::pin(fx.run()).await.unwrap();
    Box::pin(fx.complete(&seed.body)).await.unwrap();
    Box::pin(fx.enable_automatic()).await;
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
    let wire = Box::pin(fx.assert_effects(3, 0, 0)).await;
    assert!(asks_summary(&wire[1]));
    assert!(contents(&wire[1], &old));
    assert!(!contents(&wire[1], QUESTION) && !contents(&wire[1], CORRECTION));
    assert!(contents(&wire[2], FOLD) && contents(&wire[2], QUESTION));
    assert!(!contents(&wire[2], CORRECTION));
    let reopened = fx.reopened().await;
    let joined = Session::history(&reopened).await.unwrap();
    let prepared = joined
        .recorded_call(&fx.call(0), &fx.recorder.attempt.attempt_id)
        .await
        .unwrap()
        .unwrap();
    let seen = Session::history_at(&reopened, &prepared.observed.frontier)
        .await
        .unwrap()
        .display_rows()
        .await
        .unwrap();
    assert!(seen.iter().any(|r| r.content == FOLD));
    assert!(!seen.iter().any(|r| r.content == CORRECTION));
    let isolated_append = Session::history_at(&reopened, &sibling)
        .await
        .unwrap()
        .display_rows()
        .await
        .unwrap();
    assert!(isolated_append.iter().any(|r| r.content == CORRECTION));
    assert!(!isolated_append.iter().any(|r| r.content == FOLD));
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
    assert_eq!(displayed.iter().filter(|r| r.content == FOLD).count(), 1);
    assert_eq!(
        displayed.iter().filter(|r| r.content == QUESTION).count(),
        1
    );
    assert!(!displayed.iter().any(|r| r.content == old));
    let inspect = Box::pin(fx.operation_log()).await;
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
            Some("Original historical input"),
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
