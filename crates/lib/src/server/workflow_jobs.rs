//! Admission only. A workflow parent is parked; no graph driver runs here.
use super::Server;
use crate::extensions::orchestrator::spec::{FlowSpec, JoinReducer};
use crate::grants::Grants;
use crate::session::workflow_jobs::{
    AcceptedWorkflowParent, WorkflowAuthority, WorkflowParentState, WorkflowParentStatus,
    read_accepted_workflow,
};
use crate::session::{Session, SessionCommand, SessionCommandOutcome, TurnRequestId};
use crate::tool::ScopedTools;
use crate::types::ConversationId;
use eidetica::entry::ID;
use eidetica::store::DocStore;
use std::collections::BTreeMap;

fn agents_in_flow(flow: &FlowSpec, names: &mut Vec<String>) {
    match flow {
        FlowSpec::Spawn(s) => names.push(s.agent.clone()),
        FlowSpec::Sequence(s) => s.steps.iter().for_each(|node| agents_in_flow(node, names)),
        FlowSpec::Fork(s) => s
            .branches
            .values()
            .for_each(|node| agents_in_flow(node, names)),
        FlowSpec::Join(s) => {
            if let Some(JoinReducer::Agent(a)) = &s.reducer {
                names.push(a.agent.clone());
            }
        }
    }
}

