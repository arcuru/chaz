//! Loss-tolerant Session-to-ledger projection on the existing processing loop.
//! A scan discovers evidence, not execution authority. Work is bounded per
//! tick; removed definitions stay eligible for history settlement, not retry.
use super::*;
use crate::schedule_state::lifecycle::{self, Occurrence, OccurrenceScan, Preparation};
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct ProjectionPump {
    agents: Vec<crate::agent_db::AgentDb>,
    next: usize,
    scan: Option<OccurrenceScan>,
    page: VecDeque<Occurrence>,
}

impl ProjectionPump {
    pub(super) async fn step(&mut self, server: &Server) {
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
                        debug!(agent_db_id = %entry.db_id, %error, "Schedule projection discovery held")
                    }
                }
            }
            // Keep old cache owners alive until their successors are opened.
            self.agents = agents;
            self.next = 0;
        }
        let Some(owner) = self.agents.get(self.next).map(|agent| agent.id()) else {
            return;
        };
        let result: anyhow::Result<bool> = async {
            let entry = server
                .agent_index
                .find_by_id(&owner)
                .ok_or_else(|| anyhow::anyhow!("projection owner no longer hosted"))?;
            // Keys may have advanced since discovery; never use a stale signer
            // or initialize a replacement ledger to make a projection succeed.
            let agent = server.registry.open_scheduling_writer(&entry).await?;
            self.agents[self.next] = agent.clone();
            if let Some(row) = self.page.front() {
                if !lifecycle::reconcile_reference(&agent, &row.reference, 32)
                    .await?
                    .complete
                {
                    return Ok(false);
                }
                if let Preparation::HandedOff { input, .. } = &row.preparation {
                    let (conv, db) = server.registry.open_session(&input.session_db_id).await?;
                    let session = Session::new(conv, db).await;
                    if let Some(evidence) =
                        lifecycle::settle(&agent, &row.reference, &session).await?
                        && evidence.completion.is_none()
                    {
                        if matches!(
                            evidence.input.state,
                            crate::session::steering::JobInputState::Included { .. }
                                | crate::session::steering::JobInputState::Dispatching { .. }
                        ) && session
                            .latest_job_wait_checkpoint(&evidence.input.attempt_id)
                            .await?
                            .is_some_and(|checkpoint| !checkpoint.observation_finished)
                        {
                            // A restart may find a checkpoint while its audit
                            // is incomplete. The hint grants no readmission.
                            let _ = server
                                .notify_tx
                                .try_send(ProcessingCommand::Wake(input.session_db_id.clone()));
                        } else if evidence.input.executor.is_some() {
                            server
                                .recover_scheduled_input(&agent, &entry, row, &evidence)
                                .await?;
                        }
                    }
                }
                self.page.pop_front();
                return Ok(false);
            }
            if self.scan.is_none() {
                self.scan = OccurrenceScan::open(&agent).await?;
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
            Ok(true) => {
                self.next += 1;
                self.scan = None;
            }
            Err(error) => {
                debug!(%error, "Schedule projection held; reservations retained");
                // A bad row cannot starve every other occurrence/Agent. It is
                // revisited next scan; no missing read is a success or refund.
                if self.page.pop_front().is_none() {
                    self.next += 1;
                    self.scan = None;
                }
            }
        }
    }
}

impl Server {
    async fn recover_scheduled_input(
        &self,
        agent: &crate::agent_db::AgentDb,
        entry: &crate::hosted_index::DbEntry,
        row: &Occurrence,
        evidence: &crate::session::scheduled::ScheduleDeliveryEvidence,
    ) -> anyhow::Result<()> {
        let executor = self.schedule_executor(entry);
        let sid = &evidence.input.anchor.session_db_id;
        // This runs on the ordinary processing loop, serialized with new
        // admissions and exact wait readmission. Existing runners may still
        // hold processing or a live suspended attempt, which we never retry.
        if !evidence.input.executor.as_ref().is_some_and(|prior| {
            prior.home_key == executor.home_key && prior.incarnation != executor.incarnation
        }) || self.processing.lock().await.contains(sid)
            || self
                .live_attempts
                .lock()
                .await
                .contains(&evidence.input.attempt_id)
            || self.shutting_down.load(Ordering::Acquire)
        {
            return Ok(());
        }
        let schedule = agent
            .resolve_schedule_reference(&row.reference, Utc::now())
            .await?;
        self.timer_target_authority(agent, &schedule, entry).await?;
        let (conv, db, key) = self.registry.open_session_with_signer(sid).await?;
        let mut session = Session::try_new(conv, db).await?.with_signing_key(key);
        self.schedule_target_owner(&session, &row.reference, entry)
            .await?;
        if let Some(reference) =
            lifecycle::retry::recover(agent, row, evidence, &mut session, &executor).await?
        {
            info!(
                session_db_id = %sid, occurrence_id = %reference.occurrence_id,
                delivery_id = %reference.delivery_id, prior_attempt_id = %evidence.input.attempt_id,
                "Allocated opted-in schedule recovery; prior effects may repeat"
            );
        }
        Ok(())
    }

    /// Required point projection at the model response boundary. It does not
    /// scan/reconcile on admission; an invalidated certificate stops progression
    /// and the ordinary bounded pump later repairs the projection from evidence.
    pub(super) async fn project_scheduled_attempt(
        &self,
        session: &Session,
        attempt: &str,
    ) -> anyhow::Result<()> {
        #[cfg(test)]
        if session.turn_completion(attempt).await?.is_some() {
            self.check_schedule_write("completed-projection")?;
        }
        for input in session.scheduled_inputs_for_attempt(attempt).await? {
            let reference = &input.anchor.reference;
            let owner = eidetica::entry::ID::parse(&reference.owner_agent_db_id)?;
            let entry = self
                .agent_index
                .find_by_id(&owner)
                .ok_or_else(|| anyhow::anyhow!("schedule projection owner not hosted"))?;
            let agent = self.registry.open_scheduling_writer(&entry).await?;
            lifecycle::settle(&agent, reference, session).await?;
        }
        Ok(())
    }
}
