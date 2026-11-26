//! Targeted admission of an already committed schedule handoff. Capacity is
//! tried, never awaited: a deferred reference stays durable, without a waiter
//! per occurrence, Started record, generic Ack replay or fabricated job parent.
use super::*;
use crate::agent_db::{ScheduleReference, ScheduleTarget};
use futures::FutureExt;

impl Server {
    /// Deliver once into the exact active Agent, or start its targeted idle
    /// invocation through the ordinary runner. `None` means deferred or already
    /// observed, not success/fulfillment. The coordinator must rediscover queued
    /// handoffs; a missing receipt for a Started request is held, not retried.
    pub async fn deliver_schedule_input(
        self: &Arc<Self>,
        session_db_id: &str,
        reference: &ScheduleReference,
    ) -> anyhow::Result<Option<String>> {
        anyhow::ensure!(
            self.executor_authorized,
            "client cannot deliver scheduled input"
        );
        if !self.is_startup_ready() || self.shutting_down.load(Ordering::Acquire) {
            return Ok(None);
        }
        let owner = eidetica::entry::ID::parse(&reference.owner_agent_db_id)?;
        let executor = self
            .agent_index
            .find_by_id(&owner)
            .ok_or_else(|| anyhow::anyhow!("schedule owner not hosted"))?;
        let (conv, db, key) = self
            .registry
            .open_session_with_signer(session_db_id)
            .await?;
        let session = Session::new(conv, db.clone()).await.with_signing_key(key);
        let schedule = self
            .authorized_schedule(&session, reference, &executor)
            .await?;
        if session
            .schedule_delivery_evidence(reference)
            .await?
            .is_some()
        {
            return Ok(None); // never rebind an uncertain, closed or completed input
        }
        let fresh = matches!(schedule.target, ScheduleTarget::Fresh);
        if !fresh {
            let turns = self.turns.lock().await;
            if let Some(turn) = turns.get(session_db_id) {
                if turn.agent_db_id.as_deref() != Some(&reference.owner_agent_db_id)
                    || turn.cancel.is_cancelled()
                {
                    return Ok(None);
                }
                drop(turns);
                // Return the identity bound under the final turn lock, not
                // this earlier discovery snapshot of a possibly ending turn.
                return self
                    .bind_active_schedule_input_to(session_db_id, reference)
                    .await
                    .map(Some);
            }
        }
        // Idle schedule delivery cannot bypass an accepted Agent job's parent,
        // participation, result or execution-branch protocol.
        anyhow::ensure!(
            !crate::session::jobs::is_job_session(&db).await,
            "idle scheduled input cannot create a root turn in an Agent job"
        );
        if !fresh {
            let live_attempts = self.live_attempts.lock().await.clone();
            if Box::pin(self.find_resumable_wait(&session, &live_attempts))
                .await?
                .is_some()
            {
                let _ = self
                    .notify_tx
                    .try_send(ProcessingCommand::Wake(session_db_id.into()));
                return Ok(None);
            }
        }
        if fresh && !self.is_session_open(session_db_id).await {
            self.register_session(
                &db,
                self.default_backend.clone(),
                Some(executor.display_name.clone()),
                None,
            )
            .await?;
        }
        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits | TryAcquireError::Closed) => return Ok(None),
        };
        if !self
            .processing
            .lock()
            .await
            .insert(session_db_id.to_string())
        {
            return Ok(None);
        }
        let mut local_attempt = None;
        let result = std::panic::AssertUnwindSafe(async {
            // Reopen after registration/identity delegation. No interactive
            // routing override, watcher, parent scope or completion signal moves.
            let (conv, db, key) = self
                .registry
                .open_session_with_signer(session_db_id)
                .await?;
            let session = Session::new(conv, db).await.with_signing_key(key);
            anyhow::ensure!(
                self.agent_index
                    .find_by_name(&executor.display_name)
                    .is_some_and(|current| current.db_id == executor.db_id
                        && current.pubkey == executor.pubkey),
                "schedule owner name is ambiguous or changed"
            );
            let agent_db = self
                .registry
                .open_agent_db(&owner, Some(&executor.pubkey))
                .await?
                .ok_or_else(|| anyhow::anyhow!("scheduled Agent config unavailable"))?;
            let config = agent_db.read_config().await?;
            let agent = self
                .agents
                .build_from_db_config(&executor.display_name, &config);
            let agent = self.agent_for_execution(agent).await?;
            // Hydration can await I/O; the durable authority guard is still
            // required immediately before the atomic start/input transaction.
            let schedule = self
                .authorized_schedule(&session, reference, &executor)
                .await?;
            let backend = self
                .sessions
                .lock()
                .await
                .get(session_db_id)
                .map(|runtime| runtime.backend.clone())
                .ok_or_else(|| anyhow::anyhow!("scheduled runtime is no longer registered"))?;
            let id = uuid::Uuid::new_v4().to_string();
            local_attempt = Some(id.clone());
            self.live_attempts.lock().await.insert(id.clone());
            #[cfg(test)]
            self.check_schedule_write("common-start")?;
            let Some(attempt) = session
                .start_schedule_input_as(
                    reference,
                    id.clone(),
                    schedule
                        .interruption_retries
                        .map(|_| self.schedule_executor(&executor)),
                )
                .await?
            else {
                return Ok(None);
            };
            let spawn = SpawnContext {
                worker_prompt: None,
                worker_role_suffix: None,
                worker_model: None,
                job_projection: None,
                job_incarnation: None,
                job_retry: None,
                job_authority: None,
                call_depth: 0,
                max_call_depth: 0,
                parent_tools: None,
                completion_tx: None,
                processing: self.processing.clone(),
                turns: self.turns.clone(),
            };
            self.spawn_agent_task(
                session_db_id.into(),
                session,
                agent,
                None,
                backend,
                attempt,
                spawn,
                Some(permit),
                None,
                true,
            )
            .await;
            local_attempt = None; // cleanup belongs to the common runner
            Ok::<_, anyhow::Error>(Some(id))
        })
        .catch_unwind()
        .await;
        match result {
            Ok(Ok(Some(id))) => Ok(Some(id)), // common runner owns cleanup
            result => {
                if let Some(id) = local_attempt {
                    self.live_attempts.lock().await.remove(&id);
                    let mut turns = self.turns.lock().await;
                    if turns
                        .get(session_db_id)
                        .is_some_and(|turn| turn.attempt.attempt_id == id)
                    {
                        turns.remove(session_db_id);
                    }
                }
                self.processing.lock().await.remove(session_db_id);
                match result {
                    Ok(result) => result,
                    Err(_) => {
                        anyhow::bail!("scheduled admission panicked; outcome held, no replay")
                    }
                }
            }
        }
    }
}

