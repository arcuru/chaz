use super::*;
use crate::agent_db::ScheduleReference;
use crate::schedule_state::{lifecycle, readers::DueSlot};
use crate::session::steering::JobInputState;

#[derive(Default)]
struct ClosingPause {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    armed: AtomicBool,
}
struct ClosingExtension(Arc<ClosingPause>);
struct ClosingInstance {
    manifest: crate::extension::manifest::ExtensionManifest,
    pause: Arc<ClosingPause>,
}
impl crate::extension::Extension for ClosingExtension {
    fn name(&self) -> &'static str {
        "closing-pause"
    }
    fn supported_hooks(&self) -> &[crate::extension::HookKind] {
        &[crate::extension::HookKind::AgentEnd]
    }
    fn instantiate<'a>(
        &'a self,
        _: crate::extension::ScopeCtx<'a>,
    ) -> crate::extension::instance::InstantiateFuture<'a> {
        Box::pin(async move {
            Ok(Arc::new(ClosingInstance {
                manifest: self.manifest(),
                pause: self.0.clone(),
            })
                as Arc<dyn crate::extension::ExtensionInstance>)
        })
    }
}
impl crate::extension::ExtensionInstance for ClosingInstance {
    fn manifest(&self) -> &crate::extension::manifest::ExtensionManifest {
        &self.manifest
    }
    fn agent_end_hook(&self) -> Option<Arc<dyn crate::extension::handler::HookHandlerAgentEnd>> {
        Some(self.pause.clone())
    }
}
impl crate::extension::handler::HookHandlerAgentEnd for ClosingPause {
    fn on_agent_end<'a>(&'a self) -> crate::extension::handler::HandlerFuture<'a, ()> {
        Box::pin(async move {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

#[tokio::test]
async fn scheduled_closed_boundary_refusal_is_unused_then_delivers_one_idle_input() {
    let pause = Arc::new(ClosingPause::default());
    pause.armed.store(true, Ordering::SeqCst);
    let mut hub = crate::test_support::context_hub().await;
    hub.install_all(vec![Arc::new(ClosingExtension(pause.clone()))])
        .await
        .unwrap();
    let fx = Fixture::with_hub(true, Arc::new(hub)).await;
    let (reference, sid) = prepared(&fx, false, true).await;
    fx.model
        .push_text("ordinary final before the late delivery");
    let mut s = session(&fx, &sid).await;
    s.add_entry(SessionEntry {
        sender: "human".into(),
        content: "ordinary closing turn".into(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), pause.entered.notified())
        .await
        .unwrap();
    let human = fx
        .server
        .turns
        .lock()
        .await
        .get(&sid)
        .unwrap()
        .attempt
        .clone();
    // The real runtime closed input, but still owns the containing turn.
    // A positive refusal creates no binding or uncertain Session receipt.
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .is_err()
    );
    assert!(
        s.schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(s.attempts_for_test().await.len(), 1);
    pause.release.notify_one();
    idle(&fx, &sid).await;
    fx.model.push_text("late private delivery completed");
    let scheduled = fx
        .server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        scheduled, human.attempt_id,
        "never restart the containing human turn"
    );
    idle(&fx, &sid).await;
    let evidence = s
        .schedule_delivery_evidence(&reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(evidence.input.attempt_id, scheduled);
    assert!(matches!(
        evidence.input.state,
        JobInputState::Included { .. }
    ));
    assert_eq!(fx.model.recorded_calls().len(), 2);
    assert!(
        !fx.model.recorded_calls()[0]
            .messages
            .iter()
            .any(|m| matches!(m, crate::runtime::RuntimeMessage::System(text) if text == PRIVATE))
    );
    assert_eq!(
        fx.model.recorded_calls()[1]
            .messages
            .iter()
            .filter(
                |m| matches!(m, crate::runtime::RuntimeMessage::System(text) if text == PRIVATE)
            )
            .count(),
        1
    );
    assert_eq!(
        fx.view().await.list_schedule_fires().await.unwrap().len(),
        1
    );
    assert_eq!(s.attempts_for_test().await.len(), 2);
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .is_none()
    );
    fx.server.shutdown().await;
}

struct IncludedPause {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[tokio::test]
async fn scheduled_input_racing_the_final_scan_cannot_be_stranded_as_unused() {
    let fx = Fixture::new(true).await;
    let (reference, sid) = prepared(&fx, false, true).await;
    let barrier = Arc::new(crate::extensions::agent_schedule::ScheduleDispatchBarrier::default());
    *fx.server.schedule_closing_barrier.lock().unwrap() = Some(barrier.clone());
    fx.model.push_text("containing final");
    let mut s = session(&fx, &sid).await;
    s.add_entry(SessionEntry {
        sender: "human".into(),
        content: "finish the containing turn".into(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), barrier.parked.notified())
        .await
        .unwrap();
    // Pause AFTER the private-input scan but BEFORE the ordinary close commit.
    // If binding is not serialized with this interval, force publication into
    // the gap before releasing it. Otherwise the binder must wait for closure.
    let serialized = fx.server.turns.try_lock().is_err();
    let late = tokio::spawn({
        let server = fx.server.clone();
        let sid = sid.clone();
        let reference = reference.clone();
        async move { server.deliver_schedule_input(&sid, &reference).await }
    });
    let pending = if serialized {
        Some(late)
    } else {
        let _ = tokio::time::timeout(Duration::from_secs(10), late)
            .await
            .unwrap()
            .unwrap();
        None
    };
    *fx.server.schedule_closing_barrier.lock().unwrap() = None;
    barrier.release.notify_one();
    if let Some(late) = pending {
        let _ = tokio::time::timeout(Duration::from_secs(10), late)
            .await
            .unwrap()
            .unwrap();
    }
    idle(&fx, &sid).await;
    fx.model.push_text("private idle final");
    let _ = fx
        .server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap();
    idle(&fx, &sid).await;
    assert_eq!(
        fx.model.recorded_calls().len(),
        2,
        "one containing call and one private idle call"
    );
    assert_eq!(
        fx.model
            .recorded_calls()
            .iter()
            .flat_map(|call| &call.messages)
            .filter(
                |m| matches!(m, crate::runtime::RuntimeMessage::System(text) if text == PRIVATE)
            )
            .count(),
        1
    );
    let evidence = s
        .schedule_delivery_evidence(&reference)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        evidence.input.state,
        JobInputState::Included { .. }
    ));
    assert_eq!(
        fx.view().await.list_schedule_fires().await.unwrap().len(),
        1
    );
    fx.server.shutdown().await;
}

impl Tool for IncludedPause {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "included_pause".into(),
            description: "offline request boundary barrier".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok("continue the same turn".into())
        })
    }
}

