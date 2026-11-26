//! Agent-owned schedule fire path for [`Server`].
//!
//! Bounded clock admission and accounting maintenance feed the common runner.
//! The Session-start hook is shared by ordinary and scheduled registration.

use super::*;

/// Read-only accounting maintenance on the ordinary processing loop, never on
/// the clock's admission path. This is not a delivery queue: it neither creates
/// storage nor starts/retries work. Retained handles keep disposable observations
/// alive across pages; every cycle rediscovers current definitions and keys.
#[derive(Default)]
pub(super) struct AccountingPump {
    work: Vec<(crate::agent_db::AgentDb, String)>,
    next: usize,
    projections: super::scheduled_projection::ProjectionPump,
}

impl AccountingPump {
    pub(super) async fn step(&mut self, server: &Server) {
        if !server.executor_authorized
            || !server.is_startup_ready()
            || server.routine_engine().is_none()
        {
            return;
        }
        self.projections.step(server).await;
        if self.next == self.work.len() {
            let mut work = Vec::new();
            for entry in server.agent_index.list() {
                let result = async {
                    let Some(agent) = server
                        .registry
                        .open_scheduling_agent_db(&entry.db_id, Some(&entry.pubkey))
                        .await?
                    else {
                        return Ok::<_, anyhow::Error>(());
                    };
                    let schedules = agent.list_schedules().await?;
                    if !schedules.is_empty() {
                        let agent = if agent.scheduling_reference().await?.is_none() {
                            let mut eligible = false;
                            for schedule in &schedules {
                                if schedule.enabled
                                    && server
                                        .timer_target_authority(&agent, schedule, &entry)
                                        .await
                                        .is_ok()
                                {
                                    eligible = true;
                                    break;
                                }
                            }
                            if !eligible {
                                return Ok(());
                            }
                            server.registry.ensure_scheduling_agent_db(&entry).await?
                        } else {
                            agent
                        };
                        // A present unavailable ledger is never replaced.
                        agent.scheduling_database().await?.ok_or_else(|| {
                            anyhow::anyhow!("selected scheduling Executor unavailable")
                        })?;
                        for schedule in schedules {
                            work.push((agent.clone(), schedule.id));
                        }
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    debug!(agent_db_id = %entry.db_id, %error, "Scheduling accounting discovery held");
                }
            }
            // Open successors before dropping old handles: the shared cache is
            // weakly indexed and would otherwise disappear at every cycle.
            self.work = work;
            self.next = 0;
        }
        let Some((agent, id)) = self.work.get(self.next) else {
            return;
        };
        self.next += 1;
        if server.agent_index.find_by_id(&agent.id()).is_none() {
            return;
        }
        let result = async {
            // Do not finish an audit with a stale definition baseline.
            if let Some(schedule) = agent.find_schedule(id).await?
                && agent
                    .reconcile_schedule_accounting(&schedule, 32)
                    .await?
                    .complete
                && let Some(engine) = server.routine_engine()
            {
                engine.resume_agent_preparation(agent, &schedule).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            debug!(agent_db_id = %agent.id(), schedule_id = %id, %error, "Scheduling accounting reconciliation held");
        }
    }
}

impl Server {
    /// Request one retry of the named terminal delivery in this exact Session.
    /// This publishes work for the existing coordinator; it never starts a turn.
    pub(crate) async fn retry_schedule_delivery(
        &self,
        session_db_id: &str,
        entry: &crate::hosted_index::DbEntry,
        occurrence: &str,
        delivery: &str,
        expected_attempt: &str,
    ) -> anyhow::Result<crate::agent_db::ScheduleReference> {
        use crate::schedule_state::lifecycle::{self, Preparation, retry};
        anyhow::ensure!(
            self.executor_authorized
                && self.is_startup_ready()
                && !self.shutting_down.load(Ordering::Acquire),
            "schedule retry needs the running executor"
        );
        let agent = self.registry.open_scheduling_writer(entry).await?;
        let prior = retry::delivery(&agent, occurrence, delivery).await?;
        let Preparation::HandedOff { input, .. } = &prior.preparation else {
            anyhow::bail!("schedule delivery was not handed off");
        };
        anyhow::ensure!(
            input.session_db_id == session_db_id && self.is_session_open(session_db_id).await,
            "use schedule retry from the delivery's open Session"
        );
        let (conv, db, key) = self
            .registry
            .open_session_with_signer(session_db_id)
            .await?;
        let mut session = Session::try_new(conv, db).await?.with_signing_key(key);
        self.schedule_target_owner(&session, &prior.reference, entry)
            .await?;
        agent
            .resolve_schedule_reference(&prior.reference, Utc::now())
            .await?;
        lifecycle::settle(&agent, &prior.reference, &session).await?;
        retry::explicit(
            &agent,
            &prior.reference,
            expected_attempt,
            &mut session,
            Utc::now(),
        )
        .await
    }

    /// Prepare/reconcile one Fresh Session for an already reserved occurrence.
    /// This does not register a runtime, publish runnable input or start a turn.
    /// The common admission path must still commit handoff and revalidate both
    /// authority guards. Never use a creation retry as execution recovery.
    pub async fn prepare_fresh_schedule_root(
        &self,
        reference: &crate::agent_db::ScheduleReference,
    ) -> anyhow::Result<(ConversationId, eidetica::Database)> {
        anyhow::ensure!(
            self.executor_authorized,
            "client cannot provision scheduled work"
        );
        let owner = eidetica::entry::ID::parse(&reference.owner_agent_db_id)?;
        let entry = self
            .agent_index
            .find_by_id(&owner)
            .ok_or_else(|| anyhow::anyhow!("Fresh owner not hosted"))?;
        let agent = self.registry.open_scheduling_writer(&entry).await?;
        let schedule = agent
            .resolve_schedule_reference(reference, Utc::now())
            .await?;
        anyhow::ensure!(
            matches!(schedule.target, crate::agent_db::ScheduleTarget::Fresh),
            "not a Fresh schedule"
        );
        anyhow::ensure!(
            crate::db_kind::try_read_agent_home_pubkey(agent.database())
                .await?
                .is_none_or(|home| home == entry.pubkey),
            "not home peer for Fresh schedule"
        );
        self.registry
            .prepare_fresh_schedule_root(&agent, &entry, reference)
            .await
    }

    #[cfg(test)]
    pub(super) fn check_schedule_write(&self, boundary: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            *self.schedule_fail_at.lock().unwrap() != Some(boundary),
            "injected schedule {boundary} write failure"
        );
        Ok(())
    }

    /// Acknowledge only committed clock handoff, never Started or completion.
    /// Legacy payloads without a live clock/generation are inert, not execution
    /// authority. Ordered discovery uses the ordinary turn runner and capacity.
    pub async fn dispatch_agent_schedule(
        self: &Arc<Self>,
        payload: crate::routine::AgentSchedulePayload,
    ) -> anyhow::Result<()> {
        self.dispatch_timed_schedule(&payload).await
    }

    /// Compatibility entry point with the same handoff-only contract as the
    /// timer. This cannot execute a legacy prompt/target payload directly.
    pub async fn fire_agent_schedule(
        &self,
        payload: crate::routine::AgentSchedulePayload,
    ) -> anyhow::Result<()> {
        self.dispatch_timed_schedule(&payload).await
    }

    /// Open a hosted agent's Living Agent DB by display name. `None` if
    /// the agent isn't in this peer's hosted index or the DB can't be
    /// opened (no key / read error). Used by per-agent extension
    /// activation (`/extensions … agent`) and the dispatch-time filter.
    pub async fn open_agent_db_by_name(
        &self,
        agent_name: &str,
    ) -> Option<crate::agent_db::AgentDb> {
        let entry = self.agent_index.find_by_name(agent_name)?;
        self.open_agent_db_for_schedule(&entry).await
    }

    /// Per-agent narrowing filter: extension names the agent has
    /// explicitly opted out of, folded from its Living Agent DB's
    /// sparse `extensions` log. Empty when the agent has no records or
    /// isn't hosted here — i.e. "no opinion, allow everything the
    /// session allows".
    pub async fn agent_disabled_extensions(
        &self,
        agent_name: &str,
    ) -> std::collections::HashSet<String> {
        let Some(adb) = self.open_agent_db_by_name(agent_name).await else {
            return std::collections::HashSet::new();
        };
        crate::extension::read_disabled(adb.database())
            .await
            .unwrap_or_default()
    }

    /// True when a generation-bearing payload still names the live engine
    /// entry. A missing generation never grants authority. Rechecked before
    /// durable handoff; current immutable source/target guards own delivery.
    pub(super) async fn schedule_generation_is_live(
        &self,
        payload: &crate::routine::AgentSchedulePayload,
    ) -> bool {
        let Some(generation) = &payload.generation else {
            return false;
        };
        let routine_id = crate::routine::RoutineId::new(format!(
            "agent:{}:{}",
            payload.owner_agent_db_id, payload.schedule_id
        ));
        let current = match self.routine_engine() {
            Some(engine) => engine.current_generation(&routine_id).await,
            None => None,
        };
        current.as_deref() == Some(generation)
    }

    /// Open the agent's Living Agent DB if this peer hosts the agent.
    async fn open_agent_db_for_schedule(
        &self,
        entry: &crate::hosted_index::DbEntry,
    ) -> Option<crate::agent_db::AgentDb> {
        match self
            .registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
        {
            Ok(Some(adb)) => Some(adb),
            Ok(None) => {
                tracing::warn!(agent = %entry.display_name, "No key for schedule owner DB");
                None
            }
            Err(e) => {
                tracing::error!(agent = %entry.display_name, "Failed to open agent DB: {e}");
                None
            }
        }
    }

    /// Build a `HookContext` for the given session and fire `session_start`.
    /// Internal helper shared by `register_session` / `register_child_session`.
    pub(super) async fn fire_session_start_hook(
        &self,
        session_db: eidetica::Database,
        agent_name: String,
        call_depth: usize,
    ) {
        // Framework-level: record activation events for the current extension
        // set onto the session DB. Idempotent on repeat calls; only writes
        // when the set or a version differs from the latest stored event,
        // and respects `Deactivated` (so a `/extensions remove` survives
        // restart). Failure is non-fatal — we'd rather lose provenance for
        // one session-start than block the agent turn.
        if let Err(e) = self.extensions.record_active(&session_db).await {
            tracing::warn!(
                conv = %session_db.root_id(),
                "Failed to record extension activation events: {e}"
            );
        }

        let session_db_id = session_db.root_id().to_string();
        let active_extensions = self.refresh_active_extensions(&session_db_id).await;

        // Captured before the moves below so we can populate the session's
        // extension status outputs at open time — not only at the first
        // turn. The frontend's `on_write` → `SessionChanged` repaints from
        // the store, so a fresh session shows status without a message.
        let status_db = session_db.clone();
        let status_agent = agent_name.clone();

        let conv_id = ConversationId(session_db_id);
        let session = Session::new(conv_id, session_db).await;
        let ctx = HookContext {
            agent_name,
            model: None,
            call_depth,
            session: Arc::new(Mutex::new(session)),
            active_extensions,
            routine_engine: self.routine_engine().cloned(),
        };
        self.extensions.fire_session_start(&ctx).await;
        self.extensions
            .refresh_status_outputs(&status_agent, &status_db)
            .await;
    }
}
