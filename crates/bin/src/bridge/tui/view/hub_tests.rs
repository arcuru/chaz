//! Session hub and launch regressions: the hub is the base view, launch and
//! view-close never create or delete stored sessions, and drafts, scroll and
//! Settings callers survive navigation. Frames are drawn, not snapshotted.
use super::super::{
    App, ModelPickerScope, SessionLoad, TuiMode, dispatch_model_selection,
    dispatch_picker_selection, ensure_view, input, launch_session, open_session_picker,
};
use super::list_tests::{draw, fixture, send_mouse};
use super::*;
use chaz_core::backends::ModelInfo;
use chaz_core::security::SecretStore;
use chaz_core::session::SessionIndex;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use std::collections::HashSet;
use tokio::sync::mpsc;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

async fn session_ids(server: &Server) -> Vec<String> {
    let mut ids: Vec<_> = server
        .registry()
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .map(|index| index.session_db_id)
        .collect();
    ids.sort();
    ids
}

fn frame(app: &mut App, server: &Arc<Server>, backend: &BackendManager, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let screen = draw(&mut terminal, app, server, backend).join("\n");
    println!("{w}x{h} {mode:?}:\n{screen}", mode = app.mode);
    screen
}

fn channels() -> (
    mpsc::Sender<super::super::TaggedApproval>,
    mpsc::Sender<String>,
    mpsc::Sender<SessionLoad>,
) {
    (mpsc::channel(8).0, mpsc::channel(8).0, mpsc::channel(8).0)
}

async fn settle(app: &mut App) {
    while app.operations.is_pending() {
        let done = tokio::time::timeout(std::time::Duration::from_secs(5), app.operations.recv())
            .await
            .unwrap()
            .unwrap();
        super::super::operations::apply(app, done);
    }
}

#[tokio::test]
async fn hub_launch_creates_nothing_and_is_a_safe_base_view() {
    let (_instance, server, backend, _) = fixture().await;
    let before = session_ids(&server).await;
    assert!(
        launch_session(&server, None, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        session_ids(&server).await,
        before,
        "plain launch created a session"
    );

    let mut app = App::hub(HashSet::new());
    let (_, _, rows_tx) = channels();
    open_session_picker(&mut app, &server, &rows_tx);
    assert!(app.tabs.is_empty());
    assert_eq!(app.mode, TuiMode::SessionPicker);

    // Loading, failed and empty are distinct and all keep New reachable.
    let loading = frame(&mut app, &server, &backend, 80, 24);
    assert!(loading.contains("+ New session"), "{loading}");
    assert!(loading.contains("Loading sessions…"), "{loading}");
    assert!(loading.contains("s peer settings"), "{loading}");
    assert!(loading.contains("Ctrl+C quit"), "{loading}");
    assert!(
        !loading.contains("Esc back"),
        "no conversation to go back to: {loading}"
    );
    app.cancel_session_fill();
    app.session_picker_error = Some("store offline".into());
    let failed = frame(&mut app, &server, &backend, 80, 24);
    assert!(
        failed.contains("Failed to load sessions: store offline — Ctrl+P retries."),
        "{failed}"
    );
    assert!(!failed.contains("No saved sessions"), "{failed}");
    app.session_picker_error = None;
    let empty = frame(&mut app, &server, &backend, 80, 24);
    assert!(empty.contains("No saved sessions yet"), "{empty}");
    for (w, h) in [(80, 24), (120, 40), (40, 8)] {
        for state in ["empty", "loading", "error"] {
            app.session_catalog_loading = state == "loading";
            app.session_picker_error = (state == "error").then(|| "store offline".into());
            let screen = frame(&mut app, &server, &backend, w, h);
            assert!(screen.contains("New session"), "{w}x{h} {state}: {screen}");
            let message = match state {
                "loading" => "Loading sessions…",
                "error" => "Failed to load sessions:",
                _ => "No saved sessions yet",
            };
            assert!(screen.contains(message), "{w}x{h} {state}: {screen}");
            assert!(screen.lines().last().unwrap().contains("n new"));
        }
    }
    app.session_catalog_loading = false;
    app.session_picker_error = None;

    // The base hub cannot be dismissed into a nonexistent conversation.
    for code in [KeyCode::Esc, KeyCode::Up, KeyCode::Down] {
        assert!(input::handle_picker_key(&mut app, key(code)).is_none());
        assert_eq!(app.mode, TuiMode::SessionPicker);
    }
    ensure_view(&mut app, &server, &rows_tx);
    assert_eq!(app.mode, TuiMode::SessionPicker);

    // Wheel and clicks over an empty hub never reach a missing transcript.
    let _ = frame(&mut app, &server, &backend, 80, 24);
    send_mouse(&mut app, MouseEventKind::ScrollDown, 10, 10);
    send_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 70, 20);
    let new = app
        .click_regions
        .iter()
        .find(|r| matches!(r.target, super::super::ClickTarget::PickerNew))
        .copied()
        .unwrap();
    assert!(matches!(
        input::handle_mouse(
            &mut app,
            super::list_tests::mouse(MouseEventKind::Down(MouseButton::Left), new.x, new.y)
        ),
        Some(input::MouseOutcome::PickerOpenSelected)
    ));

    // Peer Settings works with no conversation and returns to the hub.
    assert!(input::handle_picker_key(&mut app, key(KeyCode::Char('s'))).is_none());
    assert_eq!(app.mode, TuiMode::Settings(SettingsScope::Peer));
    for (w, h) in [(80, 24), (120, 40), (40, 8)] {
        let screen = frame(&mut app, &server, &backend, w, h);
        assert!(screen.contains("Peer"), "{w}x{h}: {screen}");
    }
    send_mouse(&mut app, MouseEventKind::ScrollDown, 30, 5);
    super::list_tests::settings_key(&mut app, KeyCode::Down);
    super::list_tests::settings_key(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, TuiMode::SessionPicker);
    assert!(app.tabs.is_empty());
    assert_eq!(session_ids(&server).await, before);
}