async fn strict_capabilities(db: &eidetica::Database) -> anyhow::Result<Grants> {
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
    pub(super) async fn execute_submit_workflow_command(
        &self,
        parent_id: &str,
        command_id: &TurnRequestId,
        flow: &FlowSpec,
    ) -> SessionCommandOutcome {
        match self
            .submit_workflow_parent_from_command(parent_id, command_id, flow)
            .await
        {
            Ok(session_db_id) => SessionCommandOutcome::WorkflowParentAccepted { session_db_id },
            Err(error) => SessionCommandOutcome::Rejected {
                message: error.to_string(),
            },
        }
    }

    async fn submit_workflow_parent_from_command(
        &self,
        parent_id: &str,
        command_id: &TurnRequestId,
        flow: &FlowSpec,
    ) -> anyhow::Result<String> {
        let (authority, agent) =
            Box::pin(self.derive_workflow_authority(parent_id, command_id, flow)).await?;
        let handle = self
            .registry
            .accept_workflow_parent(
                parent_id,
                command_id.as_str(),
                flow.clone(),
                authority,
                &agent,
            )
            .await?;
        let (_, db) = self.registry.open_job_session(&handle).await?;
        anyhow::ensure!(
            Box::pin(self.validate_workflow_parent(&db))
                .await?
                .is_some(),
            "uncertain workflow acceptance: parent authority failed validation"
        );
        Ok(handle)
    }

    async fn derive_workflow_authority(
        &self,
        parent_id: &str,
        command_id: &TurnRequestId,
        flow: &FlowSpec,
    ) -> anyhow::Result<(WorkflowAuthority, crate::hosted_index::DbEntry)> {
        anyhow::ensure!(
            self.executor_authorized,
            "client cannot admit workflow parents"
        );
        // Re-parse the entire normalized graph, including references, before
        // creating an intent or catalog entry. The typed field is not a proof.
        let canonical =
            crate::extensions::orchestrator::spec::parse_flow(&serde_json::to_value(flow)?)
                .map_err(|e| anyhow::anyhow!(e))?;
        anyhow::ensure!(&canonical == flow, "workflow graph is not normalized");
        let mut targets = Vec::new();
        agents_in_flow(flow, &mut targets);
        anyhow::ensure!(!targets.is_empty(), "workflow needs an Agent node");
        for name in &targets {
            let target = self
                .agent_index
                .find_by_name(name)
                .or_else(|| {
                    ID::parse(name)
                        .ok()
                        .and_then(|id| self.agent_index.find_by_id(&id))
                })
                .ok_or_else(|| anyhow::anyhow!("workflow target Agent not hosted: {name}"))?;
            anyhow::ensure!(
                self.agents.get(&target.display_name).is_some(),
                "workflow target runtime unavailable"
            );
        }
        let parent_row = self
            .registry
            .list_sessions()
            .await?
            .into_iter()
            .find(|row| row.session_db_id == parent_id)
            .ok_or_else(|| anyhow::anyhow!("caller session not indexed"))?;
        anyhow::ensure!(
            !parent_row
                .source
                .as_deref()
                .is_some_and(|s| s.starts_with("spawn:")),
            "ephemeral caller has no job authority"
        );
        anyhow::ensure!(
            !parent_row
                .source
                .as_deref()
                .is_some_and(|s| s.starts_with("workflow-stage:")),
            "workflow parent has no model turn"
        );
        let (_, parent_db) = self.registry.open_session(parent_id).await?;
        let inherited = if parent_row
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("job-stage:"))
        {
            let accepted = self
                .validate_accepted_job(&parent_db)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Agent job parent not accepted"))?;
            let live = self.live_attempts.lock().await.clone();
            anyhow::ensure!(
                matches!(
                    crate::session::jobs::status_from_db(&parent_db, &live, true)
                        .await?
                        .state,
                    crate::session::jobs::JobState::Running { .. }
                ) && crate::session::jobs::owns_job(&parent_db, &self.job_incarnation).await?,
                "Agent job parent has no active claimed attempt"
            );
            Some(accepted.definition)
        } else {
            None
        };
        let parent = Session::new(ConversationId(parent_id.to_string()), parent_db.clone()).await;
        let request = parent
            .command_requests(&Default::default())
            .await?
            .into_iter()
            .find(|work| work.request.command_id == *command_id)
            .ok_or_else(|| anyhow::anyhow!("missing caller command"))?;
        anyhow::ensure!(
            matches!(request.request.command, SessionCommand::SubmitWorkflow { flow: ref requested } if requested == flow),
            "caller command differs from graph"
        );
        let meta = parent.read_meta().await;
        anyhow::ensure!(
            meta.agents.len() == 1
                && meta
                    .host_agent_db_id
                    .as_deref()
                    .is_none_or(|id| id == meta.agents[0].db_id),
            "workflow caller requires exactly one hosted Agent"
        );
        let agent_ref = &meta.agents[0];
        let agent = self
            .agent_index
            .find_by_name(&agent_ref.display_name)
            .ok_or_else(|| anyhow::anyhow!("caller Agent not hosted"))?;
        anyhow::ensure!(
            agent.db_id.to_string() == agent_ref.db_id
                && agent_ref.home_pubkey.as_deref() == Some(agent.pubkey.to_string().as_str())
                && self.peer_is_home_for(parent_id, &agent.display_name).await,
            "caller Agent/home identity differs"
        );
        let runtime_agent = self
            .agents
            .get(&agent.display_name)
            .ok_or_else(|| anyhow::anyhow!("caller runtime unavailable"))?;
        let cap = strict_capabilities(&parent_db).await?;
        anyhow::ensure!(
            meta.capabilities == cap
                && inherited
                    .as_ref()
                    .map_or(cap == Grants::default(), |p| cap == p.capability_ceiling
                        && p.target == agent.display_name),
            "narrow or malformed workflow caller scope is unsupported"
        );
        let runtime = self.sessions.lock().await;
        let registered = runtime
            .get(parent_id)
            .ok_or_else(|| anyhow::anyhow!("caller not registered with executor"))?;
        anyhow::ensure!(
            inherited.as_ref().map_or(
                registered.parent_tools.is_none() && registered.call_depth == 0,
                |p| registered.parent_tools.is_some()
                    && registered.call_depth == p.call_depth
                    && registered.max_call_depth == p.max_call_depth
            ),
            "caller runtime differs from inherited authority"
        );
        drop(runtime);
        let depth = inherited.as_ref().map_or(1, |p| p.call_depth + 1);
        let max_depth = inherited
            .as_ref()
            .map_or(runtime_agent.max_iterations as usize, |p| p.max_call_depth);
        anyhow::ensure!(
            depth <= max_depth && depth <= 128,
            "workflow depth exceeds Agent ceiling"
        );
        let active = self
            .active_extensions_for_agent(parent_id, &agent.display_name)
            .await;
        let tools = ScopedTools::new(self.tools.clone(), runtime_agent.allowed_tools.clone())
            .with_active_extensions(Some(active));
        let mut tool_ceilings = BTreeMap::new();
        for name in tools.permitted_names() {
            if name == "spawn_worker"
                || inherited
                    .as_ref()
                    .is_some_and(|p| !p.tool_ceilings.contains_key(&name))
            {
                continue;
            }
            let tool = tools
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("caller tool disappeared"))?;
            let grants = self
                .policies
                .resolve(tool.as_ref())
                .grants
                .attenuate(Some(&cap))
                .attenuate(Some(&runtime_agent.capabilities))
                .attenuate(runtime_agent.grants.get(&name))
                .attenuate(inherited.as_ref().and_then(|p| p.tool_ceilings.get(&name)));
            tool_ceilings.insert(name, grants);
        }
        let authority = WorkflowAuthority {
            agent: agent.display_name.clone(),
            agent_db_id: agent.db_id.to_string(),
            home_pubkey: agent.pubkey.to_string(),
            call_depth: depth,
            max_call_depth: max_depth,
            tool_ceilings,
            capability_ceiling: cap.attenuate(Some(&runtime_agent.capabilities)),
        };
        Ok((authority, agent))
    }

    /// Read a parent handle without pretending the graph has executed.
    pub async fn workflow_parent_status(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<WorkflowParentStatus> {
        let (_, db) = self.registry.open_job_session(session_db_id).await?;
        anyhow::ensure!(
            crate::session::workflow_jobs::is_workflow_parent(&db).await,
            "not a workflow parent"
        );
        let accepted = if self.executor_authorized {
            Box::pin(self.validate_workflow_parent(&db))
                .await?
                .is_some()
        } else if let Some(record) = read_accepted_workflow(&db).await? {
            let (_, parent_db) = self.registry.open_session(&record.parent_id).await?;
            let parent = Session::new(ConversationId(record.parent_id.clone()), parent_db).await;
            let matching = parent
                .command_requests(&Default::default())
                .await?
                .into_iter()
                .find(|work| {
                    blake3::hash(work.request.command_id.as_str().as_bytes())
                        .to_hex()
                        .as_str()
                        == record.request_key_hash
                });
            if let Some(work) = matching {
                let receipt = parent.command_result(&work.request.command_id).await?;
                matches!(work.request.command, SessionCommand::SubmitWorkflow { flow } if flow == record.flow)
                    && matches!(receipt.map(|r| r.outcome), Some(SessionCommandOutcome::WorkflowParentAccepted { session_db_id }) if session_db_id == db.root_id().to_string())
                    && self
                        .registry
                        .lookup_workflow_parent(&record.parent_id, work.request.command_id.as_str())
                        .await?
                        .as_deref()
                        == Some(session_db_id)
            } else {
                false
            }
        } else {
            false
        };
        let state = if accepted {
            WorkflowParentState::Accepted
        } else {
            WorkflowParentState::Staged
        };
        Ok(WorkflowParentStatus {
            session_db_id: session_db_id.to_string(),
            state,
        })
    }

    /// A marker is not sufficient: check catalog, parent intent, started
    /// command, and pinned Agent/home and graph. This is a consistency check,
    /// not a child-effects grant: same-login clients can write session DBs.
    /// A future graph driver must establish executor provenance independently.
    pub async fn validate_workflow_parent(
        &self,
        db: &eidetica::Database,
    ) -> anyhow::Result<Option<AcceptedWorkflowParent>> {
        let Some(record) = read_accepted_workflow(db).await? else {
            return Ok(None);
        };
        let (_, parent_db) = self.registry.open_session(&record.parent_id).await?;
        let parent = Session::new(ConversationId(record.parent_id.clone()), parent_db).await;
        let matching = parent
            .command_requests(&Default::default())
            .await?
            .into_iter()
            .find(|work| {
                blake3::hash(work.request.command_id.as_str().as_bytes())
                    .to_hex()
                    .as_str()
                    == record.request_key_hash
            })
            .ok_or_else(|| anyhow::anyhow!("workflow parent has no caller command"))?;
        anyhow::ensure!(
            !matches!(matching.state, crate::session::TurnRequestState::Queued),
            "workflow caller command was never started"
        );
        anyhow::ensure!(
            matches!(matching.request.command, SessionCommand::SubmitWorkflow { ref flow } if flow == &record.flow),
            "workflow graph differs from caller command"
        );
        let accepted = self
            .registry
            .lookup_workflow_parent(&record.parent_id, matching.request.command_id.as_str())
            .await?;
        anyhow::ensure!(
            accepted.as_deref() == Some(db.root_id().to_string().as_str()),
            "workflow catalog or parent intent differs"
        );
        let canonical =
            crate::extensions::orchestrator::spec::parse_flow(&serde_json::to_value(&record.flow)?)
                .map_err(|e| anyhow::anyhow!(e))?;
        anyhow::ensure!(canonical == record.flow, "workflow graph is invalid");
        let (expected, _) = Box::pin(self.derive_workflow_authority(
            &record.parent_id,
            &matching.request.command_id,
            &record.flow,
        ))
        .await?;
        anyhow::ensure!(
            record.authority == expected,
            "workflow authority differs from caller ceiling"
        );
        let agent = self
            .agent_index
            .find_by_name(&record.authority.agent)
            .ok_or_else(|| anyhow::anyhow!("workflow Agent not hosted"))?;
        let meta = Session::new(ConversationId(db.root_id().to_string()), db.clone())
            .await
            .read_meta()
            .await;
        anyhow::ensure!(
            agent.db_id.to_string() == record.authority.agent_db_id
                && agent.pubkey.to_string() == record.authority.home_pubkey
                && meta.agents.len() == 1
                && meta.agents[0].db_id == record.authority.agent_db_id
                && meta.agents[0].home_pubkey.as_deref()
                    == Some(record.authority.home_pubkey.as_str())
                && meta.capabilities == record.authority.capability_ceiling
                && strict_capabilities(db).await? == record.authority.capability_ceiling,
            "workflow Agent/home or ceiling differs"
        );
        anyhow::ensure!(
            Session::new(ConversationId(db.root_id().to_string()), db.clone())
                .await
                .entries()
                .is_empty(),
            "workflow parent must not contain a Directive or model turn"
        );
        Ok(Some(record))
    }
}

