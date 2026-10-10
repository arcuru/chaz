//! `Bridge` trait implementation for the TUI. Extracted from `mod.rs`.
//!
//! Child module of `tui` so the impl retains access to `TuiBridge`/`App`
//! private state and the module-level render/terminal helpers via
//! `use super::*`.

use super::*;

impl Bridge for TuiBridge {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let (approval_tx, mut approval_rx) = mpsc::channel::<TaggedApproval>(8);
        let (notify_tx, mut notify_rx) = mpsc::channel::<String>(64);
        // One-shot-style delivery of background model catalog fetches.
        // Buffered so a force-refresh kicked off mid-render doesn't block.
        let (models_tx, mut models_rx) = mpsc::channel::<Result<Vec<ModelInfo>, String>>(4);
        let (session_rows_tx, mut session_rows_rx) = mpsc::channel::<SessionLoad>(128);
        let (jobs_tx, mut jobs_rx) = mpsc::channel(1);

        // Resolve any directly opened conversation before touching the
        // terminal, so a bad `--session` surfaces as a plain error.
        let launch = launch_session(
            &server,
            self.session.as_deref(),
            self.initial_prompt.is_some(),
        )
        .await?;

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        let agent_names: HashSet<String> = server
            .agents()
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        let mut app = App::hub(agent_names);
        if let Some(session_db) = launch {
            setup_session(
                &server,
                &session_db,
                backend.clone(),
                approval_tx.clone(),
                notify_tx.clone(),
            )
            .await?;
            let session_db_id = session_db.root_id().to_string();
            app.push_tab(build_tab(&server, &backend, session_db, session_db_id).await);
            app.mode = TuiMode::Chat;
            // Prefill, never send: the user reviews the prompt first.
            if let Some(prompt) = self.initial_prompt.as_ref() {
                app.input = prompt.clone();
                app.cursor = app.input.len();
            }
        }

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            original_hook(info);
        }));

        let mut terminal = init_terminal()?;
        let mut events = EventStream::new();

        // An ordinary launch opens the hub — even on an empty install — and
        // creates nothing. The catalog loads in the background.
        if app.tabs.is_empty() {
            open_session_picker(&mut app, &server, &session_rows_tx);
        }

        let mut activity_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            terminal.draw(|f| view::ui(f, &mut app, &server, &backend, &self.config))?;
            // Use this frame's viewport, not the previous frame's cursor or size.
            if matches!(app.mode, TuiMode::SessionPicker) {
                request_session_metadata(&mut app, &server, &session_rows_tx);
            }

            if matches!(app.mode, TuiMode::Jobs)
                && app.jobs.data.observed_at.is_none()
                && app.jobs.error.is_none()
            {
                jobs::refresh(&mut app, &server, &jobs_tx);
            }
            let action = tokio::select! {
                Some(Ok(event)) = events.next() => {
                    match event {
                        Event::Key(key) => Action::Key(key),
                        Event::Mouse(m) => Action::Mouse(m),
                        _ => continue,
                    }
                }
                Some(id) = notify_rx.recv() => Action::SessionChanged(id),
                _ = activity_tick.tick() => {
                    let observing_jobs = matches!(app.mode, TuiMode::Jobs)
                        || app.current().is_some_and(|tab| tab.job.is_some());
                    for tab in &mut app.tabs {
                        if tab.job.is_some() { jobs::refresh_tab(tab).await; }
                        else if !observing_jobs { refresh_tab_activity(tab).await?; }
                    }
                    if matches!(app.mode, TuiMode::Jobs) {
                        jobs::refresh(&mut app, &server, &jobs_tx);
                    }
                    continue;
                },
                Some(msg) = approval_rx.recv() => Action::ApprovalRequest(msg),
                Some(res) = models_rx.recv() => Action::ModelsFetched(res),
                Some(load) = session_rows_rx.recv() => Action::SessionLoad(load),
                Some(result) = jobs_rx.recv() => Action::JobsLoaded(result),
            };

            match action {
                Action::Key(key) => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.should_quit = true;
                    } else if key.code == KeyCode::Char('g')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.mode = if matches!(app.mode, TuiMode::Jobs) {
                            TuiMode::Chat
                        } else {
                            TuiMode::Jobs
                        };
                        if matches!(app.mode, TuiMode::Jobs) {
                            jobs::refresh(&mut app, &server, &jobs_tx);
                        }
                    } else if key.code == KeyCode::F(2) {
                        if app.mode == TuiMode::Jobs {
                            app.jobs.details = !app.jobs.details;
                            app.jobs.detail_scroll = 0;
                        } else if app.mode == TuiMode::Chat
                            && let Some(job) = app.active_mut().job.as_mut()
                        {
                            job.details = !job.details;
                        }
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
                        // Closes the focused conversation view only; the
                        // last one closing lands back on the hub.
                        if app.mode == TuiMode::Chat {
                            app.close_tab_at(app.active_tab);
                        }
                    } else if key.code == KeyCode::Char('s')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+S opens Settings, picking scope from the
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
                                    &session_rows_tx,
                                )
                                .await;
                            }
                            TuiMode::SessionPicker => {
                                app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
                            }
                            TuiMode::Jobs | TuiMode::ModelPicker | TuiMode::Settings(_) => {}
                        }
                    } else if key.code == KeyCode::Char('p')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+P toggles the session hub. In chat mode it
                        // opens it; in the hub it returns to the open
                        // conversation, or reloads a hub that has none (or
                        // whose catalog failed to load).
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
                                    &session_rows_tx,
                                )
                                .await;
                            }
                            TuiMode::Jobs => {
                                open_session_picker(&mut app, &server, &session_rows_tx);
                            }
                            TuiMode::SessionPicker => {
                                if app.tabs.is_empty() || app.session_picker_error.is_some() {
                                    app.session_list_fresh = false;
                                    open_session_picker(&mut app, &server, &session_rows_tx);
                                } else {
                                    app.mode = TuiMode::Chat;
                                }
                            }
                            TuiMode::ModelPicker => {
                                app.mode = app.model_picker_caller;
                            }
                            // Settings users get out via Esc; Ctrl+P is a
                            // no-op here so it doesn't compete with the
                            // category navigation flow.
                            TuiMode::Settings(_) => {}
                        }
                    } else if key.code == KeyCode::Char('h')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, -1);
                    } else if key.code == KeyCode::Char('l')
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
                                apply_picker_rename(&mut app, &server, session_db_id, name).await;
                                continue;
                            }
                            input::OverlayKey::NotConsumed => {}
                        }
                        match app.mode {
                            TuiMode::Jobs => match key.code {
                                KeyCode::Esc => app.mode = TuiMode::Chat,
                                KeyCode::Up => {
                                    let selected = app.jobs.selected.saturating_sub(1);
                                    jobs::select(&mut app.jobs, selected);
                                }
                                KeyCode::Down => {
                                    let selected = app.jobs.selected + 1;
                                    jobs::select(&mut app.jobs, selected);
                                }
                                KeyCode::PageUp => {
                                    app.jobs.detail_scroll =
                                        app.jobs.detail_scroll.saturating_sub(5)
                                }
                                KeyCode::PageDown => {
                                    app.jobs.detail_scroll =
                                        app.jobs.detail_scroll.saturating_add(5)
                                }
                                KeyCode::Home => app.jobs.detail_scroll = 0,
                                KeyCode::End => app.jobs.detail_scroll = u16::MAX,
                                KeyCode::Char('d') => {
                                    app.jobs.details = !app.jobs.details;
                                    app.jobs.detail_scroll = 0;
                                }
                                KeyCode::Char('r') => jobs::refresh(&mut app, &server, &jobs_tx),
                                KeyCode::Enter => {
                                    if let Some(node) = app.jobs.data.nodes.get(app.jobs.selected) {
                                        let id = node.session_db_id.clone();
                                        if node.state.is_some()
                                            && let Err(error) = jobs::open(
                                                &mut app, &server, &backend, &id, &notify_tx,
                                            )
                                            .await
                                        {
                                            app.jobs.error =
                                                Some(format!("Job unavailable: {error}"));
                                        }
                                    }
                                }
                                _ => {}
                            },
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
                                        &session_rows_tx,
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
                                    &session_rows_tx,
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
                                    &approval_tx,
                                    &notify_tx,
                                )
                                .await;
                            }
                            input::MouseOutcome::TabActivate(i) => {
                                app.set_active_tab(i);
                            }
                            input::MouseOutcome::TabClose(i) => {
                                app.close_tab_at(i);
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
                        if app.tabs[idx].job.is_some() {
                            jobs::refresh_tab(&mut app.tabs[idx]).await;
                            continue;
                        }
                        let (db_id, db) = {
                            let tab = &app.tabs[idx];
                            (tab.session_db_id.clone(), tab.session_db.clone())
                        };
                        let session =
                            Session::new(chaz_core::types::ConversationId(db_id.clone()), db).await;
                        let entries = session.entries().to_vec();
                        let meta = session.read_meta().await;

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
                        let requested_model = session_model.as_deref().or(agent_default.as_deref());
                        let effective_model = backend.resolve_model_name(requested_model);
                        let context_model = requested_model
                            .map(str::to_string)
                            .or_else(|| backend.default_model())
                            .unwrap_or_default();
                        // Re-resolve the budget too: a `/model` change can move
                        // the effective model to one with a different window.
                        let agent_cap = current_agent
                            .as_deref()
                            .and_then(|name| server.agents().get(name))
                            .and_then(|a| a.max_context_tokens);
                        let context_budget =
                            server.effective_context_budget(&context_model, agent_cap);
                        // Refresh the full roster too: attach/detach, host
                        // changes, and per-agent model pins all move here.
                        let roster = build_roster(&server, &backend, &meta);

                        let tab = &mut app.tabs[idx];
                        tab.entries = entries;
                        tab.session_name = meta.name.clone();
                        tab.effective_model = effective_model;
                        tab.context_budget = context_budget;
                        tab.model_pin = session_model;
                        tab.roster = roster;
                        refresh_tab_activity(tab).await?;

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

                        // Keep mutable row metadata in lock-step without
                        // deriving transcript summaries for the picker.
                        if let Some(row) = app
                            .session_list
                            .iter_mut()
                            .find(|s| s.session_db_id == db_id)
                        {
                            row.name = meta.name.clone();
                            row.agent_name = meta.agent_name.clone();
                            row.loaded = true;
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
                            .send(chaz_core::bridge::ApprovalDecision::Deny);
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
                Action::JobsLoaded(result) => jobs::apply(&mut app, result),
                Action::SessionLoad(load) => match load {
                    SessionLoad::Catalog { generation, result } => {
                        apply_session_catalog_result(&mut app, generation, result);
                    }
                    SessionLoad::Row { generation, info } => {
                        apply_session_row(&mut app, generation, info);
                    }
                },
            }

            ensure_view(&mut app, &server, &session_rows_tx);

            // The picker owns the background fill; the moment we're not in it,
            // stop opening session DBs the user isn't looking at. A re-open
            // restarts the walk for whatever rows never finished.
            if !matches!(app.mode, TuiMode::SessionPicker) {
                app.cancel_session_fill();
            }

            // Refresh the extension status strip after each event so it
            // reflects the active session's latest outputs (the daemon
            // rewrote the store on the turn just rendered, and a session
            // switch points `active()` at a different store).
            // coding: cheap local store read per event; gate on
            // SessionChanged / tab-switch if it ever shows in a profile.
            if matches!(app.mode, TuiMode::Chat) {
                if app.active().job.is_some() {
                    app.status_segments.clear();
                } else {
                    refresh_status_segments(&mut app).await;
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