struct PanicTool;
impl Tool for PanicTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "panic_tool".into(),
            description: "offline task panic".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async { panic!("deliberate scheduled tool panic") })
    }
}

async fn reconcile(agent: &AgentDb, schedule: &Schedule) {
    for _ in 0..100 {
        if agent
            .reconcile_schedule_accounting(schedule, 32)
            .await
            .unwrap()
            .complete
        {
            return;
        }
    }
    panic!("bounded accounting did not finish");
}

pub(super) async fn prepared(
    fx: &Fixture,
    fresh: bool,
    committed: bool,
) -> (ScheduleReference, String) {
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    let mut runtime_agent = fx
        .server
        .agents
        .default_agent()
        .expect("fixture runtime Agent");
    runtime_agent.name = entry.display_name.clone();
    fx.server.agents.upsert(runtime_agent);
    let mut definition = fx.adb.find_schedule("wake").await.unwrap().unwrap();
    if fresh {
        definition.target = ScheduleTarget::Fresh;
        fx.adb.upsert_schedule(definition).await.unwrap();
    }
    let agent = fx
        .registry
        .ensure_scheduling_agent_db(&entry)
        .await
        .unwrap();
    let definition = agent.find_schedule("wake").await.unwrap().unwrap();
    reconcile(&agent, &definition).await;
    let slot = match definition.trigger {
        Trigger::OneShot { fire_at } => DueSlot::Calendar { due_at: fire_at },
        Trigger::Interval { period } => DueSlot::Interval {
            anchor: Utc::now() - chrono::Duration::from_std(period).unwrap(),
            ordinal: 1,
        },
        _ => panic!("unsupported fixture trigger"),
    };
    let row = lifecycle::prepare(
        &agent,
        "wake",
        &definition.revision_key().unwrap(),
        slot,
        Utc::now(),
    )
    .await
    .unwrap();
    let sid = if fresh {
        let (_, db) = fx
            .server
            .prepare_fresh_schedule_root(&row.reference)
            .await
            .unwrap();
        db.root_id().to_string()
    } else {
        fx.db.root_id().to_string()
    };
    let (conv, db, key) = fx.registry.open_session_with_signer(&sid).await.unwrap();
    let mut session = Session::new(conv, db).await.with_signing_key(key);
    session
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    if committed {
        // Fresh membership/delegation can advance the selected writer identity.
        let agent = fx.view().await;
        reconcile(&agent, &definition).await;
        lifecycle::handoff(&agent, &row.reference, &session, Utc::now())
            .await
            .unwrap();
    }
    (row.reference, sid)
}

async fn session(fx: &Fixture, sid: &str) -> Session {
    let (conv, db, key) = fx.registry.open_session_with_signer(sid).await.unwrap();
    Session::new(conv, db).await.with_signing_key(key)
}

