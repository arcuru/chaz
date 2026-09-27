//! Executor-owned admission and recovery for local agent jobs.
//!
//! A client command is a request, not authority. The executor derives the
//! parent ceiling and pins it in the child before committing a Directive.

use super::Server;
use crate::grants::Grants;
use crate::session::jobs::{AcceptedAgentJob, StagedAgentDefinition, read_accepted_job};
use crate::session::{EntryType, Session, SessionCommand, SessionCommandOutcome, TurnRequestId};
use crate::tool::ScopedTools;
use crate::types::ConversationId;
use eidetica::entry::ID;
use eidetica::store::DocStore;
use std::collections::BTreeMap;

impl Server {
    pub(super) async fn execute_submit_agent_command(
        &self,
        parent_id: &str,
        command_id: &TurnRequestId,
        agent_ref: &str,
        task: &str,
    ) -> SessionCommandOutcome {
        match self
            .submit_agent_job_from_command(parent_id, command_id, agent_ref, task)
            .await
        {
            Ok(session_db_id) => SessionCommandOutcome::AgentJobAccepted { session_db_id },
            Err(error) => SessionCommandOutcome::Rejected {
                message: error.to_string(),
            },
        }
    }

    async fn submit_agent_job_from_command(
        &self,
        parent_id: &str,
        command_id: &TurnRequestId,
        agent_ref: &str,
        task: &str,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(self.executor_authorized, "client cannot admit agent jobs");
        anyhow::ensure!(
            !agent_ref.is_empty() && !task.is_empty(),
            "agent and task required"
        );
        let parent_row = self
            .registry
            .list_sessions()
            .await?
            .into_iter()
            .find(|row| row.session_db_id == parent_id)
            .ok_or_else(|| anyhow::anyhow!("parent session not indexed"))?;
        anyhow::ensure!(
            !parent_row
                .source
                .as_deref()
                .is_some_and(|s| s.starts_with("spawn:") || s.starts_with("job-stage:")),
            "spawned parent has no recoverable job authority"
        );
        let (_, parent_db) = self.registry.open_session(parent_id).await?;
        let parent = Session::new(ConversationId(parent_id.to_string()), parent_db.clone()).await;
        let request = parent
            .command_requests(&Default::default())
            .await?
            .into_iter()
            .find(|work| work.request.command_id == *command_id)
            .ok_or_else(|| anyhow::anyhow!("missing parent command"))?;
        anyhow::ensure!(
            matches!(request.request.command, SessionCommand::SubmitAgent { agent_ref: ref requested_agent, task: ref requested_task } if requested_agent == agent_ref && requested_task == task),
            "parent command does not match job request"
        );
        let meta = parent.read_meta().await;
        anyhow::ensure!(
            meta.agents.len() == 1
                && meta
                    .host_agent_db_id
                    .as_deref()
                    .is_none_or(|id| id == meta.agents[0].db_id),
            "job parent requires exactly one hosted Agent"
        );
        let parent_name = &meta.agents[0].display_name;
        let parent_agent = self
            .agents
            .get(parent_name)
            .ok_or_else(|| anyhow::anyhow!("parent Agent not hosted"))?;
        let strict_capabilities = strict_session_capabilities(&parent_db).await?;
        anyhow::ensure!(
            strict_capabilities == Grants::default() && meta.capabilities == strict_capabilities,
            "narrow or malformed parent scope is not supported by local broad jobs"
        );
        let runtime = self.sessions.lock().await;
        let registered = runtime
            .get(parent_id)
            .ok_or_else(|| anyhow::anyhow!("parent session is not registered with executor"))?;
        anyhow::ensure!(
            registered.parent_tools.is_none() && registered.call_depth == 0,
            "parent has an ephemeral inherited scope; cannot submit a durable job"
        );
        drop(runtime);

        let target = self
            .agent_index
            .find_by_name(agent_ref)
            .or_else(|| {
                ID::parse(agent_ref)
                    .ok()
                    .and_then(|id| self.agent_index.find_by_id(&id))
            })
            .ok_or_else(|| anyhow::anyhow!("target Agent not hosted on executor"))?;
        let target_name = target.display_name.clone();
        let target_agent = self
            .agents
            .get(&target_name)
            .ok_or_else(|| anyhow::anyhow!("target Agent runtime unavailable"))?;
        let parent_active = self
            .active_extensions_for_agent(parent_id, parent_name)
            .await;
        let parent_tools = ScopedTools::new(self.tools.clone(), parent_agent.allowed_tools.clone())
            .with_active_extensions(Some(parent_active));
        let mut tool_ceilings = BTreeMap::new();
        for name in parent_tools.permitted_names() {
            if name == "spawn_worker" {
                continue;
            }
            let tool = parent_tools
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("parent tool disappeared during admission"))?;
            let policy = self.policies.resolve(tool.as_ref());
            let grants = policy
                .grants
                .attenuate(Some(&strict_capabilities))
                .attenuate(Some(&parent_agent.capabilities))
                .attenuate(parent_agent.grants.get(&name));
            tool_ceilings.insert(name, grants);
        }
        let ceiling = strict_capabilities.attenuate(Some(&parent_agent.capabilities));
        let definition = StagedAgentDefinition {
            target: target_name.clone(),
            task: task.to_string(),
            executor_pubkey: target.pubkey.to_string(),
            call_depth: 1,
            max_call_depth: (parent_agent.max_iterations as usize)
                .min(target_agent.max_iterations as usize),
            allowed_tools: tool_ceilings.keys().cloned().collect(),
            tool_ceilings,
            capability_ceiling: ceiling.clone(),
        };
        anyhow::ensure!(
            definition.call_depth <= definition.max_call_depth,
            "spawn depth exceeds Agent ceiling"
        );
        let staged = self
            .registry
            .stage_agent_job(parent_id, command_id.as_str(), definition.clone())
            .await?;
        let (_, child_db) = self.registry.open_session(&staged.session_db_id).await?;
        let existing = read_accepted_job(&child_db).await?;
        if existing.is_none() {
            self.registry
                .attach_agent_to_session(&staged.session_db_id, &target)
                .await?;
            let child = Session::new(
                ConversationId(staged.session_db_id.clone()),
                child_db.clone(),
            )
            .await;
            child
                .update_meta(|meta| meta.capabilities = ceiling)
                .await?;
            anyhow::ensure!(
                self.peer_is_home_for(&staged.session_db_id, &target_name)
                    .await,
                "target Agent is hosted on another peer"
            );
        }
        let accepted = self
            .registry
            .accept_staged_agent_job(
                &staged,
                parent_id,
                command_id.as_str(),
                &definition,
                parent_name,
            )
            .await?;
        anyhow::ensure!(
            !accepted.directive_id.is_empty(),
            "job acceptance missing Directive identity"
        );
        Ok(staged.session_db_id)
    }

    /// Install a restartable child runtime only after parent-command and
    /// child-scope revalidation. A staged child remains unseen and inert.
    pub(super) async fn register_accepted_job(
        &self,
        child_db: &eidetica::Database,
    ) -> anyhow::Result<bool> {
        let Some(accepted) = self.validate_accepted_job(child_db).await? else {
            return Ok(false);
        };
        let session_db_id = child_db.root_id().to_string();
        if self.watched.lock().await.contains(&session_db_id) {
            return Ok(true);
        }
        let allowed = Some(accepted.definition.allowed_tools.clone());
        self.sessions.lock().await.insert(
            session_db_id.clone(),
            super::SessionRuntime {
                backend: self.default_backend.clone(),
                agent_override: Some(accepted.definition.target.clone()),
                approval_tx: None,
                call_depth: accepted.definition.call_depth,
                max_call_depth: accepted.definition.max_call_depth,
                parent_tools: Some(ScopedTools::new(self.tools.clone(), allowed)),
                iteration_budget: None,
                completion_tx: None,
            },
        );
        if let Err(error) = self
            .claim_runtime(
                child_db,
                accepted.definition.target.clone(),
                accepted.definition.call_depth,
                crate::config::RuntimeMode::Always,
            )
            .await
        {
            self.sessions.lock().await.remove(&session_db_id);
            return Err(error);
        }
        let observed_tips = match child_db.snapshot().await {
            Ok(tips) => tips,
            Err(error) => {
                self.deregister_session(&session_db_id).await;
                return Err(error.into());
            }
        };
        let tx = self.notify_tx.clone();
        let sid = session_db_id.clone();
        let callback = child_db
            .on_write_at_tips(observed_tips, move |_, _| {
                let tx = tx.clone();
                let sid = sid.clone();
                Box::pin(async move {
                    let _ = tx.send(super::ProcessingCommand::Wake(sid)).await;
                    Ok(())
                })
            })
            .await;
        match callback {
            Ok(callback) => callback.detach(),
            Err(error) => {
                self.deregister_session(&session_db_id).await;
                return Err(error.into());
            }
        }
        self.watched.lock().await.insert(session_db_id.clone());
        self.maintain_job_claim(child_db.clone(), accepted);
        // Reconcile the persisted Directive even if the acceptance commit
        // preceded this callback or the process restarted after acceptance.
        let _ = self
            .notify_tx
            .send(super::ProcessingCommand::Wake(session_db_id))
            .await;
        Ok(true)
    }

    /// Validate acceptance from the parent request as well as the child row.
    /// A child DB marker or Directive alone cannot authorize local execution.
    pub(super) async fn validate_accepted_job(
        &self,
        child_db: &eidetica::Database,
    ) -> anyhow::Result<Option<AcceptedAgentJob>> {
        let Some(accepted) = read_accepted_job(child_db).await? else {
            return Ok(None);
        };
        let (_, parent_db) = self.registry.open_session(&accepted.parent_id).await?;
        let parent = Session::new(ConversationId(accepted.parent_id.clone()), parent_db).await;
        anyhow::ensure!(
            crate::session::jobs::acceptance_matches_staging(child_db, &accepted).await?,
            "accepted job differs from staged definition"
        );
        let matching = parent
            .command_requests(&Default::default())
            .await?
            .into_iter()
            .find(|work| {
                blake3::hash(work.request.command_id.as_str().as_bytes())
                    .to_hex()
                    .as_str()
                    == accepted.request_key_hash
            })
            .ok_or_else(|| anyhow::anyhow!("accepted job has no matching parent request"))?;
        anyhow::ensure!(
            !matches!(matching.state, crate::session::TurnRequestState::Queued),
            "accepted job parent command was never started by executor"
        );
        let SessionCommand::SubmitAgent { agent_ref, task } = &matching.request.command else {
            anyhow::bail!("accepted job parent command is not SubmitAgent");
        };
        anyhow::ensure!(
            task == &accepted.definition.task,
            "accepted job task differs"
        );
        let target = self
            .agent_index
            .find_by_name(agent_ref)
            .or_else(|| {
                ID::parse(agent_ref)
                    .ok()
                    .and_then(|id| self.agent_index.find_by_id(&id))
            })
            .ok_or_else(|| anyhow::anyhow!("accepted target is no longer hosted"))?;
        anyhow::ensure!(
            target.display_name == accepted.definition.target
                && target.pubkey.to_string() == accepted.definition.executor_pubkey,
            "accepted job executor or target changed"
        );
        let meta = parent.read_meta().await;
        anyhow::ensure!(
            meta.agents.len() == 1
                && meta
                    .host_agent_db_id
                    .as_deref()
                    .is_none_or(|id| id == meta.agents[0].db_id),
            "accepted job parent no longer has a single hosted Agent"
        );
        let parent_name = &meta.agents[0].display_name;
        let parent_agent = self
            .agents
            .get(parent_name)
            .ok_or_else(|| anyhow::anyhow!("accepted parent Agent is unavailable"))?;
        let parent_cap = strict_session_capabilities(parent.database()).await?;
        anyhow::ensure!(
            parent_cap == Grants::default() && meta.capabilities == parent_cap,
            "accepted parent is not a valid broad local scope"
        );
        let expected_cap = parent_cap.attenuate(Some(&parent_agent.capabilities));
        anyhow::ensure!(
            accepted.definition.capability_ceiling == expected_cap
                && strict_session_capabilities(child_db).await? == expected_cap,
            "accepted job capability ceiling differs from parent"
        );
        let parent_active = self
            .active_extensions_for_agent(&accepted.parent_id, parent_name)
            .await;
        let parent_tools = ScopedTools::new(self.tools.clone(), parent_agent.allowed_tools.clone())
            .with_active_extensions(Some(parent_active));
        let mut expected_tools = BTreeMap::new();
        for name in parent_tools.permitted_names() {
            if name == "spawn_worker" {
                continue;
            }
            let tool = parent_tools
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("parent tool disappeared during validation"))?;
            let policy = self.policies.resolve(tool.as_ref());
            expected_tools.insert(
                name.clone(),
                policy
                    .grants
                    .attenuate(Some(&parent_cap))
                    .attenuate(Some(&parent_agent.capabilities))
                    .attenuate(parent_agent.grants.get(&name)),
            );
        }
        anyhow::ensure!(
            accepted.definition.tool_ceilings == expected_tools
                && accepted.definition.allowed_tools
                    == expected_tools.keys().cloned().collect::<Vec<_>>(),
            "accepted job tool authority differs from parent"
        );
        let target_agent = self
            .agents
            .get(&target.display_name)
            .ok_or_else(|| anyhow::anyhow!("accepted target Agent runtime unavailable"))?;
        anyhow::ensure!(
            accepted.definition.call_depth == 1
                && accepted.definition.max_call_depth
                    == (parent_agent.max_iterations as usize)
                        .min(target_agent.max_iterations as usize),
            "accepted job depth differs from parent ceiling"
        );
        anyhow::ensure!(
            self.peer_is_home_for(&child_db.root_id().to_string(), &target.display_name)
                .await,
            "accepted job target is not home on this executor"
        );
        let label = format!(
            "job-stage:{}",
            blake3::hash(
                format!("{}\0{}", accepted.parent_id, matching.request.command_id).as_bytes()
            )
            .to_hex()
        );
        anyhow::ensure!(
            self.registry.list_sessions().await?.iter().any(|row| {
                row.session_db_id == child_db.root_id().to_string()
                    && row.source.as_deref() == Some(label.as_str())
            }),
            "accepted child does not match the parent request catalog entry"
        );
        let child = Session::new(
            ConversationId(child_db.root_id().to_string()),
            child_db.clone(),
        )
        .await;
        let child_meta = child.read_meta().await;
        anyhow::ensure!(
            child_meta.agents.len() == 1
                && child_meta.agents[0].db_id == target.db_id.to_string()
                && child_meta.agents[0].home_pubkey.as_deref()
                    == Some(accepted.definition.executor_pubkey.as_str()),
            "accepted job child Agent/home identity differs"
        );
        let mut entries = child.entries_with_ids();
        let only = entries
            .next()
            .ok_or_else(|| anyhow::anyhow!("accepted job has no Directive"))?;
        anyhow::ensure!(
            only.0.as_str() == accepted.directive_id
                && only.1.entry_type == EntryType::Directive
                && only.1.content == accepted.definition.task,
            "accepted job Directive differs"
        );
        anyhow::ensure!(
            !entries.any(|(_, entry)| {
                entry.entry_type == EntryType::Directive
                    || (entry.entry_type == EntryType::Message
                        && entry.sender != accepted.definition.target)
            }),
            "accepted job contains an additional runnable request"
        );
        // Completed attempts append Ack/Message/Error; verify only the
        // submission prefix, not the later transcript.
        Ok(Some(accepted))
    }
}

