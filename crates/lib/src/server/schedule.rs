//! Agent-owned schedule fire path for [`Server`].
//!
//! Extracted from `server/mod.rs`: `fire_agent_schedule`, the agent-db
//! openers it uses, the schedule-turn runner, and the session-start hook.
//! Compiled as a child module so these `impl Server` methods keep access
//! to the server's private state.

use super::*;

impl Server {
    /// Standalone execution path for agent-owned schedule fires.
    ///
    /// This is deliberately separate from [`Self::process_session`] — it
    /// calls [`crate::runtime::execute`] directly without touching the
    /// session's `SessionRuntime` (agent_override, backend, completion
    /// channel). A Pinned schedule firing into a live session the user is
    /// chatting in does not hijack the interactive routing.
    ///
    /// Steps:
    /// 1. Host check — skip if the agent isn't on this peer.
    /// 2. Resolve target — Fresh creates a session + attaches the owner;
    ///    Pinned opens the existing session + idempotently attaches.
    /// 3. Acquire the session's `processing` lock; skip if busy.
    /// 4. Load the agent, build context with the wake-prompt as a private
    ///    System message, run the ReAct loop.
    /// 5. Write ToolCall/ToolResult entries + a terminal Message (only if
    ///    non-empty; silent turns produce no entry).
    /// 6. Record a [`crate::agent_db::ScheduleFire`] with usage on the
    ///    agent's DB.
    /// 7. One-shot cleanup: delete the Schedule row from the agent DB.
    pub async fn fire_agent_schedule(
        &self,
        payload: crate::routine::AgentSchedulePayload,
    ) -> anyhow::Result<()> {
        use crate::agent_db::{ScheduleFire, ScheduleTarget};

        // 1. Host check — unparseable IDs are silently skipped (they
        //    can't possibly be hosted on this peer).
        let owner_id = match eidetica::entry::ID::parse(&payload.owner_agent_db_id) {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!(
                    agent_db_id = %payload.owner_agent_db_id,
                    schedule = %payload.schedule_id,
                    "Unparseable owner_agent_db_id; skipping schedule fire: {e}"
                );
                return Ok(());
            }
        };
        let Some(agent_entry) = self.agent_index.find_by_id(&owner_id) else {
            tracing::debug!(
                agent_db_id = %payload.owner_agent_db_id,
                schedule = %payload.schedule_id,
                "Agent not hosted on this peer; skipping schedule fire"
            );
            return Ok(());
        };

        // 1b. Lifecycle bound check (Gap 4). The agent DB is the
        //     authoritative store; the engine's in-memory routine is
        //     rebuilt from it. If the schedule has hit its expiry or
        //     max_fires bound, persist `enabled = false` and skip —
        //     this is what actually retires a recurring schedule (the
        //     in-memory routine keeps ticking until the next
        //     reload/restart, but every tick now early-returns here).
        if let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await
            && let Ok(Some(mut schedule)) = adb.find_schedule(&payload.schedule_id).await
            && let Some(reason) = schedule.retirement_reason(Utc::now())
        {
            if schedule.enabled {
                schedule.enabled = false;
                if let Err(e) = adb.upsert_schedule(schedule).await {
                    tracing::error!(
                        agent = %agent_entry.display_name,
                        schedule = %payload.schedule_id,
                        "Failed to persist schedule retirement: {e}"
                    );
                }
            }
            tracing::info!(
                agent = %agent_entry.display_name,
                schedule = %payload.schedule_id,
                "Schedule retired ({reason}); skipping fire"
            );
            return Ok(());
        }

        // 2. Resolve target
        let target: ScheduleTarget = serde_json::from_value(payload.target)
            .map_err(|e| anyhow::anyhow!("invalid schedule target in payload: {e}"))?;

        let agent_name = agent_entry.display_name.clone();

