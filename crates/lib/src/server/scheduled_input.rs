//! Private references enter the ordinary turn at its existing awaited boundary.
//! This adapter neither creates attempts nor makes inert Acks runnable. The
//! clock/coordinator must commit handoff first; a live exact-Agent turn is the
//! only delivery target currently exposed here.
use super::*;
use crate::agent_db::ScheduleReference;
use crate::schedule_state::lifecycle::ReconciliationRequired;
use crate::session::TurnTranscriptMessage;
use crate::session::scheduled::ScheduleExecutor;
use crate::session::steering::JobInputState;
use futures::future::BoxFuture;

pub(super) struct ScheduledInputs {
    pub server: std::sync::Weak<Server>,
    pub agent: crate::hosted_index::DbEntry,
}

impl ScheduledInputs {
    pub(super) fn new(
        server: std::sync::Weak<Server>,
        agent: crate::hosted_index::DbEntry,
    ) -> Self {
        Self { server, agent }
    }
}

impl Server {
    pub(super) fn schedule_executor(
        &self,
        entry: &crate::hosted_index::DbEntry,
    ) -> ScheduleExecutor {
        ScheduleExecutor {
            home_key: entry.pubkey.to_string(),
            incarnation: self.job_incarnation.clone(),
        }
    }

    /// The durable successor of admission is necessary but never sufficient:
    /// every request also reopens current keys and checks exact membership/home.
    pub(super) async fn authorized_schedule(
        &self,
        session: &Session,
        reference: &ScheduleReference,
        executor: &crate::hosted_index::DbEntry,
    ) -> anyhow::Result<crate::agent_db::Schedule> {
        self.schedule_definition(session, reference, executor, None)
            .await
    }

    async fn schedule_definition(
        &self,
        session: &Session,
        reference: &ScheduleReference,
        executor: &crate::hosted_index::DbEntry,
        included_attempt: Option<&str>,
    ) -> anyhow::Result<crate::agent_db::Schedule> {
        let agent = self
            .schedule_target_owner(session, reference, executor)
            .await?;
        let schedule = match included_attempt {
            Some(attempt) => {
                crate::schedule_state::lifecycle::retained_input(
                    &agent, reference, session, attempt,
                )
                .await?
            }
            None => {
                crate::schedule_state::lifecycle::authorized_input(
                    &agent,
                    reference,
                    session,
                    Utc::now(),
                )
                .await?
            }
        };
        // A bound Fresh root may need registration after restart. An ordinary
        // Pinned target never acquires that privilege from an inert reference.
        anyhow::ensure!(
            matches!(schedule.target, crate::agent_db::ScheduleTarget::Fresh)
                || self
                    .is_session_open(&session.database().root_id().to_string())
                    .await,
            "scheduled target is not open"
        );
        if included_attempt.is_none()
            && schedule.interruption_retries.is_some()
            && let Some(prior) =
                crate::schedule_state::lifecycle::retry::verify_interrupted_predecessor(
                    &agent,
                    reference,
                    session,
                    &self.schedule_executor(executor),
                )
                .await?
        {
            anyhow::ensure!(
                !self.live_attempts.lock().await.contains(&prior),
                "prior schedule attempt resumed"
            );
        }
        Ok(schedule)
    }

    pub(super) async fn schedule_target_owner(
        &self,
        session: &Session,
        reference: &ScheduleReference,
        executor: &crate::hosted_index::DbEntry,
    ) -> anyhow::Result<crate::agent_db::AgentDb> {
        anyhow::ensure!(
            self.executor_authorized,
            "client cannot execute scheduled input"
        );
        anyhow::ensure!(
            reference.owner_agent_db_id == executor.db_id.to_string(),
            "scheduled input targets another Agent"
        );
        let current = self
            .agent_index
            .find_by_id(&executor.db_id)
            .ok_or_else(|| anyhow::anyhow!("schedule owner no longer hosted"))?;
        anyhow::ensure!(
            current.pubkey == executor.pubkey,
            "schedule executor identity changed"
        );
        let agent = self
            .registry
            .open_scheduling_agent_db(&executor.db_id, Some(&executor.pubkey))
            .await?
            .ok_or_else(|| anyhow::anyhow!("schedule owner unavailable"))?;
        anyhow::ensure!(
            crate::db_kind::try_read_agent_home_pubkey(agent.database())
                .await?
                .is_none_or(|home| home == executor.pubkey),
            "not home peer for schedule owner"
        );
        crate::session::require_schedule_member(session.database(), agent.database(), executor)
            .await?;
        Session::require_legacy_history(session.database()).await?;
        Ok(agent)
    }