async fn strict_session_capabilities(db: &eidetica::Database) -> anyhow::Result<Grants> {
    let txn = db.new_transaction().await?;
    let doc = txn.get_store::<DocStore>("meta").await?.get_all().await?;
    match doc.get("capabilities") {
        None => Ok(Grants::default()),
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(serde_json::from_str(&json)?)
        }
    }
}

impl Server {
    /// Resolve a lost submission acknowledgement without starting another job.
    pub async fn lookup_job(
        &self,
        parent_id: &str,
        request_key: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .registry
            .lookup_job_by_key(parent_id, request_key)
            .await?
            .map(|job| job.session_db_id))
    }

    /// Inspect a job by its session DB ID. A handle is not an authorization
    /// grant; opening the DB still enforces Eidetica session permissions.
    pub async fn job_status(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<crate::session::jobs::JobStatus> {
        let (_, db) = match self.registry.open_session(session_db_id).await {
            Ok(opened) => opened,
            Err(_) => self.registry.open_job_session(session_db_id).await?,
        };
        let live = self.live_attempts.lock().await.clone();
        crate::session::jobs::status_from_db(&db, &live, self.executor_authorized).await
    }

    /// A timed-out wait returns current durable status; execution continues.
    pub async fn wait_job(
        &self,
        session_db_id: &str,
        deadline: std::time::Duration,
    ) -> anyhow::Result<crate::session::jobs::JobStatus> {
        anyhow::ensure!(
            deadline <= std::time::Duration::from_secs(300),
            "job wait must be bounded to 300 seconds"
        );
        let (_, db) = match self.registry.open_session(session_db_id).await {
            Ok(opened) => opened,
            Err(_) => self.registry.open_job_session(session_db_id).await?,
        };
        let tips = db.snapshot().await?;
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let notify_on_write = notify.clone();
        let _callback = db
            .on_write_at_tips(tips, move |_, _| {
                let notify = notify_on_write.clone();
                async move {
                    notify.notify_one();
                    Ok(())
                }
            })
            .await?;
        let observation = async {
            loop {
                let status = self.job_status(session_db_id).await?;
                if status.state.is_terminal()
                    || matches!(
                        status.state,
                        crate::session::jobs::JobState::Interrupted { .. }
                    )
                {
                    return Ok(status);
                }
                tokio::select! {
                    _ = notify.notified() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {},
                }
            }
        };
        match tokio::time::timeout(deadline, observation).await {
            Ok(result) => result,
            Err(_) => self.job_status(session_db_id).await,
        }
    }
}

