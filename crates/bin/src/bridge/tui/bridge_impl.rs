//! Ready-state TUI loop and owned runtime teardown.
//!
//! Child module of `tui` so the impl retains access to `TuiBridge`/`App`
//! private state and the module-level render/terminal helpers via
//! `use super::*`.

use super::*;
use crossterm::event::MouseEventKind;

/// Upper bound on actions handled between two frames, so a flood of input
/// still repaints regularly.
const MAX_ACTIONS_PER_FRAME: usize = 64;

impl TuiBridge {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_ready(
        mut self,
        built: chaz_core::server::BuiltServer,
        runtime_path: std::path::PathBuf,
        terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
        events: &mut EventStream,
        app: App,
        terminal_owner: supervised::TerminalGuard,
        approval_tx: mpsc::Sender<TaggedApproval>,
        mut approval_rx: mpsc::Receiver<TaggedApproval>,
        notify_tx: presentation::Sender,
        mut notify_rx: mpsc::Receiver<presentation::Delivery>,
    ) -> anyhow::Result<()> {
        let mut server = built.server.clone();
        let mut runtime = Some(built);
        let mut reads = presentation::Reads::default();
        let (connection_tx, mut connection_rx) = mpsc::channel(1);
        let mut probe_task: Option<supervised::Task<()>> = None;
        let mut runtime_task: Option<supervised::Task<()>> = None;
        let result = self
            .run_terminal(
                &mut server,
                &mut runtime,
                runtime_path,
                &connection_tx,
                &mut connection_rx,
                &mut probe_task,
                &mut runtime_task,
                terminal,
                events,
                app,
                &approval_tx,
                &mut approval_rx,
                &notify_tx,
                &mut notify_rx,
                &mut reads,
            )
            .await;
        drop(terminal_owner);
        if let Ok(Some(report)) = &result {
            eprintln!("{report}");
        }
        // The terminal state has retired its registrations. A native read
        // that began verification must still publish before it can be dropped.
        notify_rx.close();
        notify_tx.drain().await;
        while reads.join_next().await.is_some() {}
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
                        if let reconnect::ResultMessage::Rebuilt(Ok(replacement)) = message {
                            replacement.shutdown().await;
                        }
                    }
                }
            }
        }
        while let Ok(message) = connection_rx.try_recv() {
            if let reconnect::ResultMessage::Rebuilt(Ok(replacement)) = message {
                replacement.shutdown().await;
            }
        }
        if let Some(built) = runtime {
            built.shutdown().await;
        }
        result.map(|_| ())
    }
}

