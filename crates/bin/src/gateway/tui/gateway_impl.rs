//! `Gateway` trait implementation for the TUI. Extracted from `mod.rs`.
//!
//! Child module of `tui` so the impl retains access to `TuiGateway`/`App`
//! private state and the module-level render/terminal helpers via
//! `use super::*`.

use super::*;

impl Gateway for TuiGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let (approval_tx, mut approval_rx) = mpsc::channel::<TaggedApproval>(8);
        let (notify_tx, mut notify_rx) = mpsc::channel::<String>(64);
        // One-shot-style delivery of background model catalog fetches.
        // Buffered so a force-refresh kicked off mid-render doesn't block.
        let (models_tx, mut models_rx) = mpsc::channel::<Result<Vec<ModelInfo>, String>>(4);

        let (_conv_id, session_db) = default_tui_session(&server).await?;
        let session_db_id = session_db.root_id().to_string();

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        setup_session(
            &server,
            &session_db,
            backend.clone(),
            approval_tx.clone(),
            notify_tx.clone(),
        )
        .await?;

        let agent_names: HashSet<String> = server
            .agents()
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        let initial_tab = build_tab(&server, &backend, session_db, session_db_id).await;
        let mut app = App::new(agent_names, initial_tab);
        if let Some(prompt) = self.initial_prompt.as_ref() {
            app.input = prompt.clone();
            app.cursor = app.input.len();
        }

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            original_hook(info);
        }));

        let mut terminal = init_terminal()?;
        let mut events = EventStream::new();

        // When prior sessions exist, open straight into the picker so the
        // user picks one (or the New session row) instead of always landing
        // in the default session. A fresh install — only the just-created
        // empty default session — still goes directly to chat. Also
        // skipped when an initial prompt was supplied — the user already
        // signalled "I want to send something now," not "show me sessions."
        if self.initial_prompt.is_none() {
            let (sid, sdb, agent, sname) = {
                let t = app.active();
                (
                    t.session_db_id.clone(),
                    t.session_db.clone(),
                    t.current_agent.clone(),
                    t.session_name.clone(),
                )
            };
            let ctx = CommandContext {
                server: &server,
                secrets: &self.secrets,
                backend: &backend,
                session_db_id: &sid,
                session_db: &sdb,
                current_agent: &agent,
                session_name: sname.as_deref(),
            };
            if let CommandOutcome::SessionsList(list) =
                commands::dispatch(Command::ListSessions, &ctx).await
            {
                let has_known = list.len() > 1 || list.iter().any(|s| s.entry_count > 0);
                if has_known {
                    app.session_list = list;
                    app.session_list_fresh = true;
                    // Always land on the "New session" row when the picker
                    // first opens.
                    app.picker_index = 0;
                    app.mode = TuiMode::SessionPicker;
                }
            }
        }

        loop {
            terminal.draw(|f| view::ui(f, &mut app, &server, &backend, &self.config))?;

            let action = tokio::select! {
                Some(Ok(event)) = events.next() => {
                    match event {
                        Event::Key(key) => Action::Key(key),
                        Event::Mouse(m) => Action::Mouse(m),
                        _ => continue,
                    }
                }
                Some(id) = notify_rx.recv() => Action::SessionChanged(id),
                Some(msg) = approval_rx.recv() => Action::ApprovalRequest(msg),
                Some(res) = models_rx.recv() => Action::ModelsFetched(res),
            };

            match action {
                Action::Key(key) => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.should_quit = true;
                    } else if key.code == KeyCode::Char('d')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.debug_mode = !app.debug_mode;
                    } else if key.code == KeyCode::Char('t')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.expand_all = !app.expand_all;
                    } else if key.code == KeyCode::Char('w')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        close_active_tab(&mut app);
                    } else if key.code == KeyCode::Char(',')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+, opens Settings, picking scope from the
                        // current mode. Chat → Session (routed through
                        // ChatAction so the meta snapshot gets seeded);
                        // picker → Peer (no snapshot needed). Already in
                        // Settings or the model picker? No-op — Esc exits.
                        match app.mode {
                            TuiMode::Chat => {
                                handle_chat_action(
                                    ChatAction::OpenSettings(SettingsScope::Session),
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                    &models_tx,
                                )
                                .await;
                            }
                            TuiMode::SessionPicker => {
                                app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
                            }
                            TuiMode::ModelPicker | TuiMode::Settings(_) => {}
                        }
                    } else if key.code == KeyCode::Char('p')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+P toggles the session picker. In chat mode it
                        // opens it; in picker mode it dismisses back to chat.
                        match app.mode {
                            TuiMode::Chat => {
                                handle_chat_action(
                                    ChatAction::OpenPicker,
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                    &models_tx,
                                )
                                .await;
                            }
                            TuiMode::SessionPicker => {
                                app.mode = TuiMode::Chat;
                            }
                            TuiMode::ModelPicker => {
                                app.mode = app.model_picker_caller;
                            }
                            // Settings users get out via Esc; Ctrl+P is a
                            // no-op here so it doesn't compete with the
                            // category navigation flow.
                            TuiMode::Settings(_) => {}
                        }
                    } else if key.code == KeyCode::PageUp
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, -1);
                    } else if key.code == KeyCode::PageDown
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, 1);
                    } else {
                        match input::handle_overlay_key(&mut app, key) {
                            input::OverlayKey::Consumed => continue,
                            input::OverlayKey::RenameSubmit {
                                session_db_id,
                                name,
                            } => {
                                apply_picker_rename(
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    session_db_id,
                                    name,
                                )
                                .await;
                                continue;
                            }
                            input::OverlayKey::NotConsumed => {}
                        }
                        match app.mode {
                            TuiMode::Chat => {
                                if let Some(chat_action) =
                                    input::handle_chat_key(&mut app, key).await
                                {
                                    handle_chat_action(
                                        chat_action,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        &approval_tx,
                                        &notify_tx,
                                        &models_tx,
                                    )
                                    .await;
                                }
                            }
                            TuiMode::SessionPicker => {
                                if let Some(selected) = input::handle_picker_key(&mut app, key) {
                                    dispatch_picker_selection(
                                        selected,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        &approval_tx,
                                        &notify_tx,
                                    )
                                    .await;
                                }
                            }
                            TuiMode::ModelPicker => {
                                match input::handle_model_picker_key(&mut app, key) {
                                    input::ModelPickerKey::Select(model_id) => {
                                        dispatch_model_selection(
                                            model_id,
                                            &mut app,
                                            &server,
                                            &backend,
                                            &self.secrets,
                                            &approval_tx,
                                            &notify_tx,
                                        )
                                        .await;
                                    }
                                    input::ModelPickerKey::Refresh => {
                                        // Force a live re-pull: drop the
                                        // in-memory session catalog so the
                                        // fetch can't short-circuit.
                                        app.session_catalog = None;
                                        spawn_catalog_load(
                                            &mut app,
                                            backend.clone(),
                                            models_tx.clone(),
                                        );
                                    }
                                    input::ModelPickerKey::None => {}
                                }
                            }
                            TuiMode::Settings(scope) => {
                                let outcome = input::handle_settings_key(&mut app, key, scope);
                                handle_settings_outcome(
                                    outcome,
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                    &models_tx,
                                )
                                .await;
                            }
                        }
                    }
                }
                Action::Mouse(m) => {
                    if let Some(outcome) = input::handle_mouse(&mut app, m) {
                        match outcome {
                            input::MouseOutcome::PickerOpenSelected => {
                                let selected = app.picker_selection();
                                dispatch_picker_selection(
                                    selected,
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                )
                                .await;
                            }
                            input::MouseOutcome::TabActivate(i) => {
                                if i < app.tabs.len() {
                                    app.active_tab = i;
                                }
                            }
                            input::MouseOutcome::TabClose(i) => {
                                close_tab_at(&mut app, i);
                            }
                            input::MouseOutcome::ModelPickerOpenSelected => {
                                if let Some(model_id) = app.model_picker_selection() {
                                    dispatch_model_selection(
                                        model_id,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        &approval_tx,
                                        &notify_tx,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                Action::SessionChanged(id) => {
                    if let Some(idx) = app.tab_index_for(&id) {
                        let (db_id, db) = {
                            let tab = &app.tabs[idx];
                            (tab.session_db_id.clone(), tab.session_db.clone())
                        };
                        let session =
                            Session::new(chaz_core::types::ConversationId(db_id.clone()), db).await;
                        let entries = session.entries().to_vec();
                        let meta = session.read_meta().await;

                        // Decide waiting state from the fresh entries before
                        // moving them into the tab.
                        let clear_waiting = entries.last().is_some_and(|latest| {
                            app.agent_names.contains(&latest.sender)
                                && latest.entry_type == EntryType::Message
                        });

                        // Refresh effective_model from the fresh meta: if
                        // `/model X` or `/model <agent> Y` ran on this
                        // session (or a remote peer pinned a model), the
                        // resolved value moves. Per-agent override beats
                        // the session pin for the tab's current agent.
                        let current_agent = app.tabs.get(idx).map(|t| t.current_agent.clone());
                        let agent_default = current_agent
                            .as_deref()
                            .and_then(|name| server.agents().get(name))
                            .and_then(|a| a.default_model.clone());
                        let session_model = current_agent
                            .as_deref()
                            .and_then(|name| meta.resolve_model_for_agent(name))
                            .map(str::to_string);
                        let effective_model = backend.resolve_model_name(
                            session_model.as_deref().or(agent_default.as_deref()),
                        );
                        // Re-resolve the budget too: a `/model` change can move
                        // the effective model to one with a different window.
                        let agent_cap = current_agent
                            .as_deref()
                            .and_then(|name| server.agents().get(name))
                            .and_then(|a| a.max_context_tokens);
                        let context_budget =
                            server.effective_context_budget(&effective_model, agent_cap);
                        // Refresh the full roster too: attach/detach, host
                        // changes, and per-agent model pins all move here.
                        let roster = build_roster(&server, &backend, &meta);

                        let tab = &mut app.tabs[idx];
                        tab.entries = entries;
                        tab.session_name = meta.name.clone();
                        tab.effective_model = effective_model;
                        tab.context_budget = context_budget;
                        tab.roster = roster;
                        if clear_waiting {
                            tab.waiting = false;
                        }

                        // If Settings(Session) is up on the same tab,
                        // refresh the snapshot so meta edits (model pin,
                        // agent attach/detach) propagate immediately.
                        if matches!(app.mode, TuiMode::Settings(SettingsScope::Session))
                            && app
                                .session_settings_snapshot
                                .as_ref()
                                .is_some_and(|s| s.session_db_id == db_id)
                        {
                            seed_session_settings_snapshot(&mut app, &server).await;
                        }

                        // Keep the picker cache in lock-step with this tab's
                        // entries so the next picker open doesn't show stale
                        // counts / cost / name.
                        if let Some(row) = app
                            .session_list
                            .iter_mut()
                            .find(|s| s.session_db_id == db_id)
                        {
                            let entries_ref = &app.tabs[idx].entries;
                            row.entry_count = entries_ref.len();
                            row.name = meta.name.clone();
                            row.agent_name = meta.agent_name.clone();
                            row.last_message =
                                chaz_core::session::summarize_last_message(entries_ref);
                            let (cost, reported, calls) =
                                chaz_core::session::sum_session_cost(entries_ref);
                            row.total_cost_usd = cost;
                            row.cost_reported = reported;
                            row.llm_call_count = calls;
                        }
                    }
                }
                Action::ApprovalRequest((id, exchange)) => {
                    if let Some(idx) = app.tab_index_for(&id) {
                        app.tabs[idx].pending_approval = Some(exchange);
                    } else {
                        // Tab was closed but an approval snuck through — deny
                        // so the runtime doesn't hang waiting.
                        let _ = exchange
                            .decision_tx
                            .send(chaz_core::gateway::ApprovalDecision::Deny);
                    }
                }
                Action::ModelsFetched(res) => {
                    app.model_picker_loading = false;
                    match res {
                        Ok(catalog) => {
                            app.model_picker_error = None;
                            // Hold the pulled catalog in memory so reopening the
                            // picker this session is instant (no re-fetch).
                            app.session_catalog = Some(catalog.clone());
                            app.rebuild_model_list(catalog);
                        }
                        Err(msg) => {
                            app.model_picker_error = Some(msg);
                        }
                    }
                }
            }

            if app.should_quit {
                break;
            }
        }

        restore_terminal();
        Ok(())
    }
}