async fn idle(fx: &Fixture, sid: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while fx.server.processing.lock().await.contains(sid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn explicit_retry_runs_fresh_in_the_same_session_after_terminal_failure() {
    use crate::session::outcome::TurnOutcome;
    let fx = Fixture::new(true).await;
    let (original, sid) = prepared(&fx, true, true).await;
    fx.model.push_err(crate::error::LlmError::InvalidRequest {
        message: "stubbed ordinary failure".into(),
    });
    let failed = fx
        .server
        .deliver_schedule_input(&sid, &original)
        .await
        .unwrap()
        .unwrap();
    idle(&fx, &sid).await;
    let s = session(&fx, &sid).await;
    assert_eq!(
        s.turn_completion(&failed).await.unwrap().unwrap().outcome,
        TurnOutcome::Failure
    );
    let agent = fx.view().await;
    assert!(agent.effective_schedule("wake").await.unwrap().is_none());
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    assert!(
        fx.server
            .retry_schedule_delivery(
                &fx.db.root_id().to_string(),
                &entry,
                &original.occurrence_id,
                &original.delivery_id,
                &failed
            )
            .await
            .is_err()
    );
    let retried = fx
        .server
        .retry_schedule_delivery(
            &sid,
            &entry,
            &original.occurrence_id,
            &original.delivery_id,
            &failed,
        )
        .await
        .unwrap();
    assert_eq!(s.database().root_id().to_string(), sid);
    assert!(
        s.schedule_delivery_evidence(&retried)
            .await
            .unwrap()
            .is_none()
    );
    fx.model.push_text("retry final from the exact Agent");
    let attempt = fx
        .server
        .deliver_schedule_input(&sid, &retried)
        .await
        .unwrap()
        .unwrap();
    idle(&fx, &sid).await;
    assert_ne!(attempt, failed);
    assert_eq!(
        s.turn_completion(&attempt).await.unwrap().unwrap().outcome,
        TurnOutcome::Success
    );
    assert_eq!(agent.list_schedule_fires().await.unwrap().len(), 2);
    assert!(agent.effective_schedule("wake").await.unwrap().is_none());
    assert_eq!(fx.model.recorded_calls().len(), 2);
}

#[tokio::test]
async fn interruption_retry_through_pumps_revalidates_stop_and_does_not_restart_prior_request() {
    use super::super::{scheduled_delivery::DeliveryPump, scheduled_projection::ProjectionPump};
    use crate::session::outcome::TurnOutcome;
    use crate::session::scheduled::ScheduleExecutor;
    for stop_after_allocation in [false, true] {
        let fx = Fixture::new(true).await;
        let mut definition = fx.adb.find_schedule("wake").await.unwrap().unwrap();
        definition.interruption_retries = std::num::NonZeroU32::new(1);
        fx.adb.upsert_schedule(definition).await.unwrap();
        let (original, sid) = prepared(&fx, !stop_after_allocation, true).await;
        let mut s = session(&fx, &sid).await;
        let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
        let prior_executor = ScheduleExecutor {
            incarnation: uuid::Uuid::new_v4().to_string(),
            ..fx.server.schedule_executor(&entry)
        };
        let prior = s
            .start_schedule_input_as(&original, "crashed-attempt".into(), Some(prior_executor))
            .await
            .unwrap()
            .unwrap();
        for state in [
            JobInputState::Accepted,
            JobInputState::Dispatching { model_sequence: 1 },
        ] {
            s.record_schedule_input(&original, &prior.attempt_id, state)
                .await
                .unwrap();
        }
        let view = fx.view().await;
        let mut projections = ProjectionPump::default();
        fx.server
            .live_attempts
            .lock()
            .await
            .insert(prior.attempt_id.clone());
        for _ in 0..12 {
            projections.step(&fx.server).await;
        }
        assert_eq!(view.list_schedule_fires().await.unwrap().len(), 1);
        fx.server
            .live_attempts
            .lock()
            .await
            .remove(&prior.attempt_id);
        for _ in 0..20 {
            projections.step(&fx.server).await;
            if view.list_schedule_fires().await.unwrap().len() == 2 {
                break;
            }
        }
        let retry = view
            .schedule_occurrences()
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.reference.delivery_id != original.delivery_id)
            .expect("the projection pump must durably allocate the retry");
        assert_eq!(retry.reference.occurrence_id, original.occurrence_id);
        assert_eq!(
            s.attempts_for_test().await.len(),
            1,
            "allocation alone never starts a turn"
        );
        if stop_after_allocation {
            s.add_entry(SessionEntry {
                sender: "operator".into(),
                content: serde_json::to_string(&crate::session::CancelRequest {
                    request_id: prior.request_id.clone(),
                    attempt_id: prior.attempt_id.clone(),
                })
                .unwrap(),
                timestamp: Utc::now(),
                entry_type: EntryType::Cancel,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        } else {
            fx.model
                .push_text("recovery delivered via the common runner");
        }
        let mut deliveries = DeliveryPump::default();
        pump_steps(&mut deliveries, &fx, 12).await;
        idle(&fx, &sid).await;
        let evidence = s
            .schedule_delivery_evidence(&retry.reference)
            .await
            .unwrap();
        assert_eq!(
            fx.model.recorded_calls().len(),
            usize::from(!stop_after_allocation)
        );
        assert_eq!(
            s.read_turn_attempt(&prior.attempt_id)
                .await
                .unwrap()
                .unwrap(),
            prior
        );
        if stop_after_allocation {
            assert!(
                evidence.is_none(),
                "a late durable stop prevents starting the retry"
            );
            assert_eq!(s.attempts_for_test().await.len(), 1);
        } else {
            let evidence = evidence.unwrap();
            assert_ne!(evidence.input.request_id, prior.request_id);
            assert_eq!(evidence.completion.unwrap().outcome, TurnOutcome::Success);
        }
    }
}

#[tokio::test]
async fn scheduled_common_idle_and_fresh_use_real_ack_capacity_and_local_final_once() {
    for fresh in [false, true] {
        let fx = Fixture::new(true).await;
        let (reference, sid) = prepared(&fx, fresh, true).await;
        let anchor = session(&fx, &sid)
            .await
            .schedule_input(&reference)
            .await
            .unwrap()
            .unwrap();
        fx.model.push_text("private invocation completed");
        let gate = fx.model.block_next_call();
        let id = fx
            .server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
            .await
            .unwrap();
        let s = session(&fx, &sid).await;
        let attempt = s.attempts_for_test().await.pop().unwrap();
        assert_eq!(attempt.attempt_id, id);
        assert_eq!(attempt.request_id, anchor.request_id);
        assert_eq!(s.attempts_for_test().await.len(), 1);
        assert_eq!(
            fx.server.semaphore.available_permits(),
            MAX_CONCURRENT_LLM_CALLS - 1
        );
        assert!(
            s.schedule_delivery_evidence(&reference)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            fx.server
                .deliver_schedule_input(&sid, &reference)
                .await
                .unwrap()
                .is_none()
        );
        gate.release();
        idle(&fx, &sid).await;
        let s = session(&fx, &sid).await;
        let evidence = s
            .schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            evidence.input.state,
            JobInputState::Included { .. }
        ));
        assert_eq!(
            evidence.completion.unwrap().outcome,
            crate::session::outcome::TurnOutcome::Success
        );
        assert_eq!(
            s.entries()
                .iter()
                .filter(|entry| entry.entry_type == EntryType::Message
                    && entry.sender == "timer"
                    && entry.content == "private invocation completed")
                .count(),
            1
        );
        assert!(
            s.entries()
                .iter()
                .all(|entry| entry.routing.is_none() && !entry.content.contains(PRIVATE))
        );
        let transcript = s.turn_transcript(&id).await.unwrap();
        assert!(!format!("{transcript:?}").contains(PRIVATE));
        assert!(
            transcript.iter().any(|record| matches!(
                record.message,
                TurnTranscriptMessage::ScheduledInput { .. }
            ))
        );
        assert_eq!(fx.model.recorded_calls().len(), 1);
        assert!(fx.model.recorded_calls()[0].messages.iter().any(|message|
            matches!(message, crate::runtime::RuntimeMessage::System(text) if text == PRIVATE)));
        assert_eq!(
            fx.server.semaphore.available_permits(),
            MAX_CONCURRENT_LLM_CALLS
        );
        let views = fx.view().await.preview_scheduling().await.unwrap();
        let crate::schedule_state::readers::AccountingView::Current(state) = &views[0].accounting
        else {
            panic!("current accounting")
        };
        assert_eq!(
            (
                state.handoff_count,
                state.successful_fires,
                state.reservations
            ),
            (1, 1, 0)
        );
        assert!(
            fx.server
                .deliver_schedule_input(&sid, &reference)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(s.attempts_for_test().await.len(), 1);
    }
}

#[tokio::test]
async fn scheduled_common_projection_recovers_terminal_after_removal_without_redelivery() {
    let fx = Fixture::new(true).await;
    let (reference, sid) = prepared(&fx, true, true).await;
    *fx.server.schedule_fail_at.lock().unwrap() = Some("completed-projection");
    fx.model.push_text("successful private execution");
    let id = fx
        .server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    idle(&fx, &sid).await;
    let view = fx.view().await;
    let views = view.preview_scheduling().await.unwrap();
    let crate::schedule_state::readers::AccountingView::Current(state) = &views[0].accounting
    else {
        panic!("current accounting")
    };
    assert_eq!(
        (state.successful_fires, state.reservations),
        (0, 1),
        "Included Fresh is not terminal success"
    );
    let s = session(&fx, &sid).await;
    assert_eq!(
        s.turn_completion(&id).await.unwrap().unwrap().outcome,
        crate::session::outcome::TurnOutcome::Success
    );
    // This uses actual stored terminal evidence from the common runner, not a
    // synthetic success. Lost projection must not make the turn runnable again.
    fx.adb.remove_schedule("wake").await.unwrap();
    let mut pump = super::super::scheduled_projection::ProjectionPump::default();
    for _ in 0..40 {
        pump.step(&fx.server).await;
    }
    let view = fx.view().await;
    let db = view.scheduling_database().await.unwrap().unwrap();
    let txn = db.new_transaction().await.unwrap();
    let state = txn
        .get_store::<eidetica::store::Table<crate::schedule_state::readers::AccountingState>>(
            "schedule_state",
        )
        .await
        .unwrap()
        .get(&reference.lifetime)
        .await
        .unwrap();
    assert_eq!(
        (
            state.handoff_count,
            state.successful_fires,
            state.reservations
        ),
        (1, 1, 0)
    );
    let before = db.snapshot().await.unwrap();
    for _ in 0..40 {
        pump.step(&fx.server).await;
    }
    assert_eq!(
        before,
        db.snapshot().await.unwrap(),
        "projection is idempotent"
    );
    assert_eq!(session(&fx, &sid).await.attempts_for_test().await.len(), 1);
    assert_eq!(fx.model.recorded_calls().len(), 1);
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn scheduled_common_full_capacity_keeps_idle_and_fresh_unstarted_then_revalidates() {
    for fresh in [false, true] {
        let fx = Fixture::new(true).await;
        let (reference, sid) = prepared(&fx, fresh, true).await;
        let all = fx
            .server
            .semaphore
            .clone()
            .acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                fx.server.deliver_schedule_input(&sid, &reference)
            )
            .await
            .unwrap()
            .unwrap()
            .is_none()
        );
        let s = session(&fx, &sid).await;
        assert!(s.attempts_for_test().await.is_empty());
        assert!(
            s.schedule_delivery_evidence(&reference)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!fx.server.processing.lock().await.contains(&sid));
        assert!(fx.model.recorded_calls().is_empty());
        let mut definition = fx.adb.find_schedule("wake").await.unwrap().unwrap();
        definition.prompt = "replacement, not permission to use old wake".into();
        fx.adb.upsert_schedule(definition).await.unwrap();
        drop(all);
        assert!(
            fx.server
                .deliver_schedule_input(&sid, &reference)
                .await
                .is_err()
        );
        assert!(
            session(&fx, &sid)
                .await
                .attempts_for_test()
                .await
                .is_empty()
        );
        assert!(fx.model.recorded_calls().is_empty());
    }
    // Positive capacity release uses the same reference, not another admission.
    let fx = Fixture::new(true).await;
    let (reference, sid) = prepared(&fx, false, true).await;
    let all = fx
        .server
        .semaphore
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
        .await
        .unwrap();
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .is_none()
    );
    drop(all);
    fx.model.push_text("after capacity");
    fx.server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    idle(&fx, &sid).await;
    assert_eq!(fx.model.recorded_calls().len(), 1);
}