#[tokio::test]
async fn hub_frames_fit_supported_sizes_with_unicode_rows() {
    let (_instance, server, backend, _) = fixture().await;
    let mut app = App::hub(HashSet::new());
    super::super::apply_session_catalog(
        &mut app,
        vec![SessionIndex {
            session_db_id: "session-café".into(),
            source: Some("tui".into()),
            bridge: chaz_core::session::BridgeKind::Tui,
            created_at: None,
            status: chaz_core::session::SessionStatus::Active,
            name: Some("café 界 e\u{301}".into()),
        }],
    );
    for (w, h) in [(80, 24), (120, 40), (40, 8)] {
        let screen = frame(&mut app, &server, &backend, w, h);
        let lines: Vec<_> = screen.lines().collect();
        assert_eq!(lines.len(), h as usize);
        assert!(screen.contains("New session"), "{w}x{h}: {screen}");
        assert!(screen.contains("café 界"), "{w}x{h}: {screen}");
        let footer = lines.last().unwrap();
        assert!(footer.contains("n new"), "{w}x{h} footer: {footer:?}");
        assert!(!footer.contains("Esc back"), "{w}x{h} footer: {footer:?}");
    }
}

#[tokio::test]
async fn launch_creates_or_reopens_exactly_the_requested_session() {
    let (_instance, server, _backend, _) = fixture().await;
    let base = session_ids(&server).await;

    // Prompt-only: exactly one new, unnamed, TUI-origin session.
    let prompt_db = launch_session(&server, None, true).await.unwrap().unwrap();
    let prompt_id = prompt_db.root_id().to_string();
    let after_prompt = session_ids(&server).await;
    assert_eq!(after_prompt.len(), base.len() + 1);
    let row = server
        .registry()
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .find(|index| index.session_db_id == prompt_id)
        .unwrap();
    assert_eq!(row.bridge, chaz_core::session::BridgeKind::Tui);
    assert_eq!(row.name, None, "no special name for a prompt session");

    // Absent name: created once and named; present name (with or without a
    // prompt): reopened, never duplicated.
    let work = launch_session(&server, Some("work"), false)
        .await
        .unwrap()
        .unwrap();
    let work_id = work.root_id().to_string();
    assert_eq!(
        server.registry().find_by_name("work").await.unwrap(),
        Some(work_id.clone())
    );
    let after_named = session_ids(&server).await;
    assert_eq!(after_named.len(), after_prompt.len() + 1);
    for prompt in [true, false] {
        let again = launch_session(&server, Some("work"), prompt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.root_id().to_string(), work_id);
    }
    assert_eq!(session_ids(&server).await, after_named);

    // An ordinary session that happens to be called `tui` is just a session:
    // a hub launch neither reuses nor replaces it.
    let (_, tui) = server.registry().create_session(Some("tui")).await.unwrap();
    let tui_id = tui.root_id().to_string();
    server
        .registry()
        .set_session_name(&tui_id, "tui".into())
        .await
        .unwrap();
    let with_tui = session_ids(&server).await;
    assert!(
        launch_session(&server, None, false)
            .await
            .unwrap()
            .is_none()
    );
    let reopened = launch_session(&server, Some("tui"), false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reopened.root_id().to_string(), tui_id);
    assert_eq!(session_ids(&server).await, with_tui);

    // Failed authorization must not publish an alias. Construct a legacy stale
    // index entry directly to check that launch refuses to replace it.
    assert!(
        server
            .registry()
            .set_session_name("not-a-session", "ghost".into())
            .await
            .is_err()
    );
    assert!(
        server
            .registry()
            .find_by_name("ghost")
            .await
            .unwrap()
            .is_none()
    );
    let txn = server
        .registry()
        .chaz_group()
        .new_transaction()
        .await
        .unwrap();
    txn.get_store::<eidetica::store::DocStore>("session_names")
        .await
        .unwrap()
        .set_string("ghost", "not-a-session")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let error = launch_session(&server, Some("ghost"), true)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("Failed to open session 'ghost'"),
        "{error:#}"
    );
    assert_eq!(session_ids(&server).await, with_tui);
}

