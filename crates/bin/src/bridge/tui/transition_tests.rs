//! Ordered UI transitions against disposable, non-executing backend state.
use super::*;
use chaz_core::server::{BuildOptions, BuiltServer, McpReadiness};
use eidetica::{Instance, NewUser, backend::database::InMemory};
use tokio::sync::oneshot;

fn steering_backend(port: u16) -> Vec<chaz_core::config::Backend> {
    serde_yaml::from_str(&format!("- type: openaicompatible\n  api_base: http://127.0.0.1:{port}/v1\n  api_key: offline-fixture\n  models: [{{name: stub}}]\n  max_retries: 0\n  request_timeout: 15\n")).unwrap()
}

async fn runtime(dir: &std::path::Path) -> BuiltServer {
    runtime_with_stub(dir, None).await
}

async fn runtime_with_stub(dir: &std::path::Path, stub_port: Option<u16>) -> BuiltServer {
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
    if let Some(port) = stub_port {
        config.execution = Some(chaz_core::config::ExecutionRole::Executor);
        config.backends = Some(steering_backend(port));
    }
    chaz_core::server::build(
        &mut config,
        instance,
        user,
        BuildOptions {
            config_path: dir.join("config.yaml"),
            run_routine_engine: false,
            bootstrap_agents_from_config: stub_port.is_some(),
            run_agent_loop: stub_port.is_some(),
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
            details: false,
        });
        app.push_tab(tab);
        if opening == 0 {
            *app.active().live.input_retry.lock().await = retry.lock().await.clone();
        }
        assert_eq!(
            app.active()
                .live
                .input_retry
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

/// Two bounded HTTP calls, both recorded and held before responding. No
/// fallback/provider exists; an extra request fails by closing the listener.
struct SteeringStub {
    port: u16,
    calls: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    started: [Arc<tokio::sync::Semaphore>; 2],
    release: [Arc<tokio::sync::Semaphore>; 2],
    task: supervised::Task<()>,
}

async fn steering_http_stub() -> SteeringStub {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let started = std::array::from_fn::<_, 2, _>(|_| Arc::new(tokio::sync::Semaphore::new(0)));
    let release = std::array::from_fn::<_, 2, _>(|_| Arc::new(tokio::sync::Semaphore::new(0)));
    let task = tokio::spawn({
        let calls = calls.clone();
        let started = started.clone();
        let release = release.clone();
        async move {
            let mut i = 0;
            for _ in 0..8 {
                if i == 2 {
                    break;
                }
                let (mut stream, _) =
                    tokio::time::timeout(std::time::Duration::from_secs(20), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut request = Vec::new();
                let body = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        let mut buf = [0; 4096];
                        let n = stream.read(&mut buf).await.unwrap();
                        assert!(n > 0);
                        request.extend_from_slice(&buf[..n]);
                        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let header =
                                std::str::from_utf8(&request[..end]).unwrap().to_lowercase();
                            if header.starts_with("get /v1/models ") {
                                break serde_json::Value::Null;
                            }
                            assert!(
                                header.starts_with("post /v1/chat/completions "),
                                "unexpected fixture route: {header}"
                            );
                            let length: usize = header
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length: "))
                                .unwrap()
                                .parse()
                                .unwrap();
                            if request.len() >= end + 4 + length {
                                break serde_json::from_slice::<serde_json::Value>(
                                    &request[end + 4..end + 4 + length],
                                )
                                .unwrap();
                            }
                        }
                    }
                })
                .await
                .unwrap();
                if body.is_null() {
                    let catalog =
                        serde_json::json!({"data":[{"id":"stub","context_length":32768}]})
                            .to_string();
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{catalog}", catalog.len()).as_bytes()).await.unwrap();
                    continue;
                }
                calls.lock().unwrap().push(body);
                started[i].add_permits(1);
                tokio::time::timeout(std::time::Duration::from_secs(20), release[i].acquire())
                    .await
                    .unwrap()
                    .unwrap()
                    .forget();
                let response = serde_json::json!({"id":format!("stub-{i}"), "object":"chat.completion", "model":"stub", "choices":[{"index":0, "finish_reason":"stop", "message":{"role":"assistant", "content":if i == 0 {"frame provisional"} else {"frame continued"}}}]}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
                i += 1;
            }
            assert_eq!(i, 2, "stub catalog traffic exceeded its bound");
        }
    });
    SteeringStub {
        port,
        calls,
        started,
        release,
        task: supervised::Task(task),
    }
}

