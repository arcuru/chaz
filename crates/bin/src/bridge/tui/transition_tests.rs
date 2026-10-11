//! Ordered UI transitions against disposable, non-executing backend state.
use super::*;
use chaz_core::server::{BuildOptions, BuiltServer, McpReadiness};
use eidetica::{Instance, NewUser, backend::database::InMemory};
use tokio::sync::oneshot;

async fn runtime(dir: &std::path::Path) -> BuiltServer {
    let (instance, user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewUser::passwordless("ui-transitions"),
    )
    .await
    .unwrap();
    let mut config: Config = serde_yaml::from_str(&format!(
        r#"execution: client
state_dir: {0}
eidetica:
  connection: sqlite://{0}/unused.db
  login:
    username: ui-transitions
    passwordless: true
agents:
  - name: a
  - name: b
  - name: c
default_agents: [a, b]
"#,
        dir.display()
    ))
    .unwrap();
    chaz_core::server::build(
        &mut config,
        instance,
        user,
        BuildOptions {
            config_path: dir.join("config.yaml"),
            run_routine_engine: false,
            bootstrap_agents_from_config: false,
            run_agent_loop: false,
            extra_auto_approved_tools: Vec::new(),
            mcp_readiness: McpReadiness::Deferred,
            targeted_session: None,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn queued_defaults_add_then_delete_and_move_preserve_identity_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path()).await;
    let server = &built.server;
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let (approval, _) = mpsc::channel(8);
    let (notify, _) = mpsc::channel(64);
    let (models, _) = mpsc::channel(4);
    let (rows, _) = mpsc::channel(128);
    let mut observations = Vec::new();
    for reorder in [false, true] {
        server.set_default_agents(vec!["a".into(), "b".into()]);
        let mut app = App::hub(HashSet::new());
        app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
        app.peer_settings_index = PeerSettingsCategory::ALL
            .iter()
            .position(|c| matches!(c, PeerSettingsCategory::Defaults))
            .unwrap();
        app.peer_defaults = server.default_agents();
        let (release, gate) = oneshot::channel();
        operations::submit(&mut app, true, None, async move {
            gate.await.unwrap();
            Ok(operations::Effect::Sent)
        });
        handle_settings_outcome(
            input::SettingsKey::PromptSubmit {
                intent: SettingsPromptIntent::AddPeerDefault,
                value: "c".into(),
            },
            &mut app,
            server,
            &backend,
            &built.secret_store,
            &approval,
            &notify,
            &models,
            &rows,
        );
        // Accept the next action before the earlier add can update worker state.
        app.peer_defaults_cursor = usize::from(reorder);
        let key = if reorder {
            KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL)
        } else {
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)
        };
        let outcome = input::handle_settings_key(&mut app, key, SettingsScope::Peer);
        handle_settings_outcome(
            outcome,
            &mut app,
            server,
            &backend,
            &built.secret_store,
            &approval,
            &notify,
            &models,
            &rows,
        );
        assert_eq!(
            server.default_agents(),
            ["a", "b"],
            "test did not hold worker pending"
        );
        release.send(()).unwrap();
        for _ in 0..3 {
            let result = app.operations.recv().await.unwrap();
            operations::apply(&mut app, result);
        }
        let expected = if reorder {
            vec!["b", "a", "c"]
        } else {
            vec!["b", "c"]
        };
        let persisted = server.registry().load_peer_default_agents().await.unwrap();
        observations.push((server.default_agents(), persisted, expected));
    }
    built.shutdown().await;
    for (worker, persisted, expected) in observations {
        assert_eq!(worker, expected, "later delete/move lost accepted add-c");
        assert_eq!(persisted, expected);
    }
}

#[tokio::test]
async fn queued_old_catalog_cannot_clear_or_abort_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path()).await;
    let mut app = App::hub(HashSet::new());
    let (tx, mut rx) = mpsc::channel(4);
    spawn_catalog_load(
        &mut app,
        BackendManager::new(&None, built.secret_store.clone()),
        tx,
    );
    // Completion A has already left its task, so aborting A cannot retract it.
    let old = rx.recv().await.unwrap();
    reconnect::disconnect(&mut app);
    app.backend_available = true;
    let (release, gate) = oneshot::channel::<()>();
    let (tx, mut new_rx) = mpsc::channel(4);
    start_catalog_load(&mut app, tx, async move {
        gate.await.unwrap();
        Ok(vec![ModelInfo {
            id: "replacement-B".into(),
            ..Default::default()
        }])
    });
    tokio::task::yield_now().await;
    apply_model_catalog(&mut app, old);
    assert!(app.model_picker_loading, "stale A cleared B loading state");
    assert!(
        app.model_catalog_task.is_some(),
        "stale A dropped B task ownership"
    );
    assert!(!release.is_closed(), "stale A aborted B");
    assert!(
        app.session_catalog.is_none(),
        "stale A installed its catalog"
    );
    release.send(()).unwrap();
    apply_model_catalog(&mut app, new_rx.recv().await.unwrap());
    assert!(!app.model_picker_loading && app.model_catalog_task.is_none());
    assert_eq!(app.session_catalog.as_ref().unwrap()[0].id, "replacement-B");
    built.shutdown().await;
}