#[tokio::test]
async fn new_open_switch_and_close_preserve_sessions_drafts_and_scroll() {
    let (_instance, server, backend, _) = fixture().await;
    let existing = session_ids(&server).await;
    let (approval_tx, notify_tx, rows_tx) = channels();
    let mut app = App::hub(HashSet::new());
    open_session_picker(&mut app, &server, &rows_tx);

    // New creates and opens exactly one stored session.
    dispatch_picker_selection(
        "__new__".into(),
        &mut app,
        &server,
        &backend,
        &approval_tx,
        &notify_tx,
    );
    settle(&mut app).await;
    assert_eq!(app.mode, TuiMode::Chat);
    assert_eq!(app.tabs.len(), 1);
    let created = app.active().session_db_id.clone();
    let after_new = session_ids(&server).await;
    assert_eq!(after_new.len(), existing.len() + 1);
    assert!(after_new.contains(&created));

    for c in "draft-é界".chars() {
        input::handle_chat_key(&mut app, key(KeyCode::Char(c)));
    }
    input::handle_chat_key(&mut app, key(KeyCode::Left));
    let (draft, cursor) = (app.input.clone(), app.cursor);
    app.active_mut().scroll_offset = 5;

    // Hub → another stored session: a fresh composer there.
    open_session_picker(&mut app, &server, &rows_tx);
    assert!(!frame(&mut app, &server, &backend, 80, 24).contains("Ctrl+C quit · Esc"));
    assert!(input::handle_picker_key(&mut app, key(KeyCode::Esc)).is_none());
    assert_eq!(app.mode, TuiMode::Chat, "Esc returns to the live caller");
    assert_eq!((app.input.as_str(), app.cursor), (draft.as_str(), cursor));
    open_session_picker(&mut app, &server, &rows_tx);
    dispatch_picker_selection(
        existing[0].clone(),
        &mut app,
        &server,
        &backend,
        &approval_tx,
        &notify_tx,
    );
    settle(&mut app).await;
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.active().session_db_id, existing[0]);
    assert!(app.input.is_empty());
    app.input = "second".into();
    app.cursor = 3;

    // Switching back restores the first view's draft, cursor and scroll.
    app.set_active_tab(0);
    assert_eq!((app.input.as_str(), app.cursor), (draft.as_str(), cursor));
    assert_eq!(app.active().scroll_offset, 5);
    // Plain PageUp/PageDown still scroll this view rather than switching tabs.
    input::handle_chat_key(&mut app, key(KeyCode::PageUp));
    assert_eq!(app.active().scroll_offset, 25);
    assert_eq!(app.active().session_db_id, created);
    input::handle_chat_key(&mut app, key(KeyCode::PageDown));
    assert_eq!(app.active().scroll_offset, 5);
    assert_eq!(app.active().session_db_id, created);
    assert_eq!((app.input.as_str(), app.cursor), (draft.as_str(), cursor));
    let screen = frame(&mut app, &server, &backend, 120, 40);
    assert!(screen.contains("Ctrl+S settings"), "{screen}");
    assert!(screen.contains("Ctrl+H/L tabs"), "{screen}");
    // Picking an already-open session focuses its view, not a duplicate.
    open_session_picker(&mut app, &server, &rows_tx);
    dispatch_picker_selection(
        existing[0].clone(),
        &mut app,
        &server,
        &backend,
        &approval_tx,
        &notify_tx,
    );
    settle(&mut app).await;
    assert_eq!(app.tabs.len(), 2);
    assert_eq!((app.input.as_str(), app.cursor), ("second", 3));

    // Session Settings round trip keeps the caller, scope and composer.
    app.set_active_tab(0);
    app.open_settings(SettingsScope::Session, TuiMode::Chat);
    let screen = frame(&mut app, &server, &backend, 80, 24);
    assert!(screen.contains("Session Settings"), "{screen}");
    super::list_tests::settings_key(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, TuiMode::Chat);
    assert_eq!(app.active().session_db_id, created);
    assert_eq!((app.input.as_str(), app.cursor), (draft.as_str(), cursor));
    assert_eq!(app.active().scroll_offset, 5);

    // Closing views never deletes sessions; the last one lands on the hub.
    app.close_tab_at(app.active_tab);
    assert_eq!(app.active().session_db_id, existing[0]);
    assert_eq!((app.input.as_str(), app.cursor), ("second", 3));
    app.close_tab_at(app.active_tab);
    assert!(app.tabs.is_empty() && app.input.is_empty());
    ensure_view(&mut app, &server, &rows_tx);
    assert_eq!(app.mode, TuiMode::SessionPicker);
    assert_eq!(session_ids(&server).await, after_new);
    let screen = frame(&mut app, &server, &backend, 80, 24);
    assert!(!screen.contains("Esc back"), "{screen}");

    // Reopen works and creates nothing.
    dispatch_picker_selection(
        created.clone(),
        &mut app,
        &server,
        &backend,
        &approval_tx,
        &notify_tx,
    );
    settle(&mut app).await;
    assert_eq!(app.mode, TuiMode::Chat);
    assert_eq!(app.active().session_db_id, created);
    assert_eq!(session_ids(&server).await, after_new);
}