#[tokio::test]
async fn scheduled_common_idle_refuses_orphan_handoff_and_failed_start_without_effects() {
    let fx = Fixture::new(true).await;
    let (reference, sid) = prepared(&fx, false, false).await;
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .is_err()
    );
    assert!(
        session(&fx, &sid)
            .await
            .attempts_for_test()
            .await
            .is_empty()
    );
    let agent = fx.view().await;
    let definition = agent.find_schedule("wake").await.unwrap().unwrap();
    reconcile(&agent, &definition).await;
    lifecycle::handoff(&agent, &reference, &session(&fx, &sid).await, Utc::now())
        .await
        .unwrap();
    *fx.server.schedule_fail_at.lock().unwrap() = Some("common-start");
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .is_err()
    );
    assert!(
        session(&fx, &sid)
            .await
            .attempts_for_test()
            .await
            .is_empty()
    );
    assert!(fx.server.live_attempts.lock().await.is_empty());
    assert!(!fx.server.processing.lock().await.contains(&sid));
    assert_eq!(
        fx.server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
    assert!(fx.model.recorded_calls().is_empty());
    *fx.server.schedule_fail_at.lock().unwrap() = None;
    fx.model.push_text("one valid invocation");
    fx.server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    idle(&fx, &sid).await;
    assert_eq!(fx.model.recorded_calls().len(), 1);
}

