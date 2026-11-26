use super::*;
use crate::agent_db::ScheduleReference;
use crate::runtime::{RuntimeMessage, RuntimeRecord, RuntimeRecorder};
use crate::schedule_state::{lifecycle, readers::DueSlot};
use crate::session::steering::JobInputState;

async fn prepared(fx: &Fixture, committed: bool) -> ScheduleReference {
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    // Direct input fixtures must install a routable runtime
    // Agent. The common runner must resolve this exact Agent, not its default.
    let mut runtime_agent = fx
        .server
        .agents
        .default_agent()
        .expect("fixture runtime Agent");
    runtime_agent.name = "timer".into();
    fx.server.agents.upsert(runtime_agent);
    let agent = fx
        .registry
        .ensure_scheduling_agent_db(&entry)
        .await
        .unwrap();
    let definition = agent.find_schedule("wake").await.unwrap().unwrap();
    for _ in 0..100 {
        if agent
            .reconcile_schedule_accounting(&definition, 32)
            .await
            .unwrap()
            .complete
        {
            break;
        }
    }
    let Trigger::OneShot { fire_at } = definition.trigger else {
        panic!("one shot fixture");
    };
    let row = lifecycle::prepare(
        &agent,
        "wake",
        &definition.revision_key().unwrap(),
        DueSlot::Calendar { due_at: fire_at },
        Utc::now(),
    )
    .await
    .unwrap();
    let mut session = fx.session().await;
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    if committed {
        lifecycle::handoff(&agent, &row.reference, &session, Utc::now())
            .await
            .unwrap();
    }
    row.reference
}

async fn user(session: &mut Session, text: &str) -> TurnRequestId {
    session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: text.into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap()
}

async fn running(fx: &Fixture) -> (crate::test_support::MockCallGate, TurnAttempt) {
    fx.model.push_text("provisional");
    fx.model.push_text("final");
    let gate = fx.model.block_next_call();
    let mut session = fx.session().await;
    user(&mut session, "ordinary containing human turn").await;
    tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
        .await
        .unwrap();
    let attempt = fx
        .server
        .turns
        .lock()
        .await
        .get(&fx.db.root_id().to_string())
        .unwrap()
        .attempt
        .clone();
    (gate, attempt)
}