        let (session_db, is_fresh, session_db_id) = match &target {
            ScheduleTarget::Fresh => {
                // Home-peer gate (agent-level). Fresh schedules have no
                // session yet to carry `AgentRef.home_pubkey`, so the gate
                // falls back to the agent DB's `meta.home_pubkey`. Legacy
                // `None` lets every keyholder run (pre-feature default).
                if let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await
                    && let Some(home) = crate::db_kind::read_agent_home_pubkey(adb.database()).await
                    && home != agent_entry.pubkey
                {
                    tracing::debug!(
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Not home peer for agent's Fresh schedule fire; skipping"
                    );
                    return Ok(());
                }

                let source = format!(
                    "schedule:{}:{}",
                    payload.owner_agent_db_id, payload.schedule_id
                );
                let (_conv, db) = self.registry.create_session(Some(&source)).await?;
                let sid = db.root_id().to_string();

                // Attach the owner agent to the session so it has Write
                // permission and the session meta records membership.
                self.registry
                    .attach_agent_to_session(&sid, &agent_entry)
                    .await?;

                // Register with the server so the session has a
                // SessionRuntime (backend + on_write callback) for any
                // tools that need it (spawn_agent writes to the on_write
                // path). The agent_override is set to the schedule owner so
                // future interactive writes route to this agent.
                self.register_session(
                    &db,
                    self.default_backend.clone(),
                    Some(agent_name.clone()),
                    None,
                )
                .await?;

                tracing::info!(
                    session = %sid,
                    agent = %agent_name,
                    schedule = %payload.schedule_id,
                    "Created Fresh session for agent schedule fire"
                );

                (db, true, sid)
            }
            ScheduleTarget::Pinned { session_db_id } => {
                // Closed-session retirement. If the pinned session has been
                // deregistered, soft-disable the schedule rather than reopen
                // a dead DB. Mirrors the lifecycle-bound retirement pattern
                // above: the in-memory routine keeps ticking until the next
                // agent reload, but every tick now early-returns here.
                if !self.is_session_open(session_db_id).await {
                    if let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await
                        && let Ok(Some(mut schedule)) =
                            adb.find_schedule(&payload.schedule_id).await
                        && schedule.enabled
                    {
                        schedule.enabled = false;
                        if let Err(e) = adb.upsert_schedule(schedule).await {
                            tracing::error!(
                                agent = %agent_name,
                                schedule = %payload.schedule_id,
                                "Failed to persist Pinned-target retirement: {e}"
                            );
                        }
                    }
                    tracing::info!(
                        session = %session_db_id,
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Pinned target session closed; retiring schedule"
                    );
                    return Ok(());
                }

                let (_conv, db) = self.registry.open_session(session_db_id).await?;
                let sid = session_db_id.clone();

                // Home-peer gate (per-session). Pinned schedules fire into
                // an existing session, so the gate uses that session's
                // `AgentRef.home_pubkey` — same source as `process_session`.
                // If the session was rehosted to another peer, this fire
                // belongs to them.
                if !self.peer_is_home_for(&sid, &agent_name).await {
                    tracing::debug!(
                        session = %sid,
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Not home peer for pinned schedule's session; skipping"
                    );
                    self.record_home_skip(&sid, &agent_name).await;
                    return Ok(());
                }

                // Idempotent attach: if the agent was detached after the
                // schedule was created, the fire-time membership check
                // catches it and re-attaches. If attach fails (session
                // gone, auth broken), skip this fire.
                let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
                let meta = session.read_meta().await;
                let already_member = meta
                    .agents
                    .iter()
                    .any(|a| a.db_id == agent_entry.db_id.to_string());
                if !already_member {
                    if let Err(e) = self
                        .registry
                        .attach_agent_to_session(&sid, &agent_entry)
                        .await
                    {
                        tracing::warn!(
                            session = %sid,
                            agent = %agent_name,
                            schedule = %payload.schedule_id,
                            "Pinned schedule: failed to re-attach agent to session: {e}"
                        );
                        return Ok(()); // self-skip
                    }
                    tracing::info!(
                        session = %sid,
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Pinned schedule: re-attached agent to session"
                    );
                }

                (db, false, sid)
            }
        };

        // 3. Acquire the processing lock (skip if session is busy).
        //    Release at the end of this scope via the deferred block.
        {
            let mut processing = self.processing.lock().await;
            if !processing.insert(session_db_id.clone()) {
                tracing::debug!(
                    session = %session_db_id,
                    schedule = %payload.schedule_id,
                    "Session busy; skipping schedule fire"
                );
                return Ok(());
            }
        }

        // 4. Load the agent + build context + run the turn.
        let outcome = self
            .run_schedule_turn(
                &agent_name,
                &session_db,
                &session_db_id,
                &payload.prompt,
                &payload.schedule_id,
            )
            .await;

        // Release the processing lock.
        {
            let mut processing = self.processing.lock().await;
            processing.remove(&session_db_id);
        }