#[tokio::test]
async fn scheduled_common_other_agent_stays_pending_then_idle_targets_owner_without_override() {
    let fx = Fixture::new(true).await;
    let (reference, sid) = prepared(&fx, false, true).await;
    let (other, _) = tests::seed_agent(&fx.server, &fx.registry, "other").await;
    let mut runtime_agent = fx
        .server
        .agents
        .default_agent()
        .expect("fixture runtime Agent");
    runtime_agent.name = "other".into();
    fx.server.agents.upsert(runtime_agent);
    fx.registry
        .attach_agent_to_session(&sid, &other)
        .await
        .unwrap();
    fx.server
        .sessions
        .lock()
        .await
        .get_mut(&sid)
        .unwrap()
        .agent_override = Some("other".into());
    fx.model.push_text("other finished");
    let gate = fx.model.block_next_call();
    let mut s = session(&fx, &sid).await;
    s.add_entry(SessionEntry {
        sender: "human".into(),
        content: "ordinary turn for other".into(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), gate.wait_started())
        .await
        .unwrap();
    assert_eq!(
        fx.server
            .turns
            .lock()
            .await
            .get(&sid)
            .unwrap()
            .agent_db_id
            .as_deref(),
        Some(other.db_id.to_string().as_str())
    );
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        session(&fx, &sid)
            .await
            .schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!format!("{:?}", fx.model.recorded_calls()[0]).contains(PRIVATE));
    gate.release();
    idle(&fx, &sid).await;
    fx.model.push_text("owner finished");
    fx.server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    idle(&fx, &sid).await;
    assert_eq!(fx.model.recorded_calls().len(), 2);
    assert!(format!("{:?}", fx.model.recorded_calls()[1]).contains(PRIVATE));
    assert_eq!(
        fx.server
            .sessions
            .lock()
            .await
            .get(&sid)
            .unwrap()
            .agent_override
            .as_deref(),
        Some("other")
    );
    let s = session(&fx, &sid).await;
    assert_eq!(
        s.entries()
            .iter()
            .filter(|entry| entry.entry_type == EntryType::Message && entry.sender == "timer")
            .count(),
        1
    );
}

#[tokio::test]
async fn scheduled_common_removal_after_included_does_not_fail_or_restart_human_turn() {
    for mutation in ["remove", "replace"] {
        let fx = Fixture::new(true).await;
        let (reference, sid) = prepared(&fx, false, true).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        fx.server.tools.register(IncludedPause {
            entered: entered.clone(),
            release: release.clone(),
        });
        fx.model.push_text("provisional");
        fx.model
            .push_tool_calls(vec![("pause".into(), "included_pause".into(), "{}".into())]);
        fx.model.push_text("human turn completed normally");
        let first = fx.model.block_next_call();
        let mut s = session(&fx, &sid).await;
        let request = s
            .add_entry(SessionEntry {
                sender: "human".into(),
                content: "original human request".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), first.wait_started())
            .await
            .unwrap();
        let attempt = fx
            .server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .unwrap();
        first.release();
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        let s = session(&fx, &sid).await;
        assert!(matches!(
            s.schedule_delivery_evidence(&reference)
                .await
                .unwrap()
                .unwrap()
                .input
                .state,
            JobInputState::Included { .. }
        ));
        if mutation == "remove" {
            fx.adb.remove_schedule("wake").await.unwrap();
        } else {
            let mut replacement = fx.adb.find_schedule("wake").await.unwrap().unwrap();
            replacement.prompt = "NEW-REVISION-MUST-NOT-REPLACE-INCLUDED-INPUT".into();
            fx.adb.upsert_schedule(replacement).await.unwrap();
        }
        release.notify_one();
        idle(&fx, &sid).await;
        let s = session(&fx, &sid).await;
        assert_eq!(
            fx.model.recorded_calls().len(),
            3,
            "{mutation} after Included is prospective, not cancellation of the human turn"
        );
        assert!(
            fx.model.recorded_calls()[2].messages.iter().any(
                |m| matches!(m, crate::runtime::RuntimeMessage::System(text) if text == PRIVATE)
            )
        );
        assert!(!format!("{:?}", fx.model.recorded_calls()[2]).contains("NEW-REVISION-MUST-NOT"));
        assert_eq!(s.attempts_for_test().await.len(), 1);
        assert_eq!(s.attempts_for_test().await[0].request_id, request);
        assert_eq!(
            s.turn_completion(&attempt).await.unwrap().unwrap().outcome,
            crate::session::outcome::TurnOutcome::Success
        );
    }
}

#[tokio::test]
async fn scheduled_common_panic_releases_capacity_but_never_retries_uncertain_attempt() {
    let fx = Fixture::new(true).await;
    let (reference, sid) = prepared(&fx, true, true).await;
    fx.server.tools.register(PanicTool);
    fx.model
        .push_tool_calls(vec![("panic".into(), "panic_tool".into(), "{}".into())]);
    let id = fx
        .server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    idle(&fx, &sid).await;
    assert_eq!(
        fx.server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
    assert!(!fx.server.live_attempts.lock().await.contains(&id));
    assert!(!fx.server.turns.lock().await.contains_key(&sid));
    let s = session(&fx, &sid).await;
    assert_eq!(
        s.read_turn_attempt(&id).await.unwrap().unwrap().status,
        TurnAttemptStatus::Started
    );
    assert!(s.turn_completion(&id).await.unwrap().is_none());
    assert!(matches!(
        s.schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .unwrap()
            .input
            .state,
        JobInputState::Included { .. }
    ));
    let views = fx.view().await.preview_scheduling().await.unwrap();
    let crate::schedule_state::readers::AccountingView::Current(state) = &views[0].accounting
    else {
        panic!("current accounting")
    };
    assert_eq!((state.successful_fires, state.reservations), (0, 1));
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fx.model.recorded_calls().len(), 1);
}