#[tokio::test]
async fn hub_failures_stay_in_the_hub_and_peer_actions_need_no_conversation() {
    let (_instance, server, backend, _) = fixture().await;
    let before = session_ids(&server).await;
    let (approval_tx, notify_tx, rows_tx) = channels();
    let mut app = App::hub(HashSet::new());
    open_session_picker(&mut app, &server, &rows_tx);
    dispatch_picker_selection(
        "missing-session".into(),
        &mut app,
        &server,
        &backend,
        &approval_tx,
        &notify_tx,
    );
    settle(&mut app).await;
    assert_eq!(app.mode, TuiMode::SessionPicker);
    assert!(app.tabs.is_empty());
    let notice = app.hub_notice.clone().expect("failure notice");
    assert!(notice.contains("Failed to switch session"), "{notice}");
    let screen = frame(&mut app, &server, &backend, 80, 24);
    assert!(screen.contains("Failed to switch session"), "{screen}");
    input::handle_picker_key(&mut app, key(KeyCode::Down));
    assert!(app.hub_notice.is_none(), "next hub key clears the notice");
    assert_eq!(session_ids(&server).await, before);

    // An agent-global model edit from Peer Settings dispatches without a
    // session and reports where the user is.
    app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
    app.model_picker_scope = ModelPickerScope::AgentGlobal("nobody".into());
    app.model_picker_caller = app.mode;
    app.mode = TuiMode::ModelPicker;
    let _ = frame(&mut app, &server, &backend, 80, 24);
    ensure_view(&mut app, &server, &rows_tx);
    assert_eq!(app.mode, TuiMode::ModelPicker);
    dispatch_model_selection(
        "stub".into(),
        &mut app,
        &server,
        &backend,
        &SecretStore::new(server.registry().chaz_peer().clone()).await,
        &approval_tx,
        &notify_tx,
    );
    settle(&mut app).await;
    assert_eq!(app.mode, TuiMode::Settings(SettingsScope::Peer));
    let status = app.settings_status.clone().unwrap_or_default();
    assert!(
        status.contains("No hosted agent matches 'nobody'"),
        "{status}"
    );
    assert!(app.tabs.is_empty());
    assert_eq!(session_ids(&server).await, before);
}