impl Server {
    /// A model tool is already inside its parent turn; waiting for the normal
    /// command processor would deadlock the processing slot.
    pub async fn submit_workflow_parent_inline(
        &self,
        parent_id: &str,
        command_id: TurnRequestId,
        sender: &str,
        created_at: chrono::DateTime<chrono::Utc>,
        flow: FlowSpec,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.executor_authorized,
            "client cannot admit workflow parents"
        );
        let _admission = self.job_submission_lock.lock().await;
        let (id, db) = self.registry.open_session(parent_id).await?;
        let session = Session::new(id, db).await;
        session
            .submit_command(crate::session::SessionCommandRequest {
                command_id: command_id.clone(),
                sender: sender.to_string(),
                created_at,
                command: SessionCommand::SubmitWorkflow { flow: flow.clone() },
            })
            .await?;
        if let Some(result) = session.command_result(&command_id).await? {
            return match result.outcome {
                SessionCommandOutcome::WorkflowParentAccepted { session_db_id } => {
                    Ok(session_db_id)
                }
                SessionCommandOutcome::Rejected { message }
                | SessionCommandOutcome::Failed { message } => anyhow::bail!(message),
                _ => anyhow::bail!("workflow request has a non-workflow result"),
            };
        }
        let live = self.live_attempts.lock().await.clone();
        let work = session
            .command_requests(&live)
            .await?
            .into_iter()
            .find(|work| work.request.command_id == command_id)
            .ok_or_else(|| anyhow::anyhow!("workflow command disappeared"))?;
        let attempt = match work.state {
            crate::session::TurnRequestState::Queued => {
                let attempt = session.start_turn_attempt(command_id.clone()).await?;
                self.live_attempts
                    .lock()
                    .await
                    .insert(attempt.attempt_id.clone());
                attempt
            }
            crate::session::TurnRequestState::InFlight { attempt_id }
            | crate::session::TurnRequestState::Interrupted { attempt_id } => anyhow::bail!(
                "workflow admission uncertain ({attempt_id}); look up the request key"
            ),
            crate::session::TurnRequestState::Completed { .. } => {
                anyhow::bail!("workflow command lacks result")
            }
        };
        let outcome = self
            .execute_submit_workflow_command(parent_id, &command_id, &flow)
            .await;
        let completed = session.complete_command_attempt(&attempt, outcome).await;
        self.live_attempts.lock().await.remove(&attempt.attempt_id);
        match completed?.outcome {
            SessionCommandOutcome::WorkflowParentAccepted { session_db_id } => Ok(session_db_id),
            SessionCommandOutcome::Rejected { message }
            | SessionCommandOutcome::Failed { message } => anyhow::bail!(message),
            _ => anyhow::bail!("workflow command settled with a non-workflow result"),
        }
    }

    pub async fn lookup_workflow_parent(
        &self,
        parent_id: &str,
        request_key: &str,
    ) -> anyhow::Result<Option<String>> {
        self.registry
            .lookup_workflow_parent(parent_id, request_key)
            .await
    }
}