async fn close_then_next_event(event: Event) {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path()).await;
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let (_, db) = built.registry.create_session(Some("tui")).await.unwrap();
    let id = db.root_id().to_string();
    let mut app = App::new(
        HashSet::new(),
        build_tab(&built.server, &backend, db, id).await.unwrap(),
    );
    let (approval, _) = mpsc::channel(8);
    let (notify, _) = mpsc::channel(64);
    let (models, _) = mpsc::channel(4);
    let (rows, _) = mpsc::channel(128);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| view::ui(f, &mut app, &built.server, &backend, &Config::default()))
        .unwrap();
    // Reproduce the next already-ready event before the batch-end ensure_view.
    app.close_tab_at(0);
    match event {
        Event::Key(key) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match app.mode {
                TuiMode::Chat => handle_chat_action(
                    ChatAction::OpenSettings(SettingsScope::Session),
                    &mut app,
                    &built.server,
                    &backend,
                    &built.secret_store,
                    &approval,
                    &notify,
                    &models,
                    &rows,
                ),
                TuiMode::SessionPicker => {
                    app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker)
                }
                _ => panic!("unexpected close destination"),
            }
            assert_eq!(app.mode, TuiMode::Settings(SettingsScope::Peer));
        }
        Event::Key(key) => {
            if app.mode == TuiMode::Chat {
                input::handle_chat_key(&mut app, key);
            } else {
                input::handle_picker_key(&mut app, key);
            }
            assert_eq!(app.mode, TuiMode::SessionPicker);
        }
        Event::Mouse(mouse) => {
            // Real loop draws BEFORE dispatching a click when the batch changed.
            terminal
                .draw(|f| view::ui(f, &mut app, &built.server, &backend, &Config::default()))
                .unwrap();
            input::handle_mouse(&mut app, mouse);
        }
        _ => unreachable!(),
    }
    terminal
        .draw(|f| view::ui(f, &mut app, &built.server, &backend, &Config::default()))
        .unwrap();
    built.shutdown().await;
}
#[tokio::test]
async fn last_tab_close_then_pageup_in_same_batch() {
    close_then_next_event(Event::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )))
    .await;
}
#[tokio::test]
async fn last_tab_close_then_settings_in_same_batch() {
    close_then_next_event(Event::Key(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )))
    .await;
}
#[tokio::test]
async fn last_tab_close_then_preclick_draw_in_same_batch() {
    close_then_next_event(Event::Mouse(MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 119,
        row: 38,
        modifiers: KeyModifiers::NONE,
    }))
    .await;
}

#[tokio::test]
async fn sessionless_jobs_escape_and_pending_exit_preserve_batch_view_contract() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path()).await;
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let (notify, _) = mpsc::channel(64);
    let (jobs_tx, _) = mpsc::channel(1);
    let mut app = App::hub(HashSet::new());
    app.mode = TuiMode::Jobs;
    jobs::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &built.server,
        &backend,
        &notify,
        &jobs_tx,
    );
    assert_eq!(app.mode, TuiMode::SessionPicker);
    // Real pre-click draw follows Esc in the same event batch, before ensure_view.
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| view::ui(f, &mut app, &built.server, &backend, &Config::default()))
        .unwrap();
    app.mode = TuiMode::Jobs;
    operations::submit(
        &mut app,
        true,
        Some("accepted steering".into()),
        std::future::pending(),
    );
    assert!(!app.operations.request_exit());
    terminal
        .draw(|f| view::ui(f, &mut app, &built.server, &backend, &Config::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
    assert!(
        text.contains("Quit again"),
        "Jobs concealed pending exit confirmation"
    );
    assert!(app.operations.request_exit());
    assert!(
        app.operations
            .exit_report()
            .unwrap()
            .contains("accepted steering")
    );
    drop(app);
    built.shutdown().await;
}

#[tokio::test]
async fn uncertain_job_publication_identity_survives_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path()).await;
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let (_, db) = built.registry.create_session(Some("tui")).await.unwrap();
    let id = db.root_id().to_string();
    let mut app = App::hub(HashSet::new());
    let retry = Arc::new(tokio::sync::Mutex::new(Some(
        chaz_core::session::steering::JobInputRequest::new(
            "attempt".into(),
            "accepted text".into(),
        ),
    )));
    for opening in 0..2 {
        let mut tab = build_tab(&built.server, &backend, db.clone(), id.clone())
            .await
            .unwrap();
        tab.job = Some(jobs::JobTab {
            state: None,
            inputs: Vec::new(),
            feedback: String::new(),
            retry: Default::default(),
            details: false,
        });
        app.push_tab(tab);
        if opening == 0 {
            *app.active().job.as_ref().unwrap().retry.lock().await = retry.lock().await.clone();
        }
        assert_eq!(
            app.active()
                .job
                .as_ref()
                .unwrap()
                .retry
                .lock()
                .await
                .as_ref()
                .unwrap()
                .id,
            retry.lock().await.as_ref().unwrap().id
        );
        app.close_tab_at(0);
    }
    built.shutdown().await;
}