impl Server {
    /// Model tool calls already run on the executor while holding the parent
    /// processing slot; waiting for that slot to process the command would
    /// deadlock. Commit the same typed request and settle it inline instead.
    pub async fn submit_agent_job_inline(
        &self,
        parent_id: &str,
        command_id: crate::session::TurnRequestId,
        sender: &str,
        created_at: chrono::DateTime<chrono::Utc>,
        agent_ref: &str,
        task: &str,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.executor_authorized,
            "client cannot execute job admission"
        );
        let _admission = self.job_submission_lock.lock().await;
        let (id, db) = self.registry.open_session(parent_id).await?;
        let session = Session::new(id, db).await;
        session
            .submit_command(crate::session::SessionCommandRequest {
                command_id: command_id.clone(),
                sender: sender.to_string(),
                created_at,
                command: SessionCommand::SubmitAgent {
                    agent_ref: agent_ref.to_string(),
                    task: task.to_string(),
                },
            })
            .await?;
        if let Some(result) = session.command_result(&command_id).await? {
            return match result.outcome {
                SessionCommandOutcome::AgentJobAccepted { session_db_id } => Ok(session_db_id),
                SessionCommandOutcome::Rejected { message }
                | SessionCommandOutcome::Failed { message } => anyhow::bail!(message),
                _ => anyhow::bail!("job request has a non-job command result"),
            };
        }
        let live = self.live_attempts.lock().await.clone();
        let work = session
            .command_requests(&live)
            .await?
            .into_iter()
            .find(|work| work.request.command_id == command_id)
            .ok_or_else(|| anyhow::anyhow!("durable job command disappeared"))?;
        let attempt = match work.state {
            crate::session::TurnRequestState::Queued => {
                let attempt = session.start_turn_attempt(command_id.clone()).await?;
                self.live_attempts
                    .lock()
                    .await
                    .insert(attempt.attempt_id.clone());
                attempt
            }
            crate::session::TurnRequestState::InFlight { attempt_id } => {
                anyhow::bail!("job admission {command_id} is already in flight ({attempt_id})")
            }
            crate::session::TurnRequestState::Interrupted { attempt_id } => {
                anyhow::bail!(
                    "job admission {command_id} has uncertain outcome ({attempt_id}); inspect its child by request key"
                )
            }
            crate::session::TurnRequestState::Completed { .. } => {
                anyhow::bail!("completed job command has no durable result")
            }
        };
        let outcome = self
            .execute_submit_agent_command(parent_id, &command_id, agent_ref, task)
            .await;
        let completed = session.complete_command_attempt(&attempt, outcome).await;
        self.live_attempts.lock().await.remove(&attempt.attempt_id);
        let result = completed?;
        match result.outcome {
            SessionCommandOutcome::AgentJobAccepted { session_db_id } => Ok(session_db_id),
            SessionCommandOutcome::Rejected { message }
            | SessionCommandOutcome::Failed { message } => anyhow::bail!(message),
            _ => anyhow::bail!("job command settled with a non-job result"),
        }
    }
}