        // 5. Record ScheduleFire on the agent's DB (best-effort audit).
        let fired_at = Utc::now();
        if let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await {
            let fire = ScheduleFire {
                schedule_id: payload.schedule_id.clone(),
                fired_at,
                session_db_id: session_db_id.clone(),
                fresh: is_fresh,
                usage: outcome.as_ref().ok().and_then(|o| o.metadata.clone()),
            };
            if let Err(e) = adb.record_schedule_fire(fire).await {
                tracing::error!(
                    agent = %agent_name,
                    schedule = %payload.schedule_id,
                    "Failed to record ScheduleFire: {e}"
                );
            }

            // Lifecycle accounting (Gap 4): a recurring schedule that
            // ran its turn increments `fire_count`. When that reaches
            // `max_fires`, retire it now (persist enabled=false) so the
            // next tick's pre-check — and any reload — drops it. One-shot
            // schedules are deleted below, so they don't count.
            if !payload.one_shot
                && outcome.is_ok()
                && let Ok(Some(mut schedule)) = adb.find_schedule(&payload.schedule_id).await
            {
                schedule.fire_count = schedule.fire_count.saturating_add(1);
                if let Some(reason) = schedule.retirement_reason(fired_at) {
                    schedule.enabled = false;
                    tracing::info!(
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        fire_count = schedule.fire_count,
                        "Schedule retired after fire ({reason})"
                    );
                }
                if let Err(e) = adb.upsert_schedule(schedule).await {
                    tracing::error!(
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Failed to persist schedule fire_count: {e}"
                    );
                }
            }
        }

        // 6. One-shot cleanup: delete the Schedule row after a successful fire.
        if payload.one_shot
            && let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await
        {
            if let Err(e) = adb.remove_schedule(&payload.schedule_id).await {
                tracing::error!(
                    agent = %agent_name,
                    schedule = %payload.schedule_id,
                    "Failed to remove one-shot schedule: {e}"
                );
            } else {
                tracing::info!(
                    agent = %agent_name,
                    schedule = %payload.schedule_id,
                    "Removed one-shot schedule after successful fire"
                );
            }
        }

