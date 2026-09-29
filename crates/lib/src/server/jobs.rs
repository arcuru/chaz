//! Executor validation and recovery for submitter-owned local Agent jobs.

use super::Server;
use crate::grants::Grants;
use crate::session::JobRequest;
use crate::session::jobs::{AcceptedAgentJob, StagedAgentDefinition, read_accepted_job};
use crate::session::{EntryType, Session};
use crate::tool::ScopedTools;
use crate::types::ConversationId;
use eidetica::entry::ID;
use eidetica::store::DocStore;
use std::collections::BTreeMap;

impl Server {
    /// Derive authority from the parent and hosted target. Only the executor validates jobs.
    async fn derive_job_definition(
        &self,
        parent_id: &str,
        agent_ref: &str,
        task: &str,
    ) -> anyhow::Result<(StagedAgentDefinition, String, crate::hosted_index::DbEntry)> {
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
                .is_some_and(|s| s.starts_with("spawn:")),
            "spawned parent has no recoverable job authority"
        );
        anyhow::ensure!(
            !parent_row
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("job-stage:")
                    && !source.starts_with("job-stage:submitter:")),
            "obsolete staged job cannot be a parent"
        );
        let (_, parent_db) = self.registry.open_session(parent_id).await?;
        let inherited = if parent_row
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("job-stage:submitter:"))
        {
            let accepted = read_accepted_job(&parent_db)
                .await?
                .ok_or_else(|| anyhow::anyhow!("job parent is not accepted"))?;
            Some(accepted.definition)
        } else {
            None
        };
        let parent = Session::new(ConversationId(parent_id.to_string()), parent_db.clone()).await;
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
        let parent_host = self
            .agent_index
            .find_by_name(parent_name)
            .ok_or_else(|| anyhow::anyhow!("invalid published job: parent Agent not hosted"))?;
        anyhow::ensure!(
            meta.agents[0].db_id == parent_host.db_id.to_string()
                && meta.agents[0].home_pubkey.as_deref()
                    == Some(parent_host.pubkey.to_string().as_str()),
            "invalid published job: parent Agent is not home on executor"
        );
        let strict_capabilities = strict_session_capabilities(&parent_db).await?;
        anyhow::ensure!(
            meta.capabilities == strict_capabilities
                && inherited.as_ref().map_or(
                    strict_capabilities == Grants::default(),
                    |authority| strict_capabilities == authority.capability_ceiling
                        && authority.target == *parent_name
                ),
            "narrow or malformed parent scope is not supported by local broad jobs"
        );
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
            if name == "spawn_worker"
                || inherited
                    .as_ref()
                    .is_some_and(|authority| !authority.tool_ceilings.contains_key(&name))
            {
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
                .attenuate(parent_agent.grants.get(&name))
                .attenuate(
                    inherited
                        .as_ref()
                        .and_then(|authority| authority.tool_ceilings.get(&name)),
                );
            tool_ceilings.insert(name, grants);
        }
        let ceiling = strict_capabilities.attenuate(Some(&parent_agent.capabilities));
        let definition = StagedAgentDefinition {
            target: target_name.clone(),
            task: task.to_string(),
            executor_pubkey: target.pubkey.to_string(),
            call_depth: inherited
                .as_ref()
                .map_or(1, |authority| authority.call_depth + 1),
            max_call_depth: inherited
                .as_ref()
                .map_or(parent_agent.max_iterations as usize, |authority| {
                    authority.max_call_depth
                })
                .min(target_agent.max_iterations as usize),
            allowed_tools: tool_ceilings.keys().cloned().collect(),
            tool_ceilings,
            capability_ceiling: ceiling.clone(),
        };
        anyhow::ensure!(
            definition.call_depth <= definition.max_call_depth && definition.call_depth <= 128,
            "spawn depth exceeds Agent ceiling"
        );
        Ok((definition, parent_name.to_string(), target))
    }

    /// Prewritten requests are data, not execution authority. A malformed
    /// published child is rejected; unavailable parent/Agent state retries.
    async fn inspect_published_job(
        &self,
        db: &eidetica::Database,
    ) -> anyhow::Result<(JobRequest, String)> {
        let id = db.root_id().to_string();
        let label = format!("job-stage:submitter:{id}");
        let txn = self.registry.chaz_group().new_transaction().await?;
        let routing = txn
            .get_store::<DocStore>("sessions")
            .await?
            .get_all()
            .await?;
        let catalog = txn
            .get_store::<DocStore>("session_catalog")
            .await?
            .get_all()
            .await?;
        let route: Option<String> = routing.get(&id).map(|value| value.try_into()).transpose()?;
        let catalog_json: Option<String> =
            catalog.get(&id).map(|value| value.try_into()).transpose()?;
        let entry: Option<crate::session::SessionCatalogEntry> = catalog_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| anyhow::anyhow!("invalid published job: catalog: {error}"))?;
        anyhow::ensure!(
            route.as_deref() == Some(label.as_str())
                && entry.is_some_and(|row| row.session_db_id == id
                    && row.source.as_deref() == Some(label.as_str())
                    && row.status == crate::session::SessionStatus::Active),
            "invalid published job: catalog differs"
        );
        let txn = db.new_transaction().await?;
        let marker = txn.get_store::<DocStore>("meta").await?.get_all().await?;
        let kind: Option<String> = marker.get("kind").map(|v| v.try_into()).transpose()?;
        let name: Option<String> = marker
            .get("display_name")
            .map(|v| v.try_into())
            .transpose()?;
        anyhow::ensure!(
            kind.as_deref() == Some(crate::db_kind::KIND_SESSION)
                && name.as_deref() == Some(label.as_str()),
            "invalid published job: marker differs"
        );
        let request = crate::session::SessionRegistry::read_job_request(db).await?;
        anyhow::ensure!(
            !request.parent_id.is_empty()
                && !request.target.is_empty()
                && !request.task.is_empty()
                && request.parent_id != id,
            "invalid published job: empty request or self-parent"
        );
        let root = ID::parse(&request.parent_id)
            .map_err(|error| anyhow::anyhow!("invalid published job: parent ID: {error}"))?;
        let settings = db
            .get_settings()
            .await?
            .get_auth_doc_for_validation()
            .await?;
        let key = format!("delegations.{root}");
        let delegation = match settings.get(&key) {
            Some(eidetica::crdt::doc::Value::Doc(doc)) => {
                eidetica::auth::types::DelegatedTreeRef::try_from(doc).map_err(|error| {
                    anyhow::anyhow!("invalid published job: delegation: {error}")
                })?
            }
            _ => anyhow::bail!("invalid published job: no parent delegation"),
        };
        anyhow::ensure!(
            delegation.tree.root == root
                && delegation.permission_bounds.max == eidetica::auth::types::Permission::Admin(0)
                && delegation.permission_bounds.min.is_none(),
            "invalid published job: parent delegation differs"
        );
        let child = Session::new(ConversationId(id), db.clone()).await;
        let mut entries = child.entries_with_ids();
        let (directive_id, first) = entries
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid published job: missing Directive"))?;
        let remaining = entries.collect::<Vec<_>>();
        anyhow::ensure!(
            first.entry_type == EntryType::Directive
                && first.content == request.task
                && first.sender == "submitter"
                && (read_accepted_job(db).await?.is_some() || remaining.is_empty())
                && !remaining
                    .iter()
                    .any(|(_, entry)| entry.entry_type == EntryType::Directive
                        || (entry.entry_type == EntryType::Message
                            && entry.sender != request.target)),
            "invalid published job: altered or multiple Directives"
        );
        Ok((request, directive_id.as_str().to_string()))
    }

    /// Complete the parent's admission check before deriving a nested child's
    /// authority, rather than nesting two large validators on one poll stack.
    async fn validate_job_parent(&self, parent_id: &str) -> anyhow::Result<()> {
        let (_, parent_db) = self.registry.open_session(parent_id).await?;
        let parent_is_job = self.registry.list_sessions().await?.iter().any(|row| {
            row.session_db_id == parent_id
                && row
                    .source
                    .as_deref()
                    .is_some_and(|source| source.starts_with("job-stage:"))
        });
        if parent_is_job {
            anyhow::ensure!(
                self.validate_accepted_job(&parent_db).await?.is_some(),
                "job parent is not accepted"
            );
        }
        Ok(())
    }

    async fn accept_published_job(&self, db: &eidetica::Database) -> anyhow::Result<()> {
        anyhow::ensure!(self.executor_authorized, "client cannot admit agent jobs");
        let (request, directive_id) = self.inspect_published_job(db).await?;
        self.validate_job_parent(&request.parent_id).await?;
        let (definition, _, target) = self
            .derive_job_definition(&request.parent_id, &request.target, &request.task)
            .await
            .map_err(|error| {
                if error
                    .to_string()
                    .contains("narrow or malformed parent scope")
                    || error.to_string().contains("spawn depth exceeds")
                    || error
                        .to_string()
                        .contains("target Agent not hosted on executor")
                {
                    anyhow::anyhow!("invalid published job: {error}")
                } else {
                    error
                }
            })?;
        let id = db.root_id().to_string();
        if let Some(existing) = read_accepted_job(db).await? {
            anyhow::ensure!(
                existing.parent_id == request.parent_id
                    && existing.directive_id == directive_id
                    && existing.definition == definition,
                "invalid published job: acceptance differs"
            );
            return Ok(());
        }
        let meta = Session::new(ConversationId(id.clone()), db.clone())
            .await
            .read_meta()
            .await;
        let agents = strict_session_agents(db)
            .await
            .map_err(|error| anyhow::anyhow!("invalid published job: agents: {error}"))?;
        anyhow::ensure!(
            meta.agents == agents
                && meta
                    .host_agent_db_id
                    .as_deref()
                    .is_none_or(|home| home == target.db_id.to_string()),
            "invalid published job: malformed Agent/home scope"
        );
        anyhow::ensure!(
            (meta.agents.is_empty()
                || (meta.agents.len() == 1
                    && meta.agents[0].db_id == target.db_id.to_string()
                    && meta.agents[0].display_name == target.display_name
                    && meta.agents[0].home_pubkey.as_deref()
                        == Some(target.pubkey.to_string().as_str())))
                && (strict_session_capabilities(db).await? == Grants::default()
                    || strict_session_capabilities(db).await? == definition.capability_ceiling),
            "invalid published job: includes untrusted execution scope"
        );
        if meta.agents.is_empty() {
            self.registry.attach_agent_to_session(&id, &target).await?;
        }
        Session::new(ConversationId(id.clone()), db.clone())
            .await
            .update_meta(|meta| meta.capabilities = definition.capability_ceiling.clone())
            .await?;
        anyhow::ensure!(
            self.peer_is_home_for(&id, &target.display_name).await,
            "target Agent is hosted on another peer"
        );
        let accepted = AcceptedAgentJob {
            parent_id: request.parent_id,
            definition,
            directive_id,
        };
        let txn = db.new_transaction().await?;
        txn.get_store::<DocStore>("job_acceptance")
            .await?
            .set_string("v1", serde_json::to_string(&accepted)?)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    /// Install a restartable child runtime only after child-scope revalidation.
    pub(super) async fn register_accepted_job(
        &self,
        child_db: &eidetica::Database,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(self.executor_authorized, "client cannot adopt agent jobs");
        if crate::session::jobs::read_job_rejection(child_db)
            .await?
            .is_some()
        {
            return Ok(true);
        }
        // Only a published submitter child can be adopted; legacy staged rows
        // must not be treated as runnable.
        let submitter_row = crate::db_kind::read_marker(child_db)
            .await
            .is_some_and(|(_, marker)| marker.starts_with("job-stage:submitter:"));
        if !submitter_row {
            return Ok(false);
        }
        if read_accepted_job(child_db).await?.is_none()
            && let Err(error) = self.accept_published_job(child_db).await
        {
            if error.to_string().starts_with("invalid published job:") {
                crate::session::jobs::reject_job(child_db, &error.to_string()).await?;
                return Ok(true);
            }
            return Err(error);
        }
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

    /// Walk from child to root instead of recursively polling large service
    /// futures: even a two-level job chain exceeds a normal test-thread stack.
    async fn validate_published_chain(
        &self,
        child_db: &eidetica::Database,
    ) -> anyhow::Result<AcceptedAgentJob> {
        let original = read_accepted_job(child_db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("job has no acceptance"))?;
        let mut current = child_db.clone();
        for _ in 0..128 {
            let accepted = read_accepted_job(&current)
                .await?
                .ok_or_else(|| anyhow::anyhow!("job ancestor has no acceptance"))?;
            anyhow::ensure!(
                (1..=128).contains(&accepted.definition.call_depth)
                    && accepted.definition.call_depth <= accepted.definition.max_call_depth,
                "accepted job has invalid depth"
            );
            let (request, directive_id) = self.inspect_published_job(&current).await?;
            anyhow::ensure!(
                request.parent_id == accepted.parent_id && directive_id == accepted.directive_id,
                "accepted job request differs"
            );
            let (_, parent_db) = self.registry.open_session(&request.parent_id).await?;
            let parent_row = self
                .registry
                .list_sessions()
                .await?
                .into_iter()
                .find(|row| row.session_db_id == request.parent_id)
                .ok_or_else(|| anyhow::anyhow!("job ancestor parent is not indexed"))?;
            if parent_row
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("job-stage:submitter:"))
            {
                let parent = read_accepted_job(&parent_db)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("job ancestor parent is not accepted"))?;
                anyhow::ensure!(
                    parent.definition.call_depth.checked_add(1)
                        == Some(accepted.definition.call_depth),
                    "accepted job parent chain has invalid depth"
                );
            }
            let (expected, _, target) = self
                .derive_job_definition(&request.parent_id, &request.target, &request.task)
                .await?;
            anyhow::ensure!(
                accepted.definition == expected,
                "accepted job authority differs from parent"
            );
            let meta = Session::new(
                ConversationId(current.root_id().to_string()),
                current.clone(),
            )
            .await
            .read_meta()
            .await;
            anyhow::ensure!(
                meta.agents == strict_session_agents(&current).await?
                    && meta.agents.len() == 1
                    && meta.agents[0].db_id == target.db_id.to_string()
                    && meta.agents[0].display_name == target.display_name
                    && meta
                        .host_agent_db_id
                        .as_deref()
                        .is_none_or(|home| home == target.db_id.to_string())
                    && meta.agents[0].home_pubkey.as_deref()
                        == Some(target.pubkey.to_string().as_str())
                    && strict_session_capabilities(&current).await? == expected.capability_ceiling,
                "accepted job Agent/home or scope differs"
            );
            if !parent_row
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("job-stage:submitter:"))
            {
                return Ok(original);
            }
            current = parent_db;
        }
        anyhow::bail!("accepted job parent chain exceeds depth limit")
    }

    /// A marker or Directive alone cannot authorize local execution.
    pub(super) async fn validate_accepted_job(
        &self,
        child_db: &eidetica::Database,
    ) -> anyhow::Result<Option<AcceptedAgentJob>> {
        if read_accepted_job(child_db).await?.is_none() {
            return Ok(None);
        }
        Ok(Some(self.validate_published_chain(child_db).await?))
    }
}

async fn strict_session_agents(
    db: &eidetica::Database,
) -> anyhow::Result<Vec<crate::session::AgentRef>> {
    let txn = db.new_transaction().await?;
    let doc = txn.get_store::<DocStore>("meta").await?.get_all().await?;
    match doc.get("agents") {
        None => Ok(Vec::new()),
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(serde_json::from_str(&json)?)
        }
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
    /// Publish a delegated child to the watched catalog without executing it.
    /// An uncertain publication is not safe to retry: inspect the catalog first.
    pub async fn submit_agent_job(
        &self,
        parent_id: &str,
        agent_ref: &str,
        task: &str,
    ) -> anyhow::Result<String> {
        let prepared = self
            .registry
            .prepare_agent_job(parent_id, agent_ref, task)
            .await?;
        self.registry
            .publish_agent_job(prepared)
            .await
            .map(|id| id.0)
            .map_err(|error| {
                anyhow::anyhow!(
                    "job publication outcome uncertain ({error}); inspect the existing session catalog before submitting again"
                )
            })
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