#[tokio::test]
async fn hub_peer_model_picker_with_models_and_no_pin_needs_no_tab() {
    let (_instance, server, backend, _) = fixture().await;
    let mut app = App::hub(HashSet::new());
    app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
    app.seed_model_picker(
        &backend,
        None,
        ModelPickerScope::AgentGlobal("agent-00".into()),
    );
    app.rebuild_model_list(vec![ModelInfo {
        id: "stub-model".into(),
        ..Default::default()
    }]);
    app.model_picker_caller = app.mode;
    app.mode = TuiMode::ModelPicker;
    for (w, h) in [(80, 24), (120, 40), (40, 8)] {
        let screen = frame(&mut app, &server, &backend, w, h);
        assert!(
            screen.contains("Pick model — agent-00"),
            "{w}x{h}: {screen}"
        );
        if w >= 80 {
            assert!(screen.contains("stub-model"), "{w}x{h}: {screen}");
        }
        assert_eq!(app.model_picker_selection().as_deref(), Some("stub-model"));
        assert!(
            !screen.contains("(current)"),
            "no model is pinned: {screen}"
        );
    }
    input::handle_model_picker_key(&mut app, key(KeyCode::Esc));
    assert_eq!(app.mode, TuiMode::Settings(SettingsScope::Peer));
    super::list_tests::settings_key(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, TuiMode::SessionPicker);
    assert!(app.tabs.is_empty());
}

#[tokio::test]
async fn hub_tab_cycle_cannot_change_settings_or_switcher_caller() {
    let (_instance, server, backend, mut app) = fixture().await;
    let first = app.active().session_db_id.clone();
    let (_, db) = server.registry().create_session(Some("tui")).await.unwrap();
    let id = db.root_id().to_string();
    let tab = super::super::build_tab(&server, &backend, db, id)
        .await
        .unwrap();
    app.push_tab(tab);
    app.set_active_tab(0);
    app.input = "first draft".into();
    app.cursor = 4;
    app.active_mut().scroll_offset = 7;
    for mode in [
        TuiMode::SessionPicker,
        TuiMode::Settings(SettingsScope::Session),
        TuiMode::Settings(SettingsScope::Peer),
        TuiMode::ModelPicker,
    ] {
        app.mode = mode;
        for direction in [-1, 1] {
            super::super::cycle_tab(&mut app, direction);
            assert_eq!(
                app.active().session_db_id,
                first,
                "caller changed in {mode:?}"
            );
            assert_eq!((app.input.as_str(), app.cursor), ("first draft", 4));
            assert_eq!(app.active().scroll_offset, 7);
        }
    }
    app.mode = TuiMode::Chat;
    super::super::cycle_tab(&mut app, 1);
    assert_ne!(app.active().session_db_id, first);
    super::super::cycle_tab(&mut app, -1);
    assert_eq!(app.active().session_db_id, first);
    assert_eq!((app.input.as_str(), app.cursor), ("first draft", 4));
}