        // Surface any runtime error
        outcome.map(|_| ())
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
                tracing::warn!(agent = %entry.display_name, "No key for agent DB; can't write fire audit");
                None
            }
            Err(e) => {
                tracing::error!(agent = %entry.display_name, "Failed to open agent DB: {e}");
                None
            }
        }
    }

    /// Load the agent, hydrate from DB, build context with the wake-prompt
    /// as a private System message, run the ReAct loop, and write entries.
    ///
    /// Returns the [`crate::runtime::RuntimeOutcome`] so the caller can
    /// extract usage metadata for cost attribution.
    async fn run_schedule_turn(
        &self,
        agent_name: &str,
        session_db: &eidetica::Database,
        session_db_id: &str,
        wake_prompt: &str,
        schedule_id: &str,
    ) -> anyhow::Result<crate::runtime::RuntimeOutcome> {
        // Load the agent: check the in-memory registry first; build from
        // DB config if not present (agent was never attached to a session
        // this boot).
        let mut agent = match self.agents.get(agent_name) {
            Some(a) => a,
            None => {
                // Build a minimal agent from the DB config.
                // hydrate_agent_from_db below will fill in the rest.
                self.agents
                    .build_from_db_config(agent_name, &crate::agent_db::AgentDbConfig::default())
            }
        };
        // Hydrate from the Living Agent DB (live refresh).
        agent = self.hydrate_agent_from_db(agent).await;

        let default_model = agent.default_model.clone();
        let allowed_tools = agent.allowed_tools.clone();
        let agent_grants = agent.grants.clone();
        let max_call_depth = agent.max_iterations as usize;
        let max_context_tokens = agent.max_context_tokens;
        let profile = agent
            .tool_profile
            .as_ref()
            .and_then(|name| self.tool_profiles.get(name))
            .cloned()
            .unwrap_or_default();

        let active_extensions = self
            .active_extensions_for_agent(session_db_id, agent_name)
            .await;
        let scoped_tools = ScopedTools::new(self.tools.clone(), allowed_tools)
            .with_active_extensions(Some(active_extensions.clone()));

        // Build the session view + context
        let session = Session::new(
            ConversationId(session_db_id.to_string()),
            session_db.clone(),
        )
        .await;
        let session = Arc::new(tokio::sync::Mutex::new(session));

        let tool_ctx = ToolContext {
            agent_name: agent_name.to_string(),
            call_depth: 0,
            max_call_depth,
            tools: scoped_tools,
            profile,
            session: session.clone(),
            grants: Default::default(),
            agent_grants,
            host: self.host.clone(),
            active_extensions: active_extensions.clone(),
            iteration_budget: Some(std::sync::Arc::new(std::sync::atomic::AtomicU32::new(
                agent.max_iterations,
            ))),
            routine_engine: self.routine_engine().cloned(),
        };

        let tool_defs = tool_ctx.tools.definitions(&tool_ctx.profile);
        let (session_model, mut assembled) = {
            let s = session.lock().await;
            let meta = s.read_meta().await;
            let roster: Vec<String> = meta.agents.iter().map(|a| a.display_name.clone()).collect();
            // Per-agent override > session pin > backend default. The backend-
            // default fallback mirrors the actual call, so an agent that pins no
            // model still budgets against its real window.
            let session_model = meta.resolve_model_for_agent(agent_name).map(str::to_string);
            let budget_model = budget_model_id(
                &self.default_backend,
                session_model.as_deref(),
                default_model.as_deref(),
            );
            // First use of a model we don't have a window for yet: learn it in
            // the background so the next turn budgets window-aware.
            if let Some(m) = budget_model.as_deref() {
                self.ensure_model_window_cached(&self.default_backend, m);
            }
            let max_tokens_override = resolve_context_max_tokens(
                &self.default_backend,
                budget_model.as_deref(),
                max_context_tokens,
            );
            let assembled = ContextBuilder::new(
                s.entries(),
                agent_name,
                &agent.system_prompt,
                &self.context_config,
            )
            .with_tools(&tool_defs)
            .with_max_tokens_override(max_tokens_override)
            .with_room_participants(&roster)
            .with_extension_hub(self.extensions.clone())
            .with_session_db(session_db)
            .build()
            .await;
            (session_model, assembled)
        };
        // Effective model resolution: per-agent override (new) > session pin
        // (`/model X`, `SessionMeta.model`) > agent's configured default.
        // `runtime::execute` then calls `BackendManager::resolve_model_name`
        // to strip the backend prefix and fall back to the backend default
        // when all three are None.
        let effective_model = session_model.or(default_model);

        // Prepend the wake-prompt as a private System message. This is
        // invocation-scoped — it never appears as a session entry.
        assembled.messages.insert(
            0,
            crate::runtime::RuntimeMessage::System(wake_prompt.to_string()),
        );

        if assembled.truncated {
            tracing::info!(
                agent = %agent_name,
                schedule = %schedule_id,
                "Context truncated: {} entries, ~{} tokens",
                assembled.entries_included,
                assembled.estimated_tokens
            );
        }

        // Event writer: capture ToolCall/ToolResult as session entries.
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::channel::<crate::runtime::RuntimeEvent>(64);
        let event_session = session.clone();
        let event_agent = agent_name.to_string();
        let event_writer = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let mut s = event_session.lock().await;
                match event {
                    crate::runtime::RuntimeEvent::ToolCall {
                        name, arguments, ..
                    } => {
                        s.add_entry(SessionEntry {
                            sender: event_agent.clone(),
                            content: format!("{name}({arguments})"),
                            timestamp: Utc::now(),
                            entry_type: EntryType::ToolCall,
                            metadata: None,
                        })
                        .await;
                    }
                    crate::runtime::RuntimeEvent::ToolResult {
                        name,
                        output,
                        is_error,
                        ..
                    } => {
                        let content = if is_error {
                            format!("{name}: ERROR: {output}")
                        } else {
                            let t = crate::util::truncate_chars(&output, 500);
                            let truncated = if t.len() < output.len() {
                                format!("{t}…")
                            } else {
                                output
                            };
                            format!("{name}: {truncated}")
                        };
                        s.add_entry(SessionEntry {
                            sender: event_agent.clone(),
                            content,
                            timestamp: Utc::now(),
                            entry_type: EntryType::ToolResult,
                            metadata: None,
                        })
                        .await;
                    }
                }
            }
        });

        let request_security = SecurityContext {
            leak_detector: self.security.leak_detector.clone(),
            auto_approved_tools: self.security.auto_approved_tools.clone(),
            approval_callback: None, // no interactive approval for schedule fires
        };

        let result = crate::runtime::execute(
            effective_model.as_deref(),
            assembled.messages,
            &self.default_backend,
            &request_security,
            &tool_ctx,
            &self.policies,
            Some(event_tx),
            Some(self.extensions.as_ref()),
        )
        .await;

        let _ = event_writer.await;

        // Write the terminal Message (conditional — skip empty).
        let mut s = session.lock().await;
        match &result {
            Ok(outcome) if outcome.body.trim().is_empty() => {
                tracing::debug!(
                    agent = %agent_name,
                    schedule = %schedule_id,
                    "Silent schedule turn — no Message written"
                );
            }
            Ok(outcome) => {
                s.add_entry(SessionEntry {
                    sender: agent_name.to_string(),
                    content: outcome.body.clone(),
                    timestamp: Utc::now(),
                    entry_type: EntryType::Message,
                    metadata: outcome.metadata.clone(),
                })
                .await;
            }
            Err(err) => {
                s.add_entry(SessionEntry {
                    sender: agent_name.to_string(),
                    content: format!("Error: {err}"),
                    timestamp: Utc::now(),
                    entry_type: EntryType::Error,
                    metadata: None,
                })
                .await;
            }
        }

        result.map_err(|e| anyhow::anyhow!("{e}"))
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
    }
}