/// Loss recovery uses the existing processing loop, not one resident future per
/// occurrence. A pass observes one immutable publication snapshot. Deferred or
/// uncertain input blocks later input to that target in this pass, not other
/// targets; the next pass retries discovery, never an already bound delivery.
#[derive(Default)]
pub(super) struct DeliveryPump {
    agents: Vec<crate::agent_db::AgentDb>,
    next: usize,
    scan: Option<crate::schedule_state::lifecycle::HandoffScan>,
    page: std::collections::VecDeque<crate::schedule_state::lifecycle::Occurrence>,
    blocked_targets: std::collections::HashSet<String>,
}

impl DeliveryPump {
    pub(super) async fn step(&mut self, server: &Arc<Server>) {
        use crate::schedule_state::lifecycle::{self, HandoffScan, Preparation};
        use crate::session::steering::JobInputState;
        if !server.executor_authorized
            || !server.is_startup_ready()
            || server.shutting_down.load(Ordering::Acquire)
        {
            return;
        }
        if self.next == self.agents.len() {
            let mut agents = Vec::new();
            for entry in server.agent_index.list() {
                match server
                    .registry
                    .open_scheduling_agent_db(&entry.db_id, Some(&entry.pubkey))
                    .await
                {
                    Ok(Some(agent)) => agents.push(agent),
                    Ok(None) => {}
                    Err(error) => {
                        debug!(agent_db_id = %entry.db_id, %error, "Schedule delivery discovery held")
                    }
                }
            }
            self.agents = agents;
            self.next = 0;
        }
        let Some(owner) = self.agents.get(self.next).map(|agent| agent.id()) else {
            return;
        };
        let mut target = None;
        let result: anyhow::Result<bool> = async {
            let entry = server
                .agent_index
                .find_by_id(&owner)
                .ok_or_else(|| anyhow::anyhow!("delivery owner no longer hosted"))?;
            let agent = server
                .registry
                .open_scheduling_agent_db(&owner, Some(&entry.pubkey))
                .await?
                .ok_or_else(|| anyhow::anyhow!("delivery owner unavailable"))?;
            self.agents[self.next] = agent.clone();
            if let Some(row) = self.page.front() {
                let Preparation::HandedOff { input, .. } = &row.preparation else {
                    anyhow::bail!("ordered occurrence is not handed off")
                };
                let sid = &input.session_db_id;
                target = Some(sid.clone());
                if !self.blocked_targets.contains(sid) {
                    if !lifecycle::reconcile_reference(&agent, &row.reference, 32)
                        .await?
                        .complete
                    {
                        // Retain this row until its bounded audit completes;
                        // skipping it forever would starve a large history.
                        return Ok(false);
                    }
                    if !lifecycle::retry::is_current(&agent, row).await? {
                        self.page.pop_front();
                        return Ok(false);
                    }
                    let (conv, db) = server.registry.open_session(sid).await?;
                    let session = Session::new(conv, db).await;
                    match session.schedule_delivery_evidence(&row.reference).await? {
                        Some(evidence)
                            if evidence.completion.is_some()
                                || matches!(
                                    evidence.input.state,
                                    JobInputState::Included { .. }
                                ) => {}
                        Some(_) => {
                            self.blocked_targets.insert(sid.clone());
                        }
                        None => {
                            let mut closed = false;
                            if row.unused()
                                || lifecycle::superseded(&agent, &row.reference, Utc::now()).await?
                            {
                                let writer = server.registry.open_scheduling_writer(&entry).await?;
                                closed = lifecycle::retire_unused(
                                    &writer,
                                    &row.reference,
                                    &session,
                                    Utc::now(),
                                )
                                .await?;
                            }
                            if !closed {
                                server.deliver_schedule_input(sid, &row.reference).await?;
                                self.blocked_targets.insert(sid.clone());
                            }
                        }
                    }
                }
                self.page.pop_front();
                return Ok(false);
            }
            if self.scan.is_none() {
                self.scan = HandoffScan::open(&agent).await?;
            }
            let Some(scan) = &mut self.scan else {
                return Ok(true);
            };
            match scan.next_page(&agent, 16).await? {
                Some(page) => {
                    self.page = page.into();
                    Ok(false)
                }
                None => Ok(true),
            }
        }
        .await;
        match result {
            Ok(false) => {}
            Ok(true) => self.finish_agent(),
            Err(error) => {
                debug!(%error, "Schedule delivery held; no retry authority inferred");
                if let Some(target) = target {
                    self.blocked_targets.insert(target);
                    self.page.pop_front();
                } else {
                    self.finish_agent();
                }
            }
        }
    }

    fn finish_agent(&mut self) {
        self.next += 1;
        self.scan = None;
        self.page.clear();
        self.blocked_targets.clear();
    }
}