impl Server {
    /// Only the executor with the accepted Agent/home identity can claim. A
    /// pubkey identifies the home peer, not a process generation.
    pub(super) async fn claim_accepted_job(
        &self,
        db: &eidetica::Database,
        accepted: &AcceptedAgentJob,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(self.executor_authorized, "client cannot claim agent jobs");
        let target = self
            .agent_index
            .find_by_name(&accepted.definition.target)
            .ok_or_else(|| anyhow::anyhow!("claim target Agent is not hosted"))?;
        anyhow::ensure!(
            target.pubkey.to_string() == accepted.definition.executor_pubkey
                && self
                    .peer_is_home_for(&db.root_id().to_string(), &target.display_name)
                    .await,
            "claimant does not hold target Agent/home identity"
        );
        // Recheck the full inherited authority before taking an ownership turn.
        anyhow::ensure!(
            self.validate_accepted_job(db).await?.as_ref() == Some(accepted),
            "job admission authority changed"
        );
        crate::session::jobs::claim_job(
            db,
            &target.pubkey.to_string(),
            &target.db_id.to_string(),
            &self.job_incarnation,
        )
        .await
    }

    /// A registered job keeps its claim fresh and periodically retries stale
    /// claims after executor death; wakeups also arrive on session DB writes.
    pub(super) fn maintain_job_claim(&self, db: eidetica::Database, accepted: AcceptedAgentJob) {
        let notify = self.notify_tx.clone();
        let incarnation = self.job_incarnation.clone();
        let authorized = self.executor_authorized;
        let peer = accepted.definition.executor_pubkey.clone();
        let agent_db_id = self
            .agent_index
            .find_by_name(&accepted.definition.target)
            .expect("validated target")
            .db_id
            .to_string();
        let shutdown = self.shutting_down.clone();
        let live_attempts = self.live_attempts.clone();
        self.track_task(tokio::spawn(async move {
            if !authorized {
                return;
            }
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                // Don't renew a settled job. An interrupted attempt stays held
                // until explicit retry, even after a new process takes over.
                match crate::session::jobs::read_job_result(&db).await {
                    Ok(Some(_)) => break,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "Job receipt read failed; pausing claim refresh");
                        continue;
                    }
                }
                let meta = crate::session::read_meta_from_db(&db).await;
                if meta.agents.len() != 1
                    || meta.agents[0].db_id != agent_db_id
                    || meta.agents[0].home_pubkey.as_deref() != Some(peer.as_str())
                {
                    tracing::warn!("Job Agent/home identity changed; stopping claim refresh");
                    break;
                }
                let live = live_attempts.lock().await.clone();
                match crate::session::jobs::status_from_db(&db, &live, true).await {
                    Ok(status) if status.state.is_terminal() => break,
                    Ok(status)
                        if matches!(
                            status.state,
                            crate::session::jobs::JobState::Interrupted { .. }
                        ) =>
                    {
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error, "Job status read failed; pausing claim refresh");
                        continue;
                    }
                }
                match crate::session::jobs::claim_job(&db, &peer, &agent_db_id, &incarnation).await
                {
                    Ok(true) => {
                        let _ = notify
                            .send(super::ProcessingCommand::Wake(db.root_id().to_string()))
                            .await;
                    }
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "Job claim refresh failed"),
                }
            }
        }));
    }
}