async fn publish_extra(fx: &Fixture, id: &str, sid: &str, text: &str) -> ScheduleReference {
    let schedule = Schedule::new(
        id,
        Trigger::OneShot {
            fire_at: Utc::now(),
        },
        text,
        ScheduleTarget::Pinned {
            session_db_id: sid.into(),
        },
    );
    fx.adb.upsert_schedule(schedule).await.unwrap();
    let agent = fx.view().await;
    let schedule = agent.find_schedule(id).await.unwrap().unwrap();
    reconcile(&agent, &schedule).await;
    let Trigger::OneShot { fire_at } = schedule.trigger else {
        unreachable!()
    };
    let row = lifecycle::prepare(
        &agent,
        id,
        &schedule.revision_key().unwrap(),
        DueSlot::Calendar { due_at: fire_at },
        Utc::now(),
    )
    .await
    .unwrap();
    session(fx, sid)
        .await
        .prepare_schedule_input(&row.reference)
        .await
        .unwrap();
    reconcile(&agent, &schedule).await;
    lifecycle::handoff(&agent, &row.reference, &session(fx, sid).await, Utc::now())
        .await
        .unwrap();
    row.reference
}

async fn pump_steps(
    pump: &mut super::super::scheduled_delivery::DeliveryPump,
    fx: &Fixture,
    count: usize,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..count {
            pump.step(&fx.server).await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("discovery must not wait for capacity, model or tools");
}

#[tokio::test]
async fn scheduled_revoked_key_and_malformed_home_refuse_despite_roster() {
    for revoke in [false, true] {
        let f = Fixture::new(true).await;
        let (reference, sid) = prepared(&f, false, true).await;
        let agent = f.view().await;
        let definition = agent.find_schedule("wake").await.unwrap().unwrap();
        let entry = f.server.agent_index.find_by_id(&agent.id()).unwrap();
        let s = session(&f, &sid).await;
        f.server
            .timer_target_authority(&agent, &definition, &entry)
            .await
            .unwrap();
        f.server
            .authorized_schedule(&s, &reference, &entry)
            .await
            .unwrap();
        if revoke {
            let txn = s.database().new_transaction().await.unwrap();
            txn.get_settings()
                .unwrap()
                .revoke_auth_key(&entry.pubkey)
                .await
                .unwrap();
            txn.commit().await.unwrap();
        } else {
            crate::session::update_meta_on_db(s.database(), |meta| {
                meta.agents[0].home_pubkey = Some("malformed".into());
            })
            .await
            .unwrap();
        }
        assert_eq!(s.try_read_meta().await.unwrap().agents.len(), 1);
        assert!(
            f.server
                .timer_target_authority(&agent, &definition, &entry)
                .await
                .is_err()
        );
        assert!(
            f.server
                .authorized_schedule(&s, &reference, &entry)
                .await
                .is_err()
        );
        assert!(s.attempts_for_test().await.is_empty());
    }
}

#[tokio::test]
async fn scheduled_superseded_unbound_handoff_does_not_starve_next_delivery() {
    let f = Fixture::new(true).await;
    let (old, sid) = prepared(&f, false, true).await;
    let capacity = f
        .server
        .semaphore
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
        .await
        .unwrap();
    let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
    pump_steps(&mut pump, &f, 12).await;
    let s = session(&f, &sid).await;
    assert!(s.schedule_delivery_evidence(&old).await.unwrap().is_none());
    let mut definition = f.adb.find_schedule("wake").await.unwrap().unwrap();
    definition.enabled = false;
    f.adb.upsert_schedule(definition).await.unwrap();
    let next = publish_extra(&f, "next", &sid, "CURRENT-PRIVATE-INPUT").await;
    drop(capacity);
    f.model.push_text("valid delivery after retired handoff");
    pump_steps(&mut pump, &f, 50).await;
    idle(&f, &sid).await;
    let view = f.view().await;
    let original = lifecycle::retry::delivery(&view, &old.occurrence_id, &old.delivery_id)
        .await
        .unwrap();
    assert!(original.unused());
    assert_eq!(
        view.list_schedule_fires().await.unwrap().len(),
        2,
        "dispatch history retained"
    );
    assert!(s.schedule_delivery_evidence(&old).await.unwrap().is_none());
    assert!(
        s.schedule_delivery_evidence(&next)
            .await
            .unwrap()
            .unwrap()
            .completion
            .is_some()
    );
    assert_eq!(f.model.recorded_calls().len(), 1);
}

#[tokio::test]
async fn scheduled_discovery_recovers_lost_notifications_in_publication_order_without_waiters() {
    let fx = Fixture::new(true).await;
    let (first, sid) = prepared(&fx, false, true).await;
    let second = publish_extra(&fx, "second", &sid, "SECOND-PRIVATE-INPUT").await;
    let entry = fx.server.agent_index.find_by_id(&fx.adb.id()).unwrap();
    let (_, db) = fx
        .registry
        .create_session(Some("another-target"))
        .await
        .unwrap();
    let other_sid = db.root_id().to_string();
    fx.registry
        .attach_agent_to_session(&other_sid, &entry)
        .await
        .unwrap();
    fx.server
        .register_session(
            &db,
            fx.server.default_backend.clone(),
            Some(entry.display_name),
            None,
        )
        .await
        .unwrap();
    let third = publish_extra(&fx, "other", &other_sid, "INDEPENDENT-TARGET-INPUT").await;
    let all = fx
        .server
        .semaphore
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
        .await
        .unwrap();
    let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
    pump_steps(&mut pump, &fx, 80).await;
    assert!(fx.model.recorded_calls().is_empty());
    assert!(
        session(&fx, &sid)
            .await
            .attempts_for_test()
            .await
            .is_empty()
    );
    assert!(
        session(&fx, &other_sid)
            .await
            .attempts_for_test()
            .await
            .is_empty()
    );
    assert!(fx.server.processing.lock().await.is_empty());
    assert_eq!(
        fx.view().await.schedule_occurrences().await.unwrap().len(),
        3
    );
    drop(all);

    // Discard every local cursor/notification while the references remain in
    // native stores. No direct delivery call is made by this scenario.
    let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
    fx.model.push_text("first completion");
    fx.model.push_text("other completion");
    fx.model.push_text("second completion");
    let gate = fx.model.block_next_call();
    pump_steps(&mut pump, &fx, 80).await;
    tokio::time::timeout(Duration::from_secs(5), gate.wait_started())
        .await
        .unwrap();
    idle(&fx, &other_sid).await;
    assert!(
        session(&fx, &other_sid)
            .await
            .schedule_delivery_evidence(&third)
            .await
            .unwrap()
            .unwrap()
            .completion
            .is_some(),
        "a blocked first target cannot starve an independent target"
    );
    assert!(
        session(&fx, &sid)
            .await
            .schedule_delivery_evidence(&second)
            .await
            .unwrap()
            .is_none(),
        "later publication cannot leapfrog a first input awaiting response"
    );
    gate.release();
    idle(&fx, &sid).await;
    pump_steps(&mut pump, &fx, 80).await;
    idle(&fx, &sid).await;
    let s = session(&fx, &sid).await;
    for reference in [&first, &second] {
        assert!(matches!(
            s.schedule_delivery_evidence(reference)
                .await
                .unwrap()
                .unwrap()
                .input
                .state,
            JobInputState::Included { .. }
        ));
    }
    assert_eq!(s.attempts_for_test().await.len(), 2);
    assert_eq!(fx.model.recorded_calls().len(), 3);
    let texts: Vec<_> = fx
        .model
        .recorded_calls()
        .iter()
        .flat_map(|call| call.messages.clone())
        .filter_map(|message| match message {
            crate::runtime::RuntimeMessage::System(text)
                if text == PRIVATE || text == "SECOND-PRIVATE-INPUT" =>
            {
                Some(text)
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec![PRIVATE, "SECOND-PRIVATE-INPUT"]);
    let mut restarted = super::super::scheduled_delivery::DeliveryPump::default();
    pump_steps(&mut restarted, &fx, 80).await;
    assert_eq!(
        fx.model.recorded_calls().len(),
        3,
        "a restarted discovery scan is not a retry"
    );
    assert_eq!(
        fx.server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
}

#[tokio::test]
async fn scheduled_discovery_holds_stale_target_without_initializing_or_steering_another_agent() {
    let fx = Fixture::new(true).await;
    // A new coordinator/list operation cannot provision an absent ledger.
    let before = fx.adb.database().snapshot().await.unwrap();
    let mut pump = super::super::scheduled_delivery::DeliveryPump::default();
    pump_steps(&mut pump, &fx, 20).await;
    assert_eq!(before, fx.adb.database().snapshot().await.unwrap());
    assert!(fx.adb.scheduling_reference().await.unwrap().is_none());
    let (reference, sid) = prepared(&fx, false, true).await;
    let mut schedule = fx.adb.find_schedule("wake").await.unwrap().unwrap();
    schedule.prompt = "not the published immutable revision".into();
    fx.adb.upsert_schedule(schedule).await.unwrap();
    pump_steps(&mut pump, &fx, 80).await;
    assert!(fx.model.recorded_calls().is_empty());
    assert!(
        session(&fx, &sid)
            .await
            .schedule_delivery_evidence(&reference)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        session(&fx, &sid)
            .await
            .attempts_for_test()
            .await
            .is_empty()
    );
    assert_eq!(
        fx.server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
}

#[tokio::test]
async fn scheduled_discovery_processing_loop_finds_a_handoff_without_a_wake_command() {
    let fx = Fixture::new(true).await;
    let (reference, sid) = prepared(&fx, false, true).await;
    fx.server.mark_startup_pending();
    fx.model.push_text("periodic discovery completed");
    // Install the ordinary clock but do not register/fire the definition. This
    // isolates handoff discovery from both timer admission and Session writes.
    let engine = crate::routine::RoutineEngine::new(fx.registry.chaz_peer().clone(), None)
        .await
        .unwrap();
    fx.server.set_routine_engine(engine);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        fx.model.recorded_calls().is_empty(),
        "startup gate still applies"
    );
    fx.server.mark_startup_ready();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let s = session(&fx, &sid).await;
            if s.schedule_delivery_evidence(&reference)
                .await
                .unwrap()
                .is_some_and(|evidence| evidence.completion.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the actual processing loop must discover a committed handoff without notification");
    idle(&fx, &sid).await;
    assert_eq!(fx.model.recorded_calls().len(), 1);
    assert_eq!(session(&fx, &sid).await.attempts_for_test().await.len(), 1);
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_failed_recording_stops_before_effect_or_next_model_without_replay() {
    for allowed_writes in [0, 1] {
        let fx = Fixture::new(true).await;
        let (reference, sid) = prepared(&fx, false, true).await;
        let effects = Arc::new(AtomicUsize::new(0));
        fx.server.tools.register(Effect(effects.clone()));
        fx.model
            .push_tool_calls([("effect-call".into(), "counted_effect".into(), "{}".into())]);
        fx.model.push_text("must not reach a second model");
        fx.server.fail_transcript_after(allowed_writes);
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .unwrap();
        idle(&fx, &sid).await;
        assert_eq!(effects.load(Ordering::SeqCst), allowed_writes);
        assert_eq!(fx.model.recorded_calls().len(), 1);
        let s = session(&fx, &sid).await;
        let attempt = s.attempts_for_test().await.pop().unwrap();
        assert_eq!(attempt.status, TurnAttemptStatus::Started);
        assert!(
            s.turn_completion(&attempt.attempt_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !s.entries()
                .iter()
                .any(|entry| matches!(entry.entry_type, EntryType::Message | EntryType::Error))
        );
        assert!(
            fx.server
                .retry_interrupted_turn(&sid, &attempt.request_id)
                .await
                .is_err()
        );
        assert!(
            fx.server
                .deliver_schedule_input(&sid, &reference)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(fx.model.recorded_calls().len(), 1);
        assert_eq!(
            fx.view().await.list_schedule_fires().await.unwrap().len(),
            1
        );
        assert_eq!(
            fx.server.semaphore.available_permits(),
            MAX_CONCURRENT_LLM_CALLS
        );
        fx.server.shutdown().await;
    }
}

#[tokio::test]
async fn scheduled_stop_targets_the_linked_attempt_and_releases_activity() {
    let fx = Fixture::new(true).await;
    let (reference, sid) = prepared(&fx, false, true).await;
    fx.model.push_text("must be cancelled");
    let gate = fx.model.block_next_call();
    let attempt_id = fx
        .server
        .deliver_schedule_input(&sid, &reference)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gate.wait_started())
        .await
        .unwrap();
    let mut s = session(&fx, &sid).await;
    let attempt = s.attempts_for_test().await.pop().unwrap();
    assert_eq!(attempt.attempt_id, attempt_id);
    s.add_entry(SessionEntry {
        sender: "human".into(),
        content: serde_json::to_string(&crate::session::CancelRequest {
            request_id: attempt.request_id.clone(),
            attempt_id: attempt_id.clone(),
        })
        .unwrap(),
        timestamp: Utc::now(),
        entry_type: EntryType::Cancel,
        metadata: None,
        routing: None,
    })
    .await
    .unwrap();
    fx.server.process_session(&sid).await.unwrap();
    idle(&fx, &sid).await;
    assert_eq!(
        s.attempts_for_test().await[0].status,
        TurnAttemptStatus::Completed
    );
    let s = session(&fx, &sid).await;
    assert!(
        s.entries()
            .iter()
            .any(|entry| entry.content == TURN_STOPPED_MESSAGE)
    );
    assert_eq!(
        s.turn_completion(&attempt_id)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        crate::session::outcome::TurnOutcome::Cancelled
    );
    assert!(
        fx.server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .is_none()
    );
    assert!(fx.server.live_attempts.lock().await.is_empty());
    assert!(fx.server.turns.lock().await.is_empty());
    assert_eq!(
        fx.server.semaphore.available_permits(),
        MAX_CONCURRENT_LLM_CALLS
    );
    fx.server.shutdown().await;
}

#[tokio::test]
async fn scheduled_telemetry_is_live_and_retained_after_no_reply_failure_and_reopen() {
    use crate::runtime::{LLMResponse, ResponseMetadata, TokenUsage};
    for fail in [false, true] {
        let fx = Fixture::new(true).await;
        let (reference, sid) = prepared(&fx, false, true).await;
        fx.server.tools.register(crate::tools::Calculate);
        crate::session::bind_transport(&fx.db, "matrix", "@host:example", "!room:example")
            .await
            .unwrap();
        let metadata = || {
            Some(ResponseMetadata {
                model: "wire".into(),
                usage: TokenUsage {
                    prompt_tokens: 100,
                    completion_tokens: 10,
                    cost_usd: Some(0.01),
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        fx.model.push_response(Ok(LLMResponse::ToolCalls {
            content: None,
            tool_calls: vec![runtime::ToolCallRequest {
                id: "one".into(),
                name: "calculate".into(),
                arguments: "{\"expression\":\"1+1\"}".into(),
            }],
            provider_extra: Default::default(),
            metadata: metadata(),
        }));
        if fail {
            fx.model.push_err(crate::error::LlmError::Configuration {
                message: "later failure".into(),
            });
        } else {
            fx.model.push_response(Ok(LLMResponse::ToolCalls {
                content: None,
                tool_calls: vec![runtime::ToolCallRequest {
                    id: "quiet".into(),
                    name: "no_reply".into(),
                    arguments: "{}".into(),
                }],
                provider_extra: Default::default(),
                metadata: metadata(),
            }));
        }
        let first = fx.model.block_next_call();
        let second = fx.model.block_next_call();
        let attempt = fx
            .server
            .deliver_schedule_input(&sid, &reference)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), first.wait_started())
            .await
            .unwrap();
        first.release();
        tokio::time::timeout(Duration::from_secs(5), second.wait_started())
            .await
            .unwrap();
        let snapshot = fx.db.snapshot().await.unwrap();
        let entries = Session::entries_at_snapshot(&fx.db, &snapshot)
            .await
            .unwrap();
        let live = crate::session::usage::telemetry_at_snapshot(&fx.db, &snapshot, &entries)
            .await
            .unwrap();
        assert_eq!(live.totals.prompt_tokens, 100);
        assert!(live.totals.estimated.prompt_tokens > 0);
        assert!(
            !entries
                .iter()
                .any(|entry| entry.entry_type == EntryType::Message)
        );
        second.release();
        idle(&fx, &sid).await;
        let (conv, reopened) = fx.registry.open_session(&sid).await.unwrap();
        let s = Session::new(conv, reopened).await;
        let snapshot = s.database().snapshot().await.unwrap();
        let entries = Session::entries_at_snapshot(s.database(), &snapshot)
            .await
            .unwrap();
        let telemetry =
            crate::session::usage::telemetry_at_snapshot(s.database(), &snapshot, &entries)
                .await
                .unwrap();
        assert_eq!(telemetry.totals.prompt_tokens, if fail { 100 } else { 200 });
        assert_eq!(telemetry.totals.calls, 2);
        assert_eq!(
            s.turn_completion(&attempt).await.unwrap().unwrap().outcome,
            if fail {
                crate::session::outcome::TurnOutcome::Failure
            } else {
                crate::session::outcome::TurnOutcome::Success
            }
        );
        assert!(
            !entries
                .iter()
                .any(|entry| entry.entry_type == EntryType::Message
                    || entry.content.contains(PRIVATE))
        );
        assert!(!format!("{:?}", s.turn_transcript(&attempt).await.unwrap()).contains(PRIVATE));
        assert_eq!(
            fx.view().await.list_schedule_fires().await.unwrap().len(),
            1
        );
        fx.server.shutdown().await;
    }
}
