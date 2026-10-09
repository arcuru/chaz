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
    async fn run(mut self, mut server: Arc<Server>) -> anyhow::Result<()> {
        let (mut runtime, runtime_path) = match self.runtime.take() {
            Some((built, path)) => (Some(built), Some(path)),
            None => (None, None),
        };
        let (connection_tx, mut connection_rx) = mpsc::channel(1);
        let mut probe_task: Option<supervised::Task<()>> = None;
        let mut runtime_task: Option<supervised::Task<()>> = None;
        let mut reconnect_allowed = true;
        let result = async {
        let (approval_tx, mut approval_rx) = mpsc::channel::<TaggedApproval>(8);
        let (notify_tx, mut notify_rx) = mpsc::channel::<String>(64);
        // One-shot-style delivery of background model catalog fetches.
        // Buffered so a force-refresh kicked off mid-render doesn't block.
        let (models_tx, mut models_rx) = mpsc::channel::<Result<Vec<ModelInfo>, String>>(4);
        let (session_rows_tx, mut session_rows_rx) = mpsc::channel::<SessionLoad>(128);
        // Background tab reads (session reloads, status strip, live-turn
        // counts). Tasks are owned by their tab's slots; the channel adds
        // bounded backpressure when many open tabs finish together.
        let (refresh_tx, mut refresh_rx) = mpsc::channel::<refresh::Refresh>(64);

        // Resolve any directly opened conversation before touching the
        // terminal, so a bad `--session` surfaces as a plain error.
        let launch = launch_session(
            &server,
            self.session.as_deref(),
            self.initial_prompt.is_some(),
        )
        .await?;

        let mut backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        let agent_names: HashSet<String> = server
            .agents()
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        let mut app = App::hub(agent_names);
        if let Some(session_db) = launch {
            let observation = setup_session(&server, &session_db, backend.clone(), approval_tx.clone(), notify_tx.clone()).await?;
            let session_db_id = session_db.root_id().to_string();
            let mut tab = build_tab(&server, &backend, session_db, session_db_id).await.map_err(anyhow::Error::msg)?;
            tab.live.observation = Some(observation);
            app.push_tab(tab);
            app.mode = TuiMode::Chat;
            // Prefill, never send: the user reviews the prompt first.
            if let Some(prompt) = self.initial_prompt.as_ref() {
                app.input = prompt.clone();
                app.cursor = app.input.len();
            }
        }

        supervised::install_panic_hook();
        let _terminal_owner = supervised::TerminalGuard;
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

        let mut activity_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        let mut needs_draw = true;
        'run: loop {
            operations::cancel_closed_reads(&mut app);
            if !app.backend_available && reconnect_allowed && runtime_task.is_none() && !app.operations.is_pending()
                && let Some(path) = runtime_path.clone()
            {
                let old = runtime.take();
                let config = self.config.clone();
                let tabs = app.tabs.iter().map(|t| (t.session_db_id.clone(), t.live.incarnation)).collect();
                let tx = connection_tx.clone(); let approval_tx = approval_tx.clone(); let notify_tx = notify_tx.clone();
                runtime_task = Some(supervised::Task(tokio::spawn(async move {
                    let result = supervised::work(async move { Ok(reconnect::rebuild(old, config, path, tabs, approval_tx, notify_tx).await) }).await
                        .unwrap_or_else(|e| Err((e, false)));
                    if let Err(e) = tx.send(reconnect::ResultMessage::Rebuilt(result)).await
                        && let reconnect::ResultMessage::Rebuilt(Ok(replacement)) = e.0
                    { replacement.built.shutdown().await; }
                })));
            }
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
                    Some(msg) = connection_rx.recv() => Action::Connection(msg),
                    Some(msg) = app.operations.recv() => Action::Operation(msg),
                    Some(msg) = refresh_rx.recv() => Action::Refresh(msg),
                    Some(id) = notify_rx.recv() => Action::SessionChanged(id),
                    Some(msg) = approval_rx.recv() => Action::ApprovalRequest(msg),
                    Some(res) = models_rx.recv() => Action::ModelsFetched(res),
                    Some(load) = session_rows_rx.recv() => Action::SessionLoad(load),
                    _ = activity_tick.tick() => {
                        if app.backend_available {
                            let requests: Vec<_> = app.tabs.iter().flat_map(|tab| [refresh::RefreshKind::Session, refresh::RefreshKind::Status, refresh::RefreshKind::Activity].into_iter().filter(|kind| *kind == refresh::RefreshKind::Activity || tab.live.failed(*kind)).map(|kind| (tab.session_db_id.clone(), kind))).collect();
                            for (id, kind) in requests { refresh::request(&mut app, &id, kind, &refresh_tx); }
                        }
                        if app.backend_available && runtime_path.is_some() && probe_task.is_none()
                            && server.registry().instance().remote_connection().is_some()
                        {
                            let instance = server.registry().instance().clone(); let tx = connection_tx.clone();
                            probe_task = Some(supervised::Task(tokio::spawn(async move {
                                let result = supervised::read(async move {
                                    match chaz_core::instance::probe_service(&instance).await {
                                        Ok(()) => Ok(Ok(())),
                                        Err(e) => { let transient = e.is_transient_service(); Ok(Err((e.to_string(), transient))) }
                                    }
                                }).await.unwrap_or_else(|e| Err((e, false)));
                                let _ = tx.send(reconnect::ResultMessage::Probe(result)).await;
                            })));
                        }
                        needs_draw |= app.operations.warn_if_late();
                        if !app.session_row_errors.is_empty() && app.mode == TuiMode::SessionPicker {
                            app.session_row_errors.clear();
                            needs_draw = true;
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

                let navigation_before = (app.mode, app.current().map(|t| t.live.incarnation), app.peer_settings_index, app.session_settings_index, app.peer_agents_cursor);
                match action {
                    Action::Key(key) => {
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            app.should_quit = app.operations.request_exit();
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
                                    );
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
                                    );
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
                                    apply_picker_rename(&mut app, &server, session_db_id, name);
                                    continue;
                                }
                                input::OverlayKey::NotConsumed => {}
                            }
                            match app.mode {
                                TuiMode::Chat => {
                                    if let Some(chat_action) =
                                        input::handle_chat_key(&mut app, key)
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
                                        );
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
                                            &approval_tx,
                                            &notify_tx,
                                        );
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
                                            );
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
                                    );
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
                                    );
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
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Action::SessionChanged(id) => {
                        if !app.backend_available { continue; }
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
                    Action::Connection(msg) => match msg {
                        reconnect::ResultMessage::Probe(result) => {
                            probe_task = None;
                            if let Err((error, transient)) = result {
                                if transient {
                                    tracing::warn!("TUI service disconnected: {error}");
                                    reconnect::disconnect(&mut app);
                                } else {
                                    // A slow health read is not proof of a dead
                                    // connection and must not kill healthy work.
                                    tracing::warn!("Service health read failed: {error}");
                                }
                            }
                        }
                        reconnect::ResultMessage::Rebuilt(result) => {
                            runtime_task = None;
                            match result {
                                Ok(mut replacement) => {
                                    server = replacement.built.server.clone();
                                    self.secrets = replacement.built.secret_store.clone();
                                    backend = BackendManager::new(&self.config.backends, self.secrets.clone());
                                    reconnect::apply(&mut app, &mut replacement, &backend);
                                    runtime = Some(replacement.built);
                                    if app.mode == TuiMode::SessionPicker { open_session_picker(&mut app, &server, &session_rows_tx); }
                                }
                                Err((error, transient)) => {
                                    reconnect_allowed = transient;
                                    app.operations.notice = Some(format!("Reconnect failed: {error}; cached data retained"));
                                    // Park a retry rather than spin if the socket is still absent.
                                    if transient {
                                        let tx = connection_tx.clone();
                                        runtime_task = Some(supervised::Task(tokio::spawn(async move {
                                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                            let _ = tx.send(reconnect::ResultMessage::Retry).await;
                                        })));
                                    }
                                }
                            }
                        }
                        reconnect::ResultMessage::Retry => { runtime_task = None; }
                    },
                    Action::Operation(msg) => {
                        operations::apply(&mut app, msg);
                        if app.backend_available {
                            let ids: Vec<_> = app.tabs.iter().map(|t| t.session_db_id.clone()).collect();
                            for id in ids {
                                refresh::request(&mut app, &id, refresh::RefreshKind::Session, &refresh_tx);
                                refresh::request(&mut app, &id, refresh::RefreshKind::Status, &refresh_tx);
                            }
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
                    Action::ApprovalRequest((id, observation_id, exchange)) => {
                        if let Some(idx) = app.tab_index_for(&id).filter(|&i| app.tabs[i].live.observation.as_ref().is_some_and(|o| o.id == observation_id)) {
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
                        app.model_catalog_task = None;
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
                        SessionLoad::Row { generation, id, result } => {
                            apply_session_row(&mut app, generation, id, result);
                        }
                    },
                }

                let navigation_after = (app.mode, app.current().map(|t| t.live.incarnation), app.peer_settings_index, app.session_settings_index, app.peer_agents_cursor);
                if navigation_before != navigation_after {
                    app.navigation_generation += 1;
                    if app.backend_available && matches!(app.mode, TuiMode::Chat | TuiMode::Settings(SettingsScope::Session))
                        && let Some(id) = app.current().map(|t| t.session_db_id.clone())
                    {
                        if matches!(app.mode, TuiMode::Settings(SettingsScope::Session)) { refresh::request(&mut app, &id, refresh::RefreshKind::Session, &refresh_tx); }
                        refresh::request(&mut app, &id, refresh::RefreshKind::Status, &refresh_tx);
                    }
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

            ensure_view(&mut app, &server, &session_rows_tx);
        }

        let exit_report = app.operations.exit_report();
        drop(app);
        drop(terminal);
        drop(_terminal_owner);
        if let Some(report) = exit_report { eprintln!("{report}"); }
        Ok::<(), anyhow::Error>(())
        }.await;
        if let Some(mut task) = probe_task.take() {
            task.0.abort();
            let _ = (&mut task.0).await;
        }
        // Reconstruction may include writes: joining it is not cancellation or
        // rollback. The terminal has already been restored on all exit paths.
        if let Some(mut task) = runtime_task.take() {
            // Drain the bounded result channel while joining; otherwise a
            // completed worker could block shutdown sending its owned result.
            loop {
                tokio::select! {
                    _ = &mut task.0 => break,
                    Some(message) = connection_rx.recv() => {
                        if let reconnect::ResultMessage::Rebuilt(Ok(replacement)) = message { replacement.built.shutdown().await; }
                    }
                }
            }
        }
        while let Ok(message) = connection_rx.try_recv() {
            if let reconnect::ResultMessage::Rebuilt(Ok(replacement)) = message {
                replacement.built.shutdown().await;
            }
        }
        if let Some(built) = runtime {
            built.shutdown().await;
        }
        result
    }
}