#[tokio::test]
async fn hub_return_from_settings_restarts_cancelled_catalog_discovery() {
    let (_instance, server, backend, _) = fixture().await;
    let mut app = App::hub(HashSet::new());
    let (rows_tx, mut rows_rx) = mpsc::channel(8);
    open_session_picker(&mut app, &server, &rows_tx);
    let cancelled_generation = app.session_generation;
    app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
    // The event loop cancels discovery when the hub is not visible.
    app.cancel_session_fill();
    app.close_settings();
    ensure_view(&mut app, &server, &rows_tx);
    assert!(
        app.session_catalog_loading,
        "return left the catalog unloaded"
    );
    let loading = frame(&mut app, &server, &backend, 80, 24);
    assert!(loading.contains("Loading sessions…"), "{loading}");
    assert!(!loading.contains("No saved sessions"), "{loading}");
    assert!(matches!(
        super::super::apply_session_catalog_result(&mut app, cancelled_generation, Ok(Vec::new())),
        super::super::CatalogLoadOutcome::Ignored
    ));
    let load = tokio::time::timeout(std::time::Duration::from_secs(5), rows_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let SessionLoad::Catalog { generation, result } = load else {
        panic!("expected catalog")
    };
    super::super::apply_session_catalog_result(&mut app, generation, result);
    assert!(app.session_list_fresh);
    assert_eq!(app.session_list.len(), session_ids(&server).await.len());
    assert!(!app.session_list.is_empty(), "fixture has a stored session");

    // A completed empty catalog stays authoritative on a Settings round trip.
    super::super::apply_session_catalog(&mut app, Vec::new());
    app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
    app.cancel_session_fill();
    app.close_settings();
    ensure_view(&mut app, &server, &rows_tx);
    assert!(!app.session_catalog_loading);
    assert!(app.session_list_fresh);
}

#[tokio::test]
async fn description_edit_patches_the_cached_picker_row_without_renaming() {
    use super::super::{ChatAction, apply_session_catalog, handle_chat_action};
    use chaz_core::commands::Command;

    let (_instance, server, backend, _) = fixture().await;
    let (approval_tx, notify_tx, rows_tx) = channels();
    let models_tx = mpsc::channel(8).0;
    let secrets = SecretStore::new(server.registry().chaz_peer().clone()).await;
    let mut app = App::hub(HashSet::new());
    open_session_picker(&mut app, &server, &rows_tx);
    dispatch_picker_selection(
        "__new__".into(),
        &mut app,
        &server,
        &backend,
        &approval_tx,
        &notify_tx,
    );
    settle(&mut app).await;
    let id = app.active().session_db_id.clone();
    server
        .registry()
        .set_session_name(&id, "planning".into())
        .await
        .unwrap();

    // Seed the picker cache with this session's loaded (description-free) row.
    let index = server
        .registry()
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .find(|index| index.session_db_id == id)
        .unwrap();
    apply_session_catalog(&mut app, vec![index.clone()]);
    app.session_list[0] = chaz_core::commands::try_load_session_metadata(server.registry(), index)
        .await
        .unwrap();
    assert_eq!(app.session_list[0].description, None);
    app.mode = TuiMode::Chat;

    let run = async |app: &mut App, cmd: Command| {
        handle_chat_action(
            ChatAction::Dispatch(cmd),
            app,
            &server,
            &backend,
            &secrets,
            &approval_tx,
            &notify_tx,
            &models_tx,
            &rows_tx,
        );
        settle(app).await;
    };
    let canonical = async || {
        let (_, db) = server.registry().open_session(&id).await.unwrap();
        chaz_core::session::try_read_meta_from_db(&db)
            .await
            .unwrap()
    };

    run(
        &mut app,
        Command::DescribeSession("Q3 budget with finance".into()),
    )
    .await;
    // The cached row is patched in place (no catalog reload) and keeps its name.
    assert!(app.session_list_fresh);
    assert_eq!(
        app.session_list[0].description.as_deref(),
        Some("Q3 budget with finance")
    );
    assert_eq!(app.session_list[0].name.as_deref(), Some("planning"));
    let meta = canonical().await;
    assert_eq!(meta.description.as_deref(), Some("Q3 budget with finance"));
    assert_eq!(meta.name.as_deref(), Some("planning"));
    assert!(
        app.active()
            .entries
            .iter()
            .any(|e| e.content == "Session description set: Q3 budget with finance")
    );

    open_session_picker(&mut app, &server, &rows_tx);
    let screen = frame(&mut app, &server, &backend, 80, 24);
    assert!(screen.contains("\"planning\""), "{screen}");
    assert!(screen.contains("    Q3 budget with finance"), "{screen}");
    // A narrow pane truncates the description line rather than wrapping it.
    let narrow = frame(&mut app, &server, &backend, 20, 8);
    assert!(narrow.contains("    Q3 budget wit…│"), "{narrow}");
    app.mode = TuiMode::Chat;

    // A refused edit leaves both the cached row and stored value unchanged.
    run(&mut app, Command::DescribeSession("two\nlines".into())).await;
    assert_eq!(
        app.session_list[0].description.as_deref(),
        Some("Q3 budget with finance")
    );
    assert_eq!(
        canonical().await.description.as_deref(),
        Some("Q3 budget with finance")
    );

    run(&mut app, Command::ClearSessionDescription).await;
    assert_eq!(app.session_list[0].description, None);
    assert_eq!(app.session_list[0].name.as_deref(), Some("planning"));
    let meta = canonical().await;
    assert_eq!(meta.description, None);
    assert_eq!(meta.name.as_deref(), Some("planning"));
    open_session_picker(&mut app, &server, &rows_tx);
    let screen = frame(&mut app, &server, &backend, 80, 24);
    assert!(!screen.contains("Q3 budget"), "{screen}");
    app.mode = TuiMode::Chat;

    // Double-width text is clipped by terminal cells and still shows the ellipsis.
    run(&mut app, Command::DescribeSession("界".repeat(12))).await;
    open_session_picker(&mut app, &server, &rows_tx);
    let narrow = frame(&mut app, &server, &backend, 20, 8);
    let line = narrow.lines().find(|l| l.contains('界')).expect(&narrow);
    assert!(line.contains('…') && line.ends_with('│'), "{narrow}");
}

#[tokio::test]
async fn session_shortcuts_create_focus_and_retain_the_previous_conversation() {
    use super::super::{ChatAction, Command, handle_chat_action, show_system_msg};

    let (_instance, server, backend, mut app) = fixture().await;
    let (approval, notify, _) = channels();
    let (models, _) = mpsc::channel(4);
    let (rows, _) = mpsc::channel(128);
    let secrets = SecretStore::new(server.registry().chaz_peer().clone()).await;
    let original_id = app.active().session_db_id.clone();
    show_system_msg(&mut app, "retained transcript".into());
    app.input = "unsent draft".into();
    app.cursor = 4;
    app.active_mut().scroll_offset = 5;
    let before = session_ids(&server).await;
    let new_key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
    let action = input::handle_chat_key(&mut app, new_key).unwrap();
    assert!(matches!(
        action,
        ChatAction::Dispatch(Command::NewSession(None))
    ));
    assert!(!app.expand_all, "Ctrl+T still toggles expansion");
    handle_chat_action(
        action, &mut app, &server, &backend, &secrets, &approval, &notify, &models, &rows,
    );
    settle(&mut app).await;
    assert_eq!(app.mode, TuiMode::Chat);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.active_tab, 1);
    assert_ne!(app.active().session_db_id, original_id);
    assert!(app.input.is_empty());
    assert_eq!(session_ids(&server).await.len(), before.len() + 1);
    let screen = frame(&mut app, &server, &backend, 160, 30);
    assert!(screen.contains("Ctrl+T new"), "{screen}");
    assert!(!screen.contains("retained transcript"), "{screen}");
    super::super::cycle_tab(&mut app, -1);
    assert_eq!(app.active().session_db_id, original_id);
    assert_eq!((app.input.as_str(), app.cursor), ("unsent draft", 4));
    assert_eq!(app.active().scroll_offset, 5);
    let entries = app.active().entries.clone();
    app.active_mut().scroll_offset = 0;
    assert!(frame(&mut app, &server, &backend, 160, 30).contains("retained transcript"));

    let expand_key = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
    for expanded in [true, false] {
        assert!(input::handle_chat_key(&mut app, expand_key).is_none());
        assert_eq!(app.expand_all, expanded);
        let old_key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(input::handle_chat_key(&mut app, old_key).is_none());
        assert_eq!(app.expand_all, expanded, "Ctrl+R still toggles expansion");
        let screen = frame(&mut app, &server, &backend, 160, 30);
        assert_eq!(screen.contains(" | EXP"), expanded, "{screen}");
    }
    assert_eq!(app.active().entries.len(), entries.len());
    assert_eq!(app.active().entries[0].content, entries[0].content);
    assert_eq!((app.input.as_str(), app.cursor), ("unsent draft", 4));

    app.mode = TuiMode::SessionPicker;
    let selected = input::handle_picker_key(&mut app, new_key).unwrap();
    assert_eq!(selected, "__new__");
    dispatch_picker_selection(selected, &mut app, &server, &backend, &approval, &notify);
    settle(&mut app).await;
    assert_eq!(app.mode, TuiMode::Chat);
    assert_eq!(app.tabs.len(), 3);
    assert_eq!(app.active_tab, 2);
    assert_eq!(session_ids(&server).await.len(), before.len() + 2);
}

