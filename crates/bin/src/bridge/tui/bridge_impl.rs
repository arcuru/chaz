//! `Bridge` trait implementation for the TUI. Extracted from `mod.rs`.
//!
//! Child module of `tui` so the impl retains access to `TuiBridge`/`App`
//! private state and the module-level render/terminal helpers via
//! `use super::*`.

use super::*;
use crossterm::event::MouseEventKind;

/// Upper bound on actions handled between two frames, so a flood of input
/// still repaints regularly.
const MAX_ACTIONS_PER_FRAME: usize = 64;

impl Bridge for TuiBridge {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let (approval_tx, mut approval_rx) = mpsc::channel::<TaggedApproval>(8);
        let (notify_tx, mut notify_rx) = mpsc::channel::<String>(64);
        // One-shot-style delivery of background model catalog fetches.
        // Buffered so a force-refresh kicked off mid-render doesn't block.
        let (models_tx, mut models_rx) = mpsc::channel::<Result<Vec<ModelInfo>, String>>(4);
        let (session_rows_tx, mut session_rows_rx) = mpsc::channel::<SessionLoad>(128);
        // Background tab reads (session reloads, status strip, live-turn
        // counts). Bounded by `tabs × RefreshKind`, so this never fills.
        let (refresh_tx, mut refresh_rx) = mpsc::channel::<refresh::Refresh>(64);

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

        // The run loop never awaits a storage read: backend-dependent state
        // is requested from `refresh` and arrives later as `Action::Refresh`,
        // while the frame shows what is already cached.
        let initial = app.current().map(|t| t.session_db_id.clone()).unwrap_or_default();
        refresh::request(
            &mut app,
            &initial,
            refresh::RefreshKind::Status,
            &refresh_tx,
        );
        let mut status_for = initial;

        let mut activity_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        let mut needs_draw = true;
        'run: loop {
            if needs_draw {
                terminal.draw(|f| view::ui(f, &mut app, &server, &backend, &self.config))?;
                // Use this frame's viewport, not the previous frame's cursor or size.
                if matches!(app.mode, TuiMode::SessionPicker) {
                    request_session_metadata(&mut app, &server, &session_rows_tx);
                }
                needs_draw = false;
            }

            // Block for one action, then drain whatever else is already
            // queued (bounded) before drawing again, so a burst of keys,
            // wheel notches or session writes costs one frame, not one each.
            // Every event is still handled, in arrival order.
            for batch_slot in 0..MAX_ACTIONS_PER_FRAME {
                let action = tokio::select! {
                    // Local input first: it is what the user is waiting to see.
                    biased;
                    Some(Ok(event)) = events.next() => {
                        match event {
                            Event::Key(key) => Action::Key(key),
                            Event::Mouse(m) if input::mouse_is_actionable(&m) => {
                                // A click maps through the last frame's regions;
                                // draw first if earlier events in this batch
                                // moved anything.
                                if needs_draw && matches!(m.kind, MouseEventKind::Down(_)) {
                                    terminal.draw(|f| view::ui(f, &mut app, &server, &backend, &self.config))?;
                                    needs_draw = false;
                                }
                                Action::Mouse(m)
                            }
                            // Motion and other unhandled mouse reports change
                            // nothing on screen; drop them before any work.
                            Event::Mouse(_) => continue,
                            Event::Resize(..) => {
                                needs_draw = true;
                                continue;
                            }
                            _ => continue,
                        }
                    }
                    Some(msg) = refresh_rx.recv() => Action::Refresh(msg),
                    Some(id) = notify_rx.recv() => Action::SessionChanged(id),
                    Some(msg) = approval_rx.recv() => Action::ApprovalRequest(msg),
                    Some(res) = models_rx.recv() => Action::ModelsFetched(res),
                    Some(load) = session_rows_rx.recv() => Action::SessionLoad(load),
                    _ = activity_tick.tick() => {
                        let ids: Vec<String> =
                            app.tabs.iter().map(|t| t.session_db_id.clone()).collect();
                        for id in ids {
                            refresh::request(&mut app, &id, refresh::RefreshKind::Activity, &refresh_tx);
                        }
                        continue;
                    },
                    // Nothing else is ready: end the batch and draw.
                    _ = std::future::ready(()), if batch_slot > 0 => break,
                };
                // Background results decide for themselves whether they changed
                // anything visible; every other action can.
                if !matches!(action, Action::Refresh(_) | Action::SessionChanged(_)) {
                    needs_draw = true;
                }

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
                            if app.mode == TuiMode::Chat { app.close_tab_at(app.active_tab); }
                        } else if key.code == KeyCode::Char('s')
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
                                        &session_rows_tx,
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
                                        &session_rows_tx,
                                    )
                                    .await;
                                }
                                TuiMode::SessionPicker => {
                                    if app.tabs.is_empty() || app.session_picker_error.is_some() {
                                        app.session_list_fresh = false;
                                        open_session_picker(&mut app, &server, &session_rows_tx);
                                    } else { app.mode = TuiMode::Chat; }
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
                                    apply_picker_rename(&mut app, &server, session_db_id, name)
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
                                            &session_rows_tx,
                                        )
                                        .await;
                                    }
                                }
                                TuiMode::SessionPicker => {
                                    if let Some(selected) = input::handle_picker_key(&mut app, key)
                                    {
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
                                    if i < app.tabs.len() {
                                        app.set_active_tab(i);
                                    }
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
                        // Coalesced: a burst of writes while a reload is running
                        // costs one follow-up reload, not one per write.
                        refresh::request(&mut app, &id, refresh::RefreshKind::Session, &refresh_tx);
                        if app.current().is_some_and(|t| t.session_db_id == id) {
                            refresh::request(
                                &mut app,
                                &id,
                                refresh::RefreshKind::Status,
                                &refresh_tx,
                            );
                        }
                    }
                    Action::Refresh(msg) => {
                        let applied = refresh::apply(&mut app, msg, |agent, meta| {
                            refresh::resolve(&server, &backend, agent, meta)
                        });
                        needs_draw |= applied.redraw;
                        if let Some((id, kind)) = applied.follow_up {
                            refresh::request(&mut app, &id, kind, &refresh_tx);
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
                    Action::SessionLoad(load) => match load {
                        SessionLoad::Catalog { generation, result } => {
                            apply_session_catalog_result(&mut app, generation, result);
                        }
                        SessionLoad::Row { generation, info } => {
                            apply_session_row(&mut app, generation, info);
                        }
                    },
                }

                // The picker owns the background fill; the moment we're not in it,
                // stop opening session DBs the user isn't looking at. A re-open
                // restarts the walk for whatever rows never finished.
                if !matches!(app.mode, TuiMode::SessionPicker) {
                    app.cancel_session_fill();
                }

                if app.should_quit {
                    break 'run;
                }
            }

            // Tab or session switch: show the new tab's cached strip now and
            // refresh it in the background.
            ensure_view(&mut app, &server, &session_rows_tx);
            let current = app.current().map(|t| t.session_db_id.clone()).unwrap_or_default();
            if current != status_for {
                status_for = current;
                refresh::request(
                    &mut app,
                    &status_for,
                    refresh::RefreshKind::Status,
                    &refresh_tx,
                );
                needs_draw = true;
            }
        }

        restore_terminal();
        Ok(())
    }
}