fn steering_frame(app: &mut App, markers: &[&str], gray: bool) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
    terminal.draw(|f| view::ui_chat(f, app, &[])).unwrap();
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..40)
        .map(|y| (0..110).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    for marker in markers {
        let (x, y) = rows
            .iter()
            .enumerate()
            .find_map(|(y, row)| {
                row.find(marker)
                    .map(|x| (row[..x].chars().count() as u16, y as u16))
            })
            .unwrap_or_else(|| panic!("missing {marker}: {}", rows.join("\n")));
        assert_eq!(
            rows.iter()
                .map(|row| row.matches(marker).count())
                .sum::<usize>(),
            1,
            "input body must appear once"
        );
        for i in 0..marker.len() as u16 {
            if gray {
                assert_eq!(buffer[(x + i, y)].fg, theme::DIM);
            } else {
                assert_ne!(buffer[(x + i, y)].fg, theme::DIM);
            }
        }
    }
    rows.join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_steering_chat_and_job_frames_promote_before_response_refresh_and_reopen() {
    use chaz_core::session::jobs::JobState;
    use std::time::Duration;
    for is_job in [false, true] {
        let mut stub = steering_http_stub().await;
        let port = stub.port;
        let calls = stub.calls.clone();
        let started = stub.started.clone();
        let release = stub.release.clone();
        let dir = tempfile::tempdir().unwrap();
        let built = runtime_with_stub(dir.path(), Some(port)).await;
        let server = &built.server;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !server.is_startup_ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let backend =
            BackendManager::new(&Some(steering_backend(port)), built.secret_store.clone());
        let (parent, parent_db) = server.registry().create_session(Some("tui")).await.unwrap();
        let agent = server.agent_index().find_by_name("a").unwrap();
        server
            .registry()
            .attach_agent_to_session(&parent.0, &agent)
            .await
            .unwrap();
        let (id, db) = if is_job {
            let id = server
                .submit_agent_job(&parent.0, "a", "frame original")
                .await
                .unwrap();
            let (_, db) = server.registry().open_job_session(&id).await.unwrap();
            (id, db)
        } else {
            server
                .register_session(&parent_db, backend.clone(), Some("a".into()), None)
                .await
                .unwrap();
            let mut session = Session::new(parent.clone(), parent_db.clone()).await;
            session
                .add_entry(SessionEntry {
                    sender: "user".into(),
                    content: "frame original".into(),
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                    metadata: None,
                    routing: None,
                })
                .await
                .unwrap();
            (parent.0, parent_db)
        };
        tokio::time::timeout(Duration::from_secs(10), started[0].acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let (notify, _) = mpsc::channel(64);
        let tab = if is_job {
            jobs::open_tab(server, &backend, &id, notify.clone())
                .await
                .unwrap()
        } else {
            build_tab(server, &backend, db.clone(), id.clone())
                .await
                .unwrap()
        };
        let mut app = App::new(HashSet::from(["a".into()]), tab);
        let (approval, _) = mpsc::channel(8);
        let (models, _) = mpsc::channel(4);
        let (rows, _) = mpsc::channel(4);
        app.input = "frame steering one".into();
        app.cursor = app.input.len();
        assert!(
            input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
                .is_none()
        );
        app.input.push_str("frame steering two");
        app.cursor = app.input.len();
        let action =
            input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
        handle_chat_action(
            action,
            &mut app,
            server,
            &backend,
            &built.secret_store,
            &approval,
            &notify,
            &models,
            &rows,
        );
        let result = tokio::time::timeout(Duration::from_secs(10), app.operations.recv())
            .await
            .unwrap()
            .unwrap();
        operations::apply(&mut app, result);
        let mut pending = build_tab(server, &backend, db.clone(), id.clone())
            .await
            .unwrap();
        if is_job {
            pending.job = Some(jobs::JobTab {
                state: Some(JobState::StartedUnknown {
                    attempt_id: Session::steering_attempts(&db)
                        .await
                        .unwrap()
                        .pop()
                        .unwrap(),
                    activity_recent: true,
                }),
                details: false,
            });
        }
        app.tabs[0] = pending;
        assert_eq!(app.tabs[0].live.telemetry.as_ref().unwrap().totals.calls, 1);
        steering_frame(
            &mut app,
            &["frame steering one", "frame steering two"],
            true,
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
        release[0].add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), started[1].acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        // Actual background refresh is exercised, then close/reopen through each real opener.
        let (tx, mut rx) = mpsc::channel(8);
        refresh::request(&mut app, &id, refresh::RefreshKind::Session, &tx);
        let result = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        refresh::apply(&mut app, result, |_, _| refresh::Resolved {
            effective_model: "stub".into(),
            context_budget: 32_000,
            roster: vec![],
        });
        assert_eq!(app.tabs[0].live.telemetry.as_ref().unwrap().totals.calls, 2);
        assert!(app.tabs[0].live.telemetry.as_ref().unwrap().last_calls["a"].context_estimated);
        let screen = steering_frame(
            &mut app,
            &[
                "frame provisional",
                "frame steering one",
                "frame steering two",
            ],
            false,
        );
        assert!(screen.find("frame provisional") < screen.find("frame steering one"));
        assert!(
            !screen.contains("frame continued"),
            "promotion cannot wait for response"
        );
        app.close_tab_at(0);
        let reopened = if is_job {
            jobs::open_tab(server, &backend, &id, notify).await.unwrap()
        } else {
            build_tab(server, &backend, db.clone(), id.clone())
                .await
                .unwrap()
        };
        app.push_tab(reopened);
        assert_eq!(app.tabs[0].live.telemetry.as_ref().unwrap().totals.calls, 2);
        steering_frame(
            &mut app,
            &[
                "frame provisional",
                "frame steering one",
                "frame steering two",
            ],
            false,
        );
        let requests = calls.lock().unwrap().clone();
        assert!(
            !requests[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["content"]
                    .as_str()
                    .is_some_and(|t| t.contains("frame steering")))
        );
        let messages = requests[1]["messages"].as_array().unwrap();
        assert_eq!(messages[messages.len() - 2]["content"], "frame provisional");
        assert_eq!(
            messages[messages.len() - 1]["content"],
            "frame steering one\nframe steering two"
        );
        release[1].add_permits(1);
        (&mut stub.task.0).await.unwrap();
        if is_job {
            assert!(
                server
                    .wait_job(&id, Duration::from_secs(10))
                    .await
                    .unwrap()
                    .state
                    .is_terminal()
            );
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if Session::steering_attempts(&db).await.unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut final_tab = build_tab(server, &backend, db, id.clone()).await.unwrap();
        if is_job {
            final_tab.job = Some(jobs::JobTab {
                state: Some(JobState::Succeeded {
                    text: Some("frame continued".into()),
                }),
                details: false,
            });
        }
        app.tabs[0] = final_tab;
        assert_eq!(
            app.tabs[0].live.telemetry.as_ref().unwrap().totals.calls,
            2,
            "steering display and terminal settlement cannot duplicate usage"
        );
        let screen = steering_frame(
            &mut app,
            &["frame provisional", "frame steering one", "frame continued"],
            false,
        );
        assert!(screen.find("frame steering one") < screen.find("frame continued"));
        assert_eq!(calls.lock().unwrap().len(), 2);
        built.shutdown().await;
    }
}

#[tokio::test]
async fn busy_steering_publication_after_close_or_retry_is_not_an_idle_turn() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path()).await;
    let (id, db) = built
        .server
        .registry()
        .create_session(Some("tui"))
        .await
        .unwrap();
    let mut session = Session::new(id, db.clone()).await;
    let request = session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "original".into(),
            timestamp: chrono::Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    let attempt = session.start_turn_attempt(request.clone()).await.unwrap();
    session.complete_turn_attempt(&attempt, None).await.unwrap();
    let retry = Arc::new(tokio::sync::Mutex::new(None));
    let error = jobs::send(
        db.clone(),
        retry.clone(),
        "recover this text".into(),
        false,
        Some(attempt.attempt_id.clone()),
        true,
    )
    .await
    .err()
    .unwrap();
    assert!(error.contains("refused before publication"), "{error}");
    assert!(
        retry.lock().await.is_none(),
        "definite refusal cannot trap the recovered draft behind an uncertain ID"
    );
    assert!(session.turn_inputs().await.unwrap().is_empty());
    let newer = session.start_turn_attempt(request).await.unwrap();
    assert!(
        jobs::send(
            db.clone(),
            retry.clone(),
            "stale steering".into(),
            false,
            Some(attempt.attempt_id),
            true
        )
        .await
        .is_err()
    );
    assert!(
        session.turn_inputs().await.unwrap().is_empty(),
        "old intent cannot silently target a newer attempt"
    );
    session.complete_turn_attempt(&newer, None).await.unwrap();
    assert_eq!(
        Session::new(session.conversation_id.clone(), db.clone())
            .await
            .entries()
            .len(),
        1
    );
    assert!(matches!(
        jobs::send(
            db.clone(),
            retry,
            "explicit new idle input".into(),
            false,
            None,
            false
        )
        .await
        .unwrap(),
        operations::Effect::Sent
    ));
    assert_eq!(
        Session::new(session.conversation_id, db)
            .await
            .entries()
            .len(),
        2,
        "only a later explicit idle action starts a new turn"
    );
    built.shutdown().await;
}