#[tokio::test]
async fn session_shortcuts_respect_modes_and_modals() {
    use super::super::{SettingsPrompt, SettingsPromptIntent};

    let (_instance, server, backend, mut app) = fixture().await;
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    for overlay in [
        Overlay::Help { scroll: 0 },
        Overlay::RenamePrompt {
            session_db_id: app.active().session_db_id.clone(),
            title: "Rename".into(),
            input: "draft".into(),
            cursor: 5,
        },
    ] {
        app.overlay = Some(overlay);
        for c in ['t', 'r', 'o'] {
            assert!(matches!(
                input::handle_overlay_key(&mut app, ctrl(c)),
                input::OverlayKey::Consumed
            ));
        }
        assert!(app.overlay.is_some());
        assert!(!app.expand_all);
        if let Some(Overlay::RenamePrompt { input, .. }) = &app.overlay {
            assert_eq!(input, "draft");
        }
    }
    app.overlay = None;
    let (decision_tx, mut decision_rx) = tokio::sync::oneshot::channel();
    app.active_mut().pending_approval = Some(chaz_core::bridge::ApprovalExchange {
        info: chaz_core::tool::ToolApprovalInfo {
            name: "test".into(),
            arguments_display: "{}".into(),
            risk_level: chaz_core::tool::RiskLevel::High,
        },
        decision_tx,
    });
    for c in ['t', 'r', 'o'] {
        assert!(input::handle_chat_key(&mut app, ctrl(c)).is_none());
        assert!(app.has_pending_approval());
    }
    assert!(!app.expand_all);
    assert!(decision_rx.try_recv().is_err());
    app.active_mut().pending_approval = None;

    app.mode = TuiMode::ModelPicker;
    app.model_picker_loading = false;
    assert!(matches!(
        input::handle_model_picker_key(&mut app, ctrl('r')),
        input::ModelPickerKey::Refresh
    ));
    for c in ['t', 'o'] {
        assert!(matches!(
            input::handle_model_picker_key(&mut app, ctrl(c)),
            input::ModelPickerKey::None
        ));
    }
    assert!(app.model_search.is_empty());

    app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
    frame(&mut app, &server, &backend, 120, 40);
    assert!(matches!(
        input::handle_settings_key(&mut app, key(KeyCode::Char('r')), SettingsScope::Peer),
        input::SettingsKey::OpenAgentDiff { .. }
    ));
    app.settings_prompt = Some(SettingsPrompt {
        label: "Add default".into(),
        input: "draft".into(),
        cursor: 5,
        intent: SettingsPromptIntent::AddPeerDefault,
    });
    for c in ['t', 'r', 'o'] {
        assert!(matches!(
            input::handle_settings_key(&mut app, ctrl(c), SettingsScope::Peer),
            input::SettingsKey::None
        ));
    }
    assert_eq!(app.settings_prompt.as_ref().unwrap().input, "draft");
    input::handle_settings_key(&mut app, key(KeyCode::Char('r')), SettingsScope::Peer);
    assert_eq!(app.settings_prompt.as_ref().unwrap().input, "draftr");
    app.settings_prompt = None;

    app.mode = TuiMode::SessionPicker;
    super::super::apply_session_catalog(&mut app, server.registry().list_sessions().await.unwrap());
    app.picker_index = 1;
    for c in ['r', 'o'] {
        assert!(input::handle_picker_key(&mut app, ctrl(c)).is_none());
        assert!(app.overlay.is_none());
    }
    input::handle_picker_key(&mut app, key(KeyCode::Char('r')));
    assert!(matches!(app.overlay, Some(Overlay::RenamePrompt { .. })));
    assert_eq!(app.tabs.len(), 1);
    assert!(!app.expand_all);
    assert!(
        input::command_catalog()
            .iter()
            .any(|(cmd, hint)| *cmd == "/expand" && hint.contains("Ctrl+O"))
    );
}