#[tokio::test]
async fn pending_exit_banner_wins_in_all_modes_without_consuming_edits() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path()).await;
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let (_, db) = built.registry.create_session(Some("tui")).await.unwrap();
    let id = db.root_id().to_string();
    let mut app = App::new(
        HashSet::new(),
        build_tab(&built.server, &backend, db, id).await.unwrap(),
    );
    app.input = "composer-draft".into();
    operations::submit(
        &mut app,
        true,
        Some("accepted-write".into()),
        std::future::pending(),
    );
    assert!(!app.operations.request_exit());
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    for mode in [
        TuiMode::Chat,
        TuiMode::Jobs,
        TuiMode::SessionPicker,
        TuiMode::Settings(SettingsScope::Peer),
        TuiMode::Settings(SettingsScope::Session),
        TuiMode::ModelPicker,
    ] {
        app.mode = mode;
        app.settings_prompt = Some(SettingsPrompt {
            label: "edit".into(),
            input: "inline-draft".into(),
            cursor: 12,
            intent: SettingsPromptIntent::AddPeerDefault,
        });
        app.model_search = "model-draft".into();
        // Later transient notices cannot conceal the armed decision either.
        app.operations.notice = Some("another notice".into());
        for overlay in [
            None,
            Some(Overlay::Help { scroll: 0 }),
            Some(Overlay::RenamePrompt {
                session_db_id: "rename-target".into(),
                title: "Rename".into(),
                input: "rename-draft".into(),
                cursor: 12,
            }),
        ] {
            app.overlay = overlay;
            terminal
                .draw(|f| view::ui(f, &mut app, &built.server, &backend, &Config::default()))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Quit again"), "armed quit hidden in {mode:?}");
            assert_eq!(app.input, "composer-draft");
            assert_eq!(app.settings_prompt.as_ref().unwrap().input, "inline-draft");
            assert!(app.operations.is_pending());
        }
    }
    assert!(app.operations.request_exit());
    assert!(
        app.operations
            .exit_report()
            .unwrap()
            .contains("accepted-write")
    );
    drop(app);
    built.shutdown().await;
}

#[tokio::test]
async fn ordinary_open_candidates_do_not_register_before_view_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path()).await;
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let (_, db) = built.registry.create_session(Some("tui")).await.unwrap();
    let id = db.root_id().to_string();
    let (approval, _) = mpsc::channel(8);
    let (notify, _) = mpsc::channel(64);
    let mut app = App::hub(HashSet::new());
    let ctx = operation_context(
        &built.server,
        &backend,
        &built.secret_store,
        &approval,
        &notify,
    );
    operations::submit(
        &mut app,
        true,
        None,
        ctx.command(Command::SwitchSession(id.clone()), None, None),
    );
    let result = app.operations.recv().await.unwrap();
    assert!(
        !built.server.is_session_open(&id).await,
        "candidate registered before ownership"
    );
    app.navigation_generation += 1; // Discard after newer navigation.
    operations::apply(&mut app, result);
    assert!(app.tabs.is_empty());
    assert!(
        !built.server.is_session_open(&id).await,
        "discarded open installed a receiver"
    );

    let ctx = operation_context(
        &built.server,
        &backend,
        &built.secret_store,
        &approval,
        &notify,
    );
    operations::submit(
        &mut app,
        true,
        None,
        ctx.command(Command::SwitchSession(id.clone()), None, None),
    );
    let following_server = built.server.clone();
    let following_id = id.clone();
    operations::submit(
        &mut app,
        true,
        Some("following action".into()),
        async move {
            assert!(
                following_server.is_session_open(&following_id).await,
                "later action ran before open registration completed"
            );
            Ok(operations::Effect::Sent)
        },
    );
    let result = app.operations.recv().await.unwrap();
    operations::apply(&mut app, result);
    assert!(
        app.operations
            .exit_report()
            .unwrap()
            .contains("following action")
    );
    assert!(
        app.tabs.is_empty(),
        "view exposed before observation installation"
    );
    let result = app.operations.recv().await.unwrap();
    operations::apply(&mut app, result);
    assert_eq!(app.active().session_db_id, id);
    assert!(built.server.is_session_open(&id).await);
    let result = app.operations.recv().await.unwrap();
    operations::apply(&mut app, result);
    assert!(!app.operations.is_pending());
    let observation = app.active().live.observation.as_ref().unwrap().id;
    let incarnation = app.active().live.incarnation;
    app.input = "retained-draft".into();
    for stale in [false, true] {
        let ctx = operation_context(
            &built.server,
            &backend,
            &built.secret_store,
            &approval,
            &notify,
        );
        operations::submit(
            &mut app,
            true,
            None,
            ctx.command(Command::SwitchSession(id.clone()), None, None),
        );
        let result = app.operations.recv().await.unwrap();
        if stale {
            app.navigation_generation += 1;
        }
        operations::apply(&mut app, result);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active().live.incarnation, incarnation);
        assert_eq!(
            app.active().live.observation.as_ref().unwrap().id,
            observation
        );
        assert_eq!(app.input, "retained-draft");
    }
    drop(app);
    built.shutdown().await;
}
