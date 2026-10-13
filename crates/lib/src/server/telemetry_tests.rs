//! Exercise the real session recorder on both sides of atomic settlement.
use super::*;
use crate::runtime::{ResponseMetadata, RuntimeRecord, RuntimeRecorder, TokenUsage};
use crate::session::usage::{SessionTelemetry, telemetry_at_snapshot};

async fn observe(db: &eidetica::Database) -> SessionTelemetry {
    let tips = db.snapshot().await.unwrap();
    let entries = Session::entries_at_snapshot(db, &tips).await.unwrap();
    telemetry_at_snapshot(db, &tips, &entries).await.unwrap()
}

fn message(sender: &str, content: &str) -> SessionEntry {
    SessionEntry {
        sender: sender.into(),
        content: content.into(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    }
}

fn metadata(prompt: u32, completion: u32, cached: u32, cost: f64) -> ResponseMetadata {
    ResponseMetadata {
        model: "gpt-6.1-sol".into(),
        usage: TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: Some(cached),
            cost_usd: Some(cost),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn terminal_telemetry_precedes_publication_and_settles_once() {
    let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
    let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
    let request = session.add_entry(message("user", "fixture")).await.unwrap();
    let attempt = session.start_turn_attempt(request.clone()).await.unwrap();
    let session = Arc::new(Mutex::new(session));
    let recorder = SessionRuntimeRecorder {
        session: session.clone(),
        request_id: request,
        attempt_id: attempt.attempt_id.clone(),
        sequence: AtomicU32::new(0),
        terminal: Default::default(),
        explicit_bridge_send: AtomicBool::new(false),
        agent_name: "host".into(),
        requested_model: "gpt-sol-latest".into(),
        context_snapshot: None,
        branch: None,
        fail_after: Default::default(),
    };
    for (sequence, prompt, terminal) in [(0, 1000, false), (1, 2000, false), (2, 3000, true)] {
        recorder
            .record(RuntimeRecord::ModelResponse {
                model_sequence: sequence,
                content: terminal.then(|| "FINAL".into()),
                tool_calls: Vec::new(),
                provider_extra: Default::default(),
                metadata: Some(metadata(prompt, 100, prompt / 2, 0.01)),
                terminal,
            })
            .await
            .unwrap();
        let live = observe(&db).await;
        assert_eq!(live.totals.calls, sequence as u32 + 1);
        assert_eq!(
            live.totals.prompt_tokens,
            (1..=sequence + 1).sum::<u64>() * 1000
        );
        assert_eq!(live.last_calls["host"].context_tokens, Some(prompt));
        assert_eq!(live.last_calls["host"].requested_model, "gpt-sol-latest");
        assert_eq!(
            live.last_calls["host"].wire_model.as_deref(),
            Some("gpt-6.1-sol")
        );
    }
    // Terminal provider usage must be visible while hooks/preflight still run.
    let held = Session::new(ConversationId("observer".into()), db.clone()).await;
    assert!(!held.entries().iter().any(|entry| entry.content == "FINAL"));
    assert_eq!(Session::active_turn_attempts(&db).await.unwrap().len(), 1);
    let live = observe(&db).await;
    let terminal = recorder.terminal.lock().unwrap().take().unwrap();
    let mut final_entry = message("host", "FINAL");
    final_entry.metadata = Some(metadata(6000, 300, 3000, 0.03));
    final_entry.metadata.as_mut().unwrap().context_tokens = Some(3000);
    session
        .lock()
        .await
        .complete_turn_attempt_with_transcript(&attempt, Some(final_entry), Some(terminal))
        .await
        .unwrap();
    let finalized = observe(&db).await;
    assert_eq!(finalized.totals.prompt_tokens, live.totals.prompt_tokens);
    assert_eq!(finalized.totals.completion_tokens, 300);
    assert_eq!(finalized.totals.cached_tokens, 3000);
    assert!((finalized.totals.cost_usd - 0.03).abs() < 1e-12);
    assert_eq!(finalized.totals.calls, 3);
    let reopened = Session::new(ConversationId("observer".into()), db.clone()).await;
    assert_eq!(
        reopened
            .turn_transcript(&attempt.attempt_id)
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        reopened
            .entries()
            .last()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .turn_attempt_id
            .as_deref(),
        Some(attempt.attempt_id.as_str())
    );
    assert!(Session::active_turn_attempts(&db).await.unwrap().is_empty());

    // A stopped turn's Error also contains an aggregate, not another bill.
    let stopped_attempt = {
        let mut session = session.lock().await;
        let request = session
            .add_entry(message("user", "stop fixture"))
            .await
            .unwrap();
        session.start_turn_attempt(request).await.unwrap()
    };
    let stopped = SessionRuntimeRecorder {
        request_id: stopped_attempt.request_id.clone(),
        attempt_id: stopped_attempt.attempt_id.clone(),
        sequence: AtomicU32::new(0),
        terminal: Default::default(),
        ..recorder
    };
    stopped
        .record(RuntimeRecord::ModelResponse {
            model_sequence: 0,
            content: None,
            tool_calls: Vec::new(),
            provider_extra: Default::default(),
            metadata: Some(metadata(100, 10, 50, 0.01)),
            terminal: false,
        })
        .await
        .unwrap();
    stopped.record(RuntimeRecord::Cancelled).await.unwrap();
    let mut error = message("host", "Stopped.");
    error.entry_type = EntryType::Error;
    error.metadata = Some(metadata(100, 10, 50, 0.01));
    let terminal = stopped.terminal.lock().unwrap().take();
    session
        .lock()
        .await
        .complete_turn_attempt_with_transcript(&stopped_attempt, Some(error), terminal)
        .await
        .unwrap();
    let stopped_totals = observe(&db).await;
    assert_eq!(stopped_totals.totals.prompt_tokens, 6100);
    assert_eq!(stopped_totals.totals.calls, 4);
    assert_eq!(stopped_totals.last_calls["host"].context_tokens, Some(100));
}
