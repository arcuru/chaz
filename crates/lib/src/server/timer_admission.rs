//! The clock produces durable private references, never a second turn runner.
//! Preparation is bounded and inert. Ordered discovery owns delivery/capacity;
//! handler acknowledgement means committed handoff, not Started or success.
use super::*;
use crate::agent_db::{AgentDb, Schedule, ScheduleTarget};
use crate::schedule_state::lifecycle::Preparation;

impl Server {
    pub(super) async fn dispatch_timed_schedule(
        &self,
        payload: &crate::routine::AgentSchedulePayload,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.executor_authorized, "client cannot admit schedules");
        let clock = payload
            .clock
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("timer snapshot missing"))?;
        anyhow::ensure!(payload.generation.is_some(), "timer generation missing");
        if !self.schedule_generation_is_live(payload).await {
            return Ok(());
        }
        let owner = eidetica::entry::ID::parse(&payload.owner_agent_db_id)?;
        let entry = self
            .agent_index
            .find_by_id(&owner)
            .ok_or_else(|| anyhow::anyhow!("schedule owner not hosted"))?;
        // Initialization and paged accounting reconciliation belong to the
        // resident pump, not admission. A present unavailable reference refuses.
        let agent = self.registry.open_scheduling_writer(&entry).await?;
        let Some(schedule) = agent.find_schedule(&payload.schedule_id).await? else {
            return Ok(());
        };
        if !schedule.enabled
            || schedule.lifetime_key() != clock.lifetime
            || schedule.revision_key()? != clock.revision
        {
            return Ok(());
        }
        self.timer_target_authority(&agent, &schedule, &entry)
            .await?;
        if clock.missed {
            use crate::schedule_state::lifecycle::clock::MissedClock;
            match agent
                .miss_schedule_clock(
                    &schedule.id,
                    &clock.revision,
                    clock.slot.clone(),
                    Utc::now(),
                )
                .await?
            {
                MissedClock::Skipped(next) => {
                    // Catch-up off: the window is recorded and the clock moved
                    // on. No occurrence, reservation, Session input or turn.
                    if let Some(engine) = self.routine_engine()
                        && self.schedule_generation_is_live(payload).await
                    {
                        engine.acknowledge_agent_clock(payload, next).await?;
                    }
                    return Ok(());
                }
                // Catch-up on: this slot is the window's single occurrence and
                // its handoff successor coalesces the rest. An already-owned
                // slot is ordinary preparation, not a missed trigger.
                MissedClock::CatchUp | MissedClock::Owned => {}
            }
        }
        #[cfg(test)]
        self.check_schedule_write("timed-preparation")?;
        let row = agent
            .prepare_schedule_clock(
                &schedule.id,
                &clock.revision,
                clock.slot.clone(),
                Utc::now(),
            )
            .await?;
        anyhow::ensure!(
            row.preparation != Preparation::NotApplied,
            "timer preparation was released; explicit action required"
        );
        if matches!(row.preparation, Preparation::HandedOff { .. }) {
            // An earlier attempt committed but lost the timer reply. Do not
            // enter Fresh provisioning or re-prepare a Session input.
            return self
                .acknowledge_timed_handoff(payload, &agent, &schedule, &row)
                .await;
        }
        let sid = match &schedule.target {
            ScheduleTarget::Pinned { session_db_id } => session_db_id.clone(),
            ScheduleTarget::Fresh => {
                let (_, db) = self.prepare_fresh_schedule_root(&row.reference).await?;
                db.root_id().to_string()
            }
        };
        let (conv, db, key) = self.registry.open_session_with_signer(&sid).await?;
        let mut session = Session::new(conv, db).await.with_signing_key(key);
        #[cfg(test)]
        self.check_schedule_write("timed-input")?;
        session.prepare_schedule_input(&row.reference).await?;

        #[cfg(test)]
        {
            let barrier = self.timed_prepared_barrier.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.parked.notify_one();
                barrier.release.notified().await;
            }
        }
        // Delegation/Fresh provisioning may advance keys. Reopen current owned
        // identities, revalidate both guards, then commit the exact inert input.
        let current = self
            .agent_index
            .find_by_id(&owner)
            .ok_or_else(|| anyhow::anyhow!("schedule owner no longer hosted"))?;
        anyhow::ensure!(current.pubkey == entry.pubkey, "schedule owner key changed");
        let agent = self.registry.open_scheduling_writer(&current).await?;
        if !self.schedule_generation_is_live(payload).await {
            agent.release_schedule_preparation(&row.reference).await?;
            return Ok(());
        }
        self.timer_target_authority(&agent, &schedule, &current)
            .await?;
        if matches!(schedule.target, ScheduleTarget::Fresh) {
            crate::session::require_schedule_member(session.database(), agent.database(), &current)
                .await?;
            Session::require_legacy_history(session.database()).await?;
        }
        #[cfg(test)]
        self.check_schedule_write("timed-handoff")?;
        let row = agent
            .handoff_schedule_occurrence(&row.reference, &session, Utc::now())
            .await?;
        #[cfg(test)]
        self.check_schedule_write("timed-handoff-ack")?;
        self.acknowledge_timed_handoff(payload, &agent, &schedule, &row)
            .await
    }

    async fn acknowledge_timed_handoff(
        &self,
        payload: &crate::routine::AgentSchedulePayload,
        agent: &AgentDb,
        schedule: &Schedule,
        row: &crate::schedule_state::lifecycle::Occurrence,
    ) -> anyhow::Result<()> {
        let cursor = agent
            .schedule_clock_cursor(schedule)
            .await?
            .ok_or_else(|| anyhow::anyhow!("committed clock successor missing"))?;
        anyhow::ensure!(
            cursor.last.as_ref() == Some(&row.reference),
            "clock handoff readback differs"
        );
        let next = row
            .clock
            .as_ref()
            .and_then(|clock| clock.handoff.as_ref())
            .ok_or_else(|| anyhow::anyhow!("clock handoff missing"))?
            .next
            .clone();
        anyhow::ensure!(cursor.next == next, "clock successor readback differs");
        if let Some(engine) = self.routine_engine() {
            // A concurrent reload may have already loaded this durable next
            // slot. Its new incarnation must not be changed by this old callback.
            if self.schedule_generation_is_live(payload).await {
                engine.acknowledge_agent_clock(payload, next).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn timer_target_authority(
        &self,
        agent: &AgentDb,
        schedule: &Schedule,
        entry: &crate::hosted_index::DbEntry,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            crate::db_kind::try_read_agent_home_pubkey(agent.database())
                .await?
                .is_none_or(|home| home == entry.pubkey),
            "not home peer for schedule owner"
        );
        if let ScheduleTarget::Pinned { session_db_id } = &schedule.target {
            anyhow::ensure!(
                self.is_session_open(session_db_id).await,
                "ordinary Pinned target is not registered"
            );
            let (_, db) = self.registry.open_session(session_db_id).await?;
            crate::session::require_schedule_member(&db, agent.database(), entry).await?;
            Session::require_legacy_history(&db).await?;
        }
        Ok(())
    }
}