    async fn private_schedule_text(
        &self,
        session: &Session,
        reference: &ScheduleReference,
        executor: &crate::hosted_index::DbEntry,
        included_attempt: Option<&str>,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.is_session_open(&session.database().root_id().to_string())
                .await,
            "scheduled runtime is no longer registered"
        );
        Ok(self
            .schedule_definition(session, reference, executor, included_attempt)
            .await?
            .prompt)
    }

    pub(super) async fn resumable_scheduled_wait(
        &self,
        session: &Session,
        request: &TurnRequestId,
        checkpoint: crate::session::wait_resume::JobWaitCheckpoint,
    ) -> anyhow::Result<Result<crate::session::wait_resume::ResumableWait, &'static str>> {
        let records = session.turn_transcript(&checkpoint.attempt_id).await?;
        if !records
            .iter()
            .any(|record| matches!(record.message, TurnTranscriptMessage::ScheduledInput { .. }))
        {
            return session.resumable_job_wait(request, checkpoint).await;
        }
        if &checkpoint.request_id != request {
            return Ok(Err("checkpoint belongs to another request"));
        }
        let Some(executor) = self.agent_index.find_by_name(&checkpoint.agent_name) else {
            return Ok(Err("scheduled continuation executor unavailable"));
        };
        let mut private = Vec::new();
        let mut observed = Vec::new();
        for record in &records {
            if let TurnTranscriptMessage::ScheduledInput {
                model_sequence,
                references,
            } = &record.message
            {
                for reference in references {
                    let Some(evidence) = session.schedule_delivery_evidence(reference).await?
                    else {
                        return Ok(Err("scheduled continuation receipt unavailable"));
                    };
                    if evidence.input.attempt_id != checkpoint.attempt_id
                        || evidence.input.request_id != *request
                        || !matches!(evidence.input.state, JobInputState::Included { model_sequence: seq } | JobInputState::Dispatching { model_sequence: seq } if seq == *model_sequence)
                    {
                        return Ok(Err("scheduled continuation receipt differs"));
                    }
                    let text = match self
                        .private_schedule_text(
                            session,
                            reference,
                            &executor,
                            matches!(evidence.input.state, JobInputState::Included { .. })
                                .then_some(checkpoint.attempt_id.as_str()),
                        )
                        .await
                    {
                        Ok(text) => text,
                        Err(error) if error.is::<ReconciliationRequired>() => return Err(error),
                        Err(_) => return Ok(Err("scheduled continuation authority unavailable")),
                    };
                    private.push((reference.clone(), text));
                    if matches!(evidence.input.state, JobInputState::Dispatching { .. }) {
                        observed.push((reference.clone(), *model_sequence));
                    }
                }
            }
        }
        let (runtime, next_sequence) =
            match crate::session::wait_resume::plan_job_wait_resume_with_schedules(
                &checkpoint,
                &records,
                &private,
            ) {
                Ok(plan) => plan,
                Err(reason) => return Ok(Err(reason)),
            };
        // Only a fully paired same-attempt response can repair a lost receipt.
        // This never dispatches the model again or creates another delivery.
        for (reference, model_sequence) in observed {
            session
                .record_schedule_input(
                    &reference,
                    &checkpoint.attempt_id,
                    JobInputState::Included { model_sequence },
                )
                .await?;
        }
        Ok(Ok(crate::session::wait_resume::ResumableWait {
            checkpoint,
            runtime,
            next_sequence,
        }))
    }

    /// Bind to the actual containing turn, never restart it or invent a parent.
    /// Idle/Fresh admission will use the same recorder after acquiring capacity.
    pub async fn bind_active_schedule_input(
        &self,
        session_db_id: &str,
        reference: &ScheduleReference,
    ) -> anyhow::Result<()> {
        self.bind_active_schedule_input_to(session_db_id, reference)
            .await
            .map(|_| ())
    }

    pub(super) async fn bind_active_schedule_input_to(
        &self,
        session_db_id: &str,
        reference: &ScheduleReference,
    ) -> anyhow::Result<String> {
        let turns = self.turns.lock().await;
        let turn = turns
            .get(session_db_id)
            .ok_or_else(|| anyhow::anyhow!("no active containing turn"))?;
        anyhow::ensure!(!turn.cancel.is_cancelled(), "containing turn was stopped");
        let owner = eidetica::entry::ID::parse(&reference.owner_agent_db_id)?;
        let executor = self
            .agent_index
            .find_by_id(&owner)
            .ok_or_else(|| anyhow::anyhow!("schedule owner not hosted"))?;
        let (_, db) = self.registry.open_session(session_db_id).await?;
        let session = Session::new(ConversationId(session_db_id.into()), db).await;
        // Exact identity captured when the common runner took this turn, not
        // a name, the current routing override or a fabricated job parent.
        anyhow::ensure!(
            turn.agent_db_id.as_deref() == Some(reference.owner_agent_db_id.as_str()),
            "containing turn has no matching executor identity"
        );
        let schedule = self
            .authorized_schedule(&session, reference, &executor)
            .await?;
        let provenance = schedule
            .interruption_retries
            .map(|_| self.schedule_executor(&executor));
        session
            .bind_schedule_input_owned(reference, &turn.attempt.attempt_id, provenance)
            .await?;
        Ok(turn.attempt.attempt_id.clone())
    }
}

impl crate::session::turn_recorder::ScheduleInputAuthority for ScheduledInputs {
    fn lock_binding(&self) -> BoxFuture<'_, Result<Box<dyn Send>, String>> {
        Box::pin(async move {
            let server = self
                .server
                .upgrade()
                .ok_or("schedule executor unavailable")?;
            Ok(Box::new(server.turns.clone().lock_owned().await) as Box<dyn Send>)
        })
    }

    fn resolve<'a>(
        &'a self,
        session: &'a Session,
        reference: &'a ScheduleReference,
        included_attempt: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            self.server
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("schedule executor unavailable"))?
                .private_schedule_text(session, reference, &self.agent, included_attempt)
                .await
        })
    }

    fn project<'a>(
        &'a self,
        session: &'a Session,
        attempt: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.server
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("schedule executor unavailable"))?
                .project_scheduled_attempt(session, attempt)
                .await
        })
    }

    #[cfg(test)]
    fn on_closing(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let Some(server) = self.server.upgrade() else {
                return;
            };
            let barrier = server.schedule_closing_barrier.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.parked.notify_one();
                barrier.release.notified().await;
            }
        })
    }
}
