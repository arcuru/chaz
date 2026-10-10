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
    let mut config: Config = serde_yaml::from_str(&format!("execution: client\nstate_dir: {0}\neidetica:\n  connection: sqlite://{0}/unused.db\n  login:\n    username: ui-transitions\n    passwordless: true\nagents:\n  - name: a\n  - name: b\n  - name: c\ndefault_agents: [a, b]\n", dir.display())).unwrap();
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