impl TuiBridge {
    /// Own the live terminal separately from the joined runtime shutdown.
    #[allow(clippy::too_many_arguments)]
    async fn run_terminal(
        &mut self,
        server: &mut Arc<Server>,
        runtime: &mut Option<chaz_core::server::BuiltServer>,
        runtime_path: std::path::PathBuf,
        connection_tx: &mpsc::Sender<reconnect::ResultMessage>,
        connection_rx: &mut mpsc::Receiver<reconnect::ResultMessage>,
        probe_task: &mut Option<supervised::Task<()>>,
        runtime_task: &mut Option<supervised::Task<()>>,
        terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
        events: &mut EventStream,
        app: App,
        approval_tx: &mpsc::Sender<TaggedApproval>,
        approval_rx: &mut mpsc::Receiver<TaggedApproval>,
        notify_tx: &presentation::Sender,
        notify_rx: &mut mpsc::Receiver<presentation::Delivery>,
        reads: &mut presentation::Reads,
    ) -> anyhow::Result<Option<String>> {
        let mut reconnect_allowed = true;
        let mut reconnect_attempt = 0u32;
        let mut retry_at = None;
        let mut missed_probe = false;
        let mut app = app;
        let mut backend = BackendManager::new(&self.config.backends, self.secrets.clone());
        // One-shot-style delivery of background model catalog fetches.
        // Buffered so a force-refresh kicked off mid-render doesn't block.
        let (models_tx, mut models_rx) = mpsc::channel::<ModelCatalog>(4);
        let (workers_tx, mut workers_rx) = mpsc::channel(8);
        let (session_rows_tx, mut session_rows_rx) = mpsc::channel::<SessionLoad>(128);
        let (peer_pages_tx, mut peer_pages_rx) = mpsc::channel(1);
        let (session_pages_tx, mut session_pages_rx) = mpsc::channel(1);
        let (jobs_tx, mut jobs_rx) = mpsc::channel::<jobs::Loaded>(1);
        let mut catalog_changes = server.registry().catalog_changes();

        // An ordinary launch opens the hub — even on an empty install — and
        // creates nothing. The catalog loads in the background.
        if app.tabs.is_empty() {
            open_session_picker(&mut app, server, &session_rows_tx);
        }

        let mut activity_tick = tokio::time::interval(reconnect::PROBE_INTERVAL);
        activity_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut first_ready_frame = true;
        let mut needs_draw = true;
        'run: loop {
            reads.start_initial(&mut app);
            operations::cancel_closed_reads(&mut app);
            workers::update(&mut app, server, &workers_tx);
            ensure_view(&mut app, server, &session_rows_tx);
            peer_pages::update(&mut app, server, &peer_pages_tx);
            session_pages::update(&mut app, server, &backend, &session_pages_tx);
            if !app.backend_available
                && reconnect_allowed
                && retry_at.is_none()
                && runtime_task.is_none()
                && !app.operations.is_pending()
            {
                *runtime_task = Some(reconnect::spawn_rebuild(
                    runtime.take(),
                    self.config.clone(),
                    runtime_path.clone(),
                    connection_tx,
                ));
            }
            if needs_draw {
                terminal.draw(|f| view::ui(f, &mut app, server, &backend, &self.config))?;
                if first_ready_frame {
                    tracing::info!("TUI ready frame drawn");
                    first_ready_frame = false;
                }
                // Use this frame's viewport, not the previous frame's cursor or size.
                if matches!(app.mode, TuiMode::SessionPicker) {
                    request_session_metadata(&mut app, server, &session_rows_tx);
                }
                needs_draw = false;
            }

            if app.mode == TuiMode::Jobs
                && app.jobs.data.observed_at.is_none()
                && app.jobs.error.is_none()
            {
                jobs::refresh(&mut app, server, &jobs_tx);
            }
            // Block for one action, then drain whatever else is already
            // queued (bounded) before drawing again, so a burst of keys,
            // wheel notches or session writes costs one frame, not one each.
            // Every event is still handled, in arrival order.
            for batch_slot in 0..MAX_ACTIONS_PER_FRAME {
                let action = tokio::select! {
                    event = events.next() => {
                        let Some(event) = event else { break 'run };
                        match event? {
                            Event::Key(key) => Action::Key(key),
                            Event::Mouse(m) if input::mouse_is_actionable(&m) => {
                                // A click maps through the last frame's regions;
                                // draw first if earlier events in this batch
                                // moved anything.
                                if needs_draw && matches!(m.kind, MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_)) {
                                    terminal.draw(|f| {
                                        view::ui(f, &mut app, server, &backend, &self.config)
                                    })?;
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
                    Some(msg) = reads.join_next(), if !reads.is_empty() => Action::Refresh(msg),
                    Some(update) = notify_rx.recv(), if reads.has_capacity() => Action::SessionChanged(update),
                    Ok(()) = catalog_changes.changed() => Action::CatalogChanged,
                    Some(msg) = approval_rx.recv() => Action::ApprovalRequest(msg),
                    Some(res) = models_rx.recv() => Action::ModelsFetched(res),
                    Some(load) = session_rows_rx.recv() => Action::SessionLoad(load),
                    Some(loaded) = peer_pages_rx.recv() => Action::PeerPage(loaded),
                    Some(loaded) = session_pages_rx.recv() => Action::SessionPage(loaded),
                    Some(loaded) = workers_rx.recv() => Action::WorkersLoaded(loaded),
                    Some(loaded) = jobs_rx.recv() => Action::JobsLoaded(loaded),
                    _ = activity_tick.tick() => {
                        needs_draw |= workers::age(&mut app);
                        // Don't feed periodic read retries into an overdue
                        // probe or a socket that has just missed a deadline.
                        if app.backend_available && probe_task.is_none() && !missed_probe {
                            if app.mode == TuiMode::Jobs {
                                jobs::refresh(&mut app, server, &jobs_tx);
                            }
                            reconnect::request_tabs(
                                &mut app, server, &backend, approval_tx, notify_tx, connection_tx,
                            );
                            let now = chrono::Utc::now();
                            needs_draw |= refresh::age_activity(&mut app, now);
                            needs_draw |= refresh::age_steering(&mut app, now);
                        }
                        if app.backend_available && probe_task.is_none()
                            && server.registry().instance().remote_connection().is_some()
                        {
                            *probe_task = Some(reconnect::spawn_probe(server, connection_tx));
                        }
                        needs_draw |= app.operations.warn_if_late();
                        if !app.session_row_errors.is_empty()
                            && app.mode == TuiMode::SessionPicker
                        {
                            app.session_row_errors.clear();
                            needs_draw = true;
                        }
                        continue;
                    },
                    _ = tokio::time::sleep_until(retry_at.unwrap_or_else(tokio::time::Instant::now)), if retry_at.is_some() => {
                        retry_at = None;
                        break;
                    },
                    // Nothing else is ready: end the batch and draw.
                    _ = std::future::ready(()), if batch_slot > 0 => break,
                };
                // Background results decide for themselves whether they changed
                // anything visible; every other action can.
                if !matches!(action, Action::Refresh(_) | Action::SessionChanged(_)) {
                    needs_draw = true;
                }

                let navigation_before = (
                    app.mode,
                    app.focused_incarnation(),
                    app.peer_settings_index,
                    app.session_settings_index,
                    app.peer_agents_cursor,
                );
                match action {
                    Action::CatalogChanged => {
                        app.session_list_fresh = false;
                        app.session_catalog_invalidated = true;
                    }
                    Action::Key(key) => {
                        app.selection.clear();
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            app.should_quit = app.operations.request_exit();
                        } else if app.mode == TuiMode::NewSession {
                            creation::handle_key(&mut app, key);
                            if app.mode == TuiMode::SessionPicker {
                                open_session_picker(&mut app, server, &session_rows_tx);
                            }
                        } else if app.settings_too_small() && key.code != KeyCode::Esc {
                            // Only safe cancellation and the existing quit path remain live.
                            continue;
                        } else if app.field_sheet.is_some()
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            continue;
                        } else if key.code == KeyCode::Char('g')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            app.mode = if app.mode == TuiMode::Jobs {
                                TuiMode::Chat
                            } else {
                                TuiMode::Jobs
                            };
                            app.ensure_view();
                            if app.mode == TuiMode::Jobs {
                                jobs::refresh(&mut app, server, &jobs_tx);
                            }
                        } else if key.code == KeyCode::F(2) {
                            if app.mode == TuiMode::Jobs {
                                app.jobs.details = !app.jobs.details;
                                app.jobs.detail_scroll = 0;
                            } else if app.mode == TuiMode::Chat {
                                jobs::toggle_tab_details(app.active_mut());
                            }
                        } else if key.code == KeyCode::Char('d')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            app.debug_mode = !app.debug_mode;
                        } else if key.code == KeyCode::Char('w')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
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
                                        server,
                                        &backend,
                                        &self.secrets,
                                        approval_tx,
                                        notify_tx,
                                        &models_tx,
                                        &session_rows_tx,
                                    );
                                }
                                TuiMode::SessionPicker => {
                                    app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
                                }
                                TuiMode::NewSession
                                | TuiMode::Jobs
                                | TuiMode::ModelPicker
                                | TuiMode::Settings(_) => {}
                            }
                        } else if key.code == KeyCode::Char('p')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            // Ctrl+P toggles the session hub. In chat mode it
                            // opens it; in the hub it returns to the open
                            // conversation, or reloads a hub that has none (or
                            // whose catalog failed to load).
                            match app.mode {
                                TuiMode::NewSession | TuiMode::Jobs => {
                                    open_session_picker(&mut app, server, &session_rows_tx)
                                }
                                TuiMode::Chat => {
                                    handle_chat_action(
                                        ChatAction::OpenPicker,
                                        &mut app,
                                        server,
                                        &backend,
                                        &self.secrets,
                                        approval_tx,
                                        notify_tx,
                                        &models_tx,
                                        &session_rows_tx,
                                    );
                                }
                                TuiMode::SessionPicker => {
                                    if app.tabs.is_empty() || app.session_picker_error.is_some() {
                                        app.session_list_fresh = false;
                                        open_session_picker(&mut app, server, &session_rows_tx);
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
                                    apply_picker_rename(&mut app, server, session_db_id, name);
                                    continue;
                                }
                                input::OverlayKey::NotConsumed => {}
                            }
                            match app.mode {
                                TuiMode::NewSession => creation::handle_key(&mut app, key),
                                TuiMode::Jobs => jobs::handle_key(
                                    &mut app, key, server, &backend, notify_tx, &jobs_tx,
                                ),
                                TuiMode::Chat => {
                                    if let Some(chat_action) = input::handle_chat_key(&mut app, key)
                                    {
                                        handle_chat_action(
                                            chat_action,
                                            &mut app,
                                            server,
                                            &backend,
                                            &self.secrets,
                                            approval_tx,
                                            notify_tx,
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
                                            server,
                                            &backend,
                                            approval_tx,
                                            notify_tx,
                                        );
                                    }
                                }
                                TuiMode::ModelPicker => {
                                    match input::handle_model_picker_key(&mut app, key) {
                                        input::ModelPickerKey::Select(model_id) => {
                                            dispatch_model_selection(
                                                model_id,
                                                &mut app,
                                                server,
                                                &backend,
                                                &self.secrets,
                                                approval_tx,
                                                notify_tx,
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
                                        server,
                                        &backend,
                                        &self.secrets,
                                        approval_tx,
                                        notify_tx,
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
                                input::MouseOutcome::Chat(action) => handle_chat_action(
                                    action,
                                    &mut app,
                                    server,
                                    &backend,
                                    &self.secrets,
                                    approval_tx,
                                    notify_tx,
                                    &models_tx,
                                    &session_rows_tx,
                                ),
                                input::MouseOutcome::Copy(text) => {
                                    if let Err(error) = selection::copy_to(&mut io::stdout(), &text)
                                    {
                                        tracing::warn!(%error, "Terminal clipboard request failed");
                                    }
                                }
                                input::MouseOutcome::PickerOpenSelected => {
                                    let selected = app.picker_selection();
                                    dispatch_picker_selection(
                                        selected,
                                        &mut app,
                                        server,
                                        &backend,
                                        approval_tx,
                                        notify_tx,
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
                                            server,
                                            &backend,
                                            &self.secrets,
                                            approval_tx,
                                            notify_tx,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Action::SessionChanged(update) => {
                        reads.start(&mut app, update);
                    }
                    Action::Connection(msg) => match msg {
                        reconnect::ResultMessage::Probe(result) => {
                            *probe_task = None;
                            use reconnect::ProbeFailure;
                            let reason = match result {
                                Ok(()) => {
                                    missed_probe = false;
                                    None
                                }
                                Err(ProbeFailure::Deadline) if missed_probe => {
                                    tracing::warn!(
                                        "TUI service unresponsive after two health-probe deadlines"
                                    );
                                    Some("Backend unresponsive")
                                }
                                Err(ProbeFailure::Deadline) => {
                                    missed_probe = true;
                                    tracing::warn!(
                                        "TUI service health probe timed out; confirming before reconnect"
                                    );
                                    None
                                }
                                Err(ProbeFailure::Disconnected(error)) => {
                                    tracing::warn!("TUI service disconnected: {error}");
                                    Some("Backend disconnected")
                                }
                                Err(ProbeFailure::Other(error)) => {
                                    missed_probe = false;
                                    tracing::warn!("Service health read failed: {error}");
                                    None
                                }
                            };
                            if let Some(reason) = reason {
                                missed_probe = false;
                                reconnect::disconnect(&mut app, reason);
                                if let Some(connection) =
                                    server.registry().instance().remote_connection()
                                {
                                    connection.retire();
                                }
                            }
                        }
                        reconnect::ResultMessage::Rebuilt(result) => {
                            *runtime_task = None;
                            match result {
                                Ok(replacement) => {
                                    reconnect_attempt = 0;
                                    *server = replacement.server.clone();
                                    catalog_changes = server.registry().catalog_changes();
                                    self.secrets = replacement.secret_store.clone();
                                    backend = BackendManager::new(
                                        &self.config.backends,
                                        self.secrets.clone(),
                                    );
                                    reconnect::connected(&mut app);
                                    reconnect::request_tabs(
                                        &mut app,
                                        server,
                                        &backend,
                                        approval_tx,
                                        notify_tx,
                                        connection_tx,
                                    );
                                    if app.mode == TuiMode::Jobs {
                                        jobs::refresh(&mut app, server, &jobs_tx);
                                    }
                                    *runtime = Some(*replacement);
                                    if app.mode == TuiMode::SessionPicker {
                                        open_session_picker(&mut app, server, &session_rows_tx);
                                    }
                                }
                                Err((error, transient)) => {
                                    reconnect_allowed = transient;
                                    tracing::warn!(%error, "TUI service reconnect failed");
                                    if transient {
                                        retry_at = Some(
                                            tokio::time::Instant::now()
                                                + reconnect::retry_delay(reconnect_attempt),
                                        );
                                        reconnect_attempt = reconnect_attempt.saturating_add(1);
                                    } else {
                                        app.backend_notice = Some(format!(
                                            "Reconnect stopped · cached data stale: {error}"
                                        ));
                                    }
                                }
                            }
                        }
                        reconnect::ResultMessage::Tab { ticket, reopened } => {
                            reconnect::apply_retry(&mut app, ticket, *reopened);
                            reads.start_initial(&mut app);
                        }
                    },
                    Action::Operation(msg) => {
                        operations::apply(&mut app, msg);
                    }
                    Action::Refresh(result) => match result {
                        Ok(loaded) => {
                            if let Some(id) =
                                presentation::apply(&mut app, loaded, |agent, meta| {
                                    refresh::resolve(server, &backend, agent, meta)
                                })
                            {
                                session_pages::changed(&mut app, &id);
                                needs_draw = true;
                            }
                        }
                        Err(error) => tracing::warn!(%error, "Snapshot read worker stopped"),
                    },
                    Action::ApprovalRequest((id, observation_id, exchange)) => {
                        if let Some(idx) = app.tab_index_for(&id).filter(|&i| {
                            app.tabs[i]
                                .live
                                .observation
                                .as_ref()
                                .is_some_and(|o| o.id == observation_id)
                        }) {
                            app.tabs[idx].pending_approval = Some(exchange);
                        } else {
                            // Tab was closed but an approval snuck through — deny
                            // so the runtime doesn't hang waiting.
                            let _ = exchange
                                .decision_tx
                                .send(chaz_core::bridge::ApprovalDecision::Deny);
                        }
                    }
                    Action::PeerPage(loaded) => peer_pages::apply(&mut app, loaded),
                    Action::SessionPage(loaded) => session_pages::apply(&mut app, loaded),
                    Action::WorkersLoaded(loaded) => workers::apply(&mut app, loaded),
                    Action::JobsLoaded(loaded) => jobs::apply_loaded(&mut app, loaded),
                    Action::ModelsFetched(res) => {
                        apply_model_catalog(&mut app, res);
                    }
                    Action::SessionLoad(load) => match load {
                        SessionLoad::Grouped { generation, result } => match result {
                            Ok((indices, groups)) => {
                                if matches!(
                                    apply_session_catalog_result(&mut app, generation, Ok(indices)),
                                    CatalogLoadOutcome::Loaded
                                ) {
                                    workers::group(&mut app, groups);
                                }
                            }
                            Err(error) => {
                                apply_session_catalog_result(&mut app, generation, Err(error));
                            }
                        },
                        #[cfg(test)]
                        SessionLoad::Catalog { generation, result } => {
                            apply_session_catalog_result(&mut app, generation, result);
                        }
                        SessionLoad::Row {
                            generation,
                            id,
                            result,
                        } => {
                            apply_session_row(&mut app, generation, id, result);
                        }
                    },
                }

                peer_pages::update(&mut app, server, &peer_pages_tx);
                session_pages::update(&mut app, server, &backend, &session_pages_tx);

                let navigation_after = (
                    app.mode,
                    app.focused_incarnation(),
                    app.peer_settings_index,
                    app.session_settings_index,
                    app.peer_agents_cursor,
                );
                if navigation_before != navigation_after {
                    app.navigation_generation += 1;
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

            ensure_view(&mut app, server, &session_rows_tx);
        }

        let exit_report = app.operations.exit_report();
        drop(app);
        Ok(exit_report)
    }
}