fn recorder(
    fx: &Fixture,
    session: Arc<Mutex<Session>>,
    attempt: &TurnAttempt,
) -> crate::session::turn_recorder::TurnRecorder {
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    Session::turn_recorder(
        session,
        attempt,
        TurnCallIdentity {
            agent: "timer".into(),
            requested_model: "stub".into(),
            execution_id: None,
        },
        TurnRecorderOptions {
            scheduled: Some(Box::new(scheduled_input::ScheduledInputs::new(
                Arc::downgrade(&fx.server),
                entry,
            ))),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn scheduled_common_active_input_is_private_once_and_not_a_later_turn() {
    let fx = Fixture::new(true).await;
    let reference = prepared(&fx, true).await;
    // The inert real Ack does not start an ordinary turn.
    assert!(fx.model.recorded_calls().is_empty());
    let (gate, attempt) = running(&fx).await;
    fx.server
        .bind_active_schedule_input(&fx.db.root_id().to_string(), &reference)
        .await
        .unwrap();
    gate.release();
    fx.idle().await;
    let calls = fx.model.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert!(
        !calls[0]
            .messages
            .iter()
            .any(|m| matches!(m, RuntimeMessage::System(text) if text == PRIVATE))
    );
    assert_eq!(
        calls[1]
            .messages
            .iter()
            .filter(|m| matches!(m, RuntimeMessage::System(text) if text == PRIVATE))
            .count(),
        1
    );
    let mut reopened = fx.session().await;
    let evidence = reopened
        .schedule_delivery_evidence(&reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(evidence.input.attempt_id, attempt.attempt_id);
    assert!(matches!(
        evidence.input.state,
        JobInputState::Included { model_sequence: 1 }
    ));
    assert!(evidence.completion.is_some());
    let records = reopened.turn_transcript(&attempt.attempt_id).await.unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r.message, TurnTranscriptMessage::ScheduledInput { .. }))
            .count(),
        1
    );
    assert!(!serde_json::to_string(&records).unwrap().contains(PRIVATE));
    assert!(
        !reopened
            .entries()
            .iter()
            .any(|entry| entry.content.contains(PRIVATE))
    );
    assert!(
        reopened.turn_inputs().await.unwrap().is_empty(),
        "private text never becomes ordinary steering"
    );
    let gate = fx.model.block_next_call();
    fx.model.push_text("independent final");
    user(&mut reopened, "later independent question").await;
    tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
        .await
        .unwrap();
    gate.release();
    fx.idle().await;
    assert_eq!(fx.model.recorded_calls().len(), 3);
    assert!(
        !fx.model.recorded_calls()[2]
            .messages
            .iter()
            .any(|m| matches!(m, RuntimeMessage::System(text) if text == PRIVATE))
    );
    assert_eq!(
        fx.view().await.list_schedule_fires().await.unwrap().len(),
        1
    );
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_common_orphan_wrong_agent_and_revoked_revision_never_dispatch() {
    for change in ["orphan", "wrong-agent", "disable", "remove", "replace"] {
        let fx = Fixture::new(true).await;
        let reference = prepared(&fx, change != "orphan").await;
        let (gate, attempt) = running(&fx).await;
        let sid = fx.db.root_id().to_string();
        if change == "wrong-agent" {
            fx.server
                .turns
                .lock()
                .await
                .get_mut(&sid)
                .unwrap()
                .agent_db_id = Some("different-Agent".into());
        }
        let bound = fx.server.bind_active_schedule_input(&sid, &reference).await;
        if matches!(change, "orphan" | "wrong-agent") {
            assert!(bound.is_err(), "{change}");
        } else {
            bound.unwrap();
            let agent = fx.view().await;
            let mut definition = agent.find_schedule("wake").await.unwrap().unwrap();
            match change {
                "remove" => {
                    agent.remove_schedule("wake").await.unwrap();
                }
                "disable" => {
                    definition.enabled = false;
                    agent.upsert_schedule(definition).await.unwrap();
                }
                "replace" => {
                    definition.prompt = "new private revision".into();
                    agent.upsert_schedule(definition).await.unwrap();
                }
                _ => unreachable!(),
            }
        }
        gate.release();
        fx.idle().await;
        assert_eq!(fx.model.recorded_calls().len(), 1, "{change}");
        assert!(
            !fx.model.recorded_calls()[0]
                .messages
                .iter()
                .any(|m| matches!(m, RuntimeMessage::System(text) if text == PRIVATE))
        );
        let session = fx.session().await;
        assert!(
            !serde_json::to_string(&session.turn_transcript(&attempt.attempt_id).await.unwrap())
                .unwrap()
                .contains(PRIVATE)
        );
        fx.server.shutdown().await;
    }
}

#[tokio::test]
async fn scheduled_common_projection_and_late_binding_refuse_before_inclusion() {
    let fx = Fixture::new(true).await;
    let reference = prepared(&fx, true).await;
    // Hold the ordinary local slot while exercising its recorder directly.
    fx.server
        .processing
        .lock()
        .await
        .insert(fx.db.root_id().to_string());
    let mut session = fx.session().await;
    let request = user(&mut session, "manual boundary").await;
    let attempt = session.start_turn_attempt(request).await.unwrap();
    let recorder = recorder(&fx, Arc::new(Mutex::new(session)), &attempt);
    let first = recorder.input_boundary(0, false).await.unwrap();
    assert!(first.is_empty());
    // Binding between selection and dispatch cannot change the already
    // prepared request. Its private input is for the next safe boundary.
    let session = fx.session().await;
    session
        .bind_schedule_input(&reference, &attempt.attempt_id)
        .await
        .unwrap();
    recorder.input_dispatch(0, &first, &first).await.unwrap();
    assert_eq!(
        session
            .schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .unwrap()
            .input
            .state,
        JobInputState::Queued,
    );
    let messages = recorder.input_boundary(1, false).await.unwrap();
    assert_eq!(messages, vec![RuntimeMessage::System(PRIVATE.into())]);
    assert!(recorder.input_dispatch(1, &[], &messages).await.is_err());
    let session = fx.session().await;
    assert!(matches!(
        session
            .schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .unwrap()
            .input
            .state,
        JobInputState::Accepted
    ));
    assert!(
        session
            .turn_transcript(&attempt.attempt_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(fx.model.recorded_calls().is_empty());
    // A revision change AFTER selection must also fail at the final boundary.
    let agent = fx.view().await;
    let mut definition = agent.find_schedule("wake").await.unwrap().unwrap();
    definition.prompt = "replaced after selection".into();
    agent.upsert_schedule(definition).await.unwrap();
    assert!(
        recorder
            .input_dispatch(1, &messages, &messages)
            .await
            .is_err()
    );
    recorder.finish_scheduled_inputs().await.unwrap();
    recorder.input_boundary(1, true).await.unwrap();
    // A second exact reference cannot enter an already-closed containing turn.
    let mut another = reference.clone();
    another.delivery_id = "late-delivery".into();
    let mut session = fx.session().await;
    session.prepare_schedule_input(&another).await.unwrap();
    assert!(
        session
            .bind_schedule_input(&another, &attempt.attempt_id)
            .await
            .is_err()
    );
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_common_exact_wait_reconstructs_references_not_generic_replay() {
    let fx = Fixture::new(true).await;
    let reference = prepared(&fx, true).await;
    fx.server
        .processing
        .lock()
        .await
        .insert(fx.db.root_id().to_string());
    let mut session = fx.session().await;
    let request = user(&mut session, "exact wait").await;
    let attempt = session.start_turn_attempt(request.clone()).await.unwrap();
    session
        .bind_schedule_input(&reference, &attempt.attempt_id)
        .await
        .unwrap();
    session
        .submit_turn_input(crate::session::steering::JobInputRequest::new(
            attempt.attempt_id.clone(),
            "ordinary same-call steering".into(),
        ))
        .await
        .unwrap();
    let snapshot = session.database().snapshot().await.unwrap();
    let recorder = recorder(&fx, Arc::new(Mutex::new(session)), &attempt);
    let messages = recorder.input_boundary(0, false).await.unwrap();
    recorder
        .input_dispatch(0, &messages, &messages)
        .await
        .unwrap();
    let arguments = json!({"session_db_id":"child", "timeout_seconds":240}).to_string();
    recorder
        .record(RuntimeRecord::ModelResponse {
            model_sequence: 0,
            content: None,
            tool_calls: vec![runtime::ToolCallRequest {
                id: "wait-call".into(),
                name: "job_wait".into(),
                arguments: arguments.clone(),
            }],
            provider_extra: Default::default(),
            metadata: None,
            terminal: false,
        })
        .await
        .unwrap();
    let now = Utc::now();
    let checkpoint = crate::session::wait_resume::JobWaitCheckpoint::new(
        request.clone(),
        attempt.attempt_id.clone(),
        "timer".into(),
        snapshot,
        runtime::JobWaitIntent {
            model_sequence: 0,
            call_index: 0,
            call_id: "wait-call".into(),
            arguments,
            child_session_db_id: "child".into(),
            checkpointed_at: now,
            deadline: now + chrono::Duration::seconds(240),
            policy_timeout_seconds: 240,
        },
    );
    let reopened = fx.session().await;
    assert!(
        reopened
            .resumable_job_wait(&request, checkpoint.clone())
            .await
            .unwrap()
            .is_err(),
        "generic planner has no private resolver"
    );
    let plan = fx
        .server
        .resumable_scheduled_wait(&reopened, &request, checkpoint.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.next_sequence, 3);
    assert_eq!(&plan.runtime.prior_messages[..messages.len()], &messages);
    assert_eq!(
        plan.runtime.prior_messages[1],
        RuntimeMessage::System(PRIVATE.into())
    );
    assert!(
        !serde_json::to_string(&reopened.turn_transcript(&attempt.attempt_id).await.unwrap())
            .unwrap()
            .contains(PRIVATE)
    );
    let agent = fx.view().await;
    let mut definition = agent.find_schedule("wake").await.unwrap().unwrap();
    definition.enabled = false;
    agent.upsert_schedule(definition).await.unwrap();
    let retained = fx
        .server
        .resumable_scheduled_wait(&reopened, &request, checkpoint.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retained.runtime.prior_messages[1],
        RuntimeMessage::System(PRIVATE.into()),
        "disable after Included is prospective, not cancellation of a human wait"
    );
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    fx.registry
        .detach_agent_from_session(&fx.db.root_id().to_string(), &entry)
        .await
        .unwrap();
    let (conv, db, key) = fx
        .registry
        .open_session_with_signer(&fx.db.root_id().to_string())
        .await
        .unwrap();
    let unauthorized = Session::new(conv, db).await.with_signing_key(key);
    assert!(
        fx.server
            .resumable_scheduled_wait(&unauthorized, &request, checkpoint)
            .await
            .unwrap()
            .is_err(),
        "current exact-Agent membership is still required after Included"
    );
    assert!(
        fx.model.recorded_calls().is_empty(),
        "reconstruction runs neither model nor earlier effects"
    );
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_common_required_write_failure_cannot_promote_or_dispatch() {
    let fx = Fixture::new(true).await;
    let reference = prepared(&fx, true).await;
    fx.server
        .processing
        .lock()
        .await
        .insert(fx.db.root_id().to_string());
    let mut session = fx.session().await;
    let request = user(&mut session, "native write refusal").await;
    let attempt = session.start_turn_attempt(request).await.unwrap();
    session
        .bind_schedule_input(&reference, &attempt.attempt_id)
        .await
        .unwrap();
    let held = Arc::new(Mutex::new(session));
    let recorder = recorder(&fx, held.clone(), &attempt);
    let messages = recorder.input_boundary(0, false).await.unwrap();
    let before = fx.db.snapshot().await.unwrap();
    // A real native no-key Session, not a mock that merely says writes failed.
    let read_only = eidetica::Database::open(fx.registry.instance(), fx.db.root_id())
        .await
        .unwrap();
    *held.lock().await = Session::new(ConversationId(fx.db.root_id().to_string()), read_only).await;
    assert!(
        recorder
            .input_dispatch(0, &messages, &messages)
            .await
            .is_err()
    );
    assert_eq!(before, fx.db.snapshot().await.unwrap());
    let session = fx.session().await;
    assert!(
        session
            .turn_transcript(&attempt.attempt_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        session
            .schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .unwrap()
            .input
            .state,
        JobInputState::Accepted
    );
    assert!(fx.model.recorded_calls().is_empty());
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_common_stop_is_unused_and_never_restarts_containing_turn() {
    let fx = Fixture::new(true).await;
    let reference = prepared(&fx, true).await;
    let (gate, attempt) = running(&fx).await;
    let sid = fx.db.root_id().to_string();
    fx.server
        .bind_active_schedule_input(&sid, &reference)
        .await
        .unwrap();
    fx.server
        .turns
        .lock()
        .await
        .get(&sid)
        .unwrap()
        .cancel
        .cancel();
    fx.idle().await;
    gate.wait_stopped().await;
    let session = fx.session().await;
    let evidence = session
        .schedule_delivery_evidence(&reference)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        evidence.input.state,
        JobInputState::NotApplied { .. }
    ));
    assert_eq!(evidence.input.attempt_id, attempt.attempt_id);
    assert_eq!(
        evidence.completion.unwrap().outcome,
        crate::session::outcome::TurnOutcome::Cancelled
    );
    assert_eq!(fx.model.recorded_calls().len(), 1);
    assert!(
        fx.server
            .bind_active_schedule_input(&sid, &reference)
            .await
            .is_err()
    );
    fx.server.shutdown().await;
}
