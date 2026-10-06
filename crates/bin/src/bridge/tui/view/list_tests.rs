//! Frame + input regressions for the cursor lists, including variable-height
//! rows. All Settings cases draw the real sidebar/detail layout.
use super::super::{SessionMetaSnapshot, SettingsFocus, Tab, input};
use super::*;
use chaz_core::agent::{Agent, AgentRegistry};
use chaz_core::agent_db::{AgentDbConfig, WorkerDbConfig};
use chaz_core::hosted_index::HostedIndex;
use chaz_core::security::{LeakDetector, LeakPolicy, SecretStore, SecurityContext};
use chaz_core::session::{AgentRef, SessionRegistry};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use eidetica::{Instance, NewUser, backend::database::InMemory};
use ratatui::{Terminal, backend::TestBackend};
use std::collections::{HashMap, HashSet};

async fn fixture() -> (Instance, Arc<Server>, BackendManager, App) {
    fixture_with_backends(None).await
}

async fn fixture_with_backends(
    backends: Option<Vec<chaz_core::config::Backend>>,
) -> (Instance, Arc<Server>, BackendManager, App) {
    let (instance, user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewUser::passwordless("list-frames"),
    )
    .await
    .unwrap();
    let agents = Arc::new(AgentRegistry::from_config(&Config::default()));
    for i in 0..30 {
        let cfg = AgentDbConfig {
            workers: if i % 3 == 0 {
                vec![WorkerDbConfig {
                    name: format!("worker-{i}"),
                    ..Default::default()
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        agents
            .register(Agent::from_db_config(&format!("agent-{i:02}"), &cfg))
            .unwrap();
    }
    let registry = Arc::new(
        SessionRegistry::new(instance.clone(), user, agents.clone())
            .await
            .unwrap(),
    );
    let (id, db) = registry.create_session(Some("frame-test")).await.unwrap();
    let backend = BackendManager::new(
        &backends,
        SecretStore::new(registry.chaz_peer().clone()).await,
    );
    let mcp = Arc::new(chaz_core::mcp::McpRegistry::new());
    for i in 0..30 {
        mcp.insert_starting(format!("mcp-{i:02}"));
    }
    let server = Server::new(
        registry,
        agents,
        HostedIndex::empty("agent"),
        HostedIndex::empty("bank"),
        HostedIndex::empty("skill_bank"),
        Arc::new(chaz_core::tool::ToolRegistry::new()),
        Arc::new(chaz_core::tool::ToolPolicyRegistry::empty()),
        SecurityContext {
            leak_detector: LeakDetector::new(LeakPolicy::default()),
            auto_approved_tools: HashSet::new(),
            approval_callback: None,
        },
        HashMap::new(),
        Default::default(),
        Arc::new(chaz_core::tool_host::NativeToolHost::new()),
        Arc::new(chaz_core::extension::ExtensionHub::new()),
        backend.clone(),
        mcp,
        None,
    );
    server.set_default_agents((0..30).map(|i| format!("agent-{i:02}")).collect());
    let tab = Tab {
        session_db_id: id.to_string(),
        session_db: db,
        entries: Vec::new(),
        scroll_offset: 0,
        pending_approval: None,
        active_turns: 0,
        current_agent: "agent-00".into(),
        session_name: None,
        effective_model: String::new(),
        roster: Vec::new(),
        context_budget: 0,
        model_pin: None,
        expanded_entries: HashSet::new(),
    };
    let mut app = App::new(HashSet::new(), tab);
    app.session_settings_snapshot = Some(SessionMetaSnapshot {
        session_db_id: app.active().session_db_id.clone(),
        model_pin: None,
        agent_models: HashMap::new(),
        agents: (0..30)
            .map(|i| AgentRef {
                db_id: format!("db-{i}"),
                display_name: format!("agent-{i:02}"),
                home_pubkey: None,
            })
            .collect(),
        host_agent_db_id: None,
        created_at: None,
        entry_count: 0,
    });
    (instance, server, backend, app)
}

fn draw(
    terminal: &mut Terminal<TestBackend>,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
) -> Vec<String> {
    terminal
        .draw(|f| ui(f, app, server, backend, &Config::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[derive(Clone, Copy, Debug)]
enum List {
    Defaults,
    PeerAgents,
    Mcp,
    SessionAgents,
    Models,
}
impl List {
    fn open(self, app: &mut App) {
        let (scope, category) = match self {
            Self::Defaults => (
                SettingsScope::Peer,
                PeerSettingsCategory::ALL
                    .iter()
                    .position(|c| *c == PeerSettingsCategory::Defaults)
                    .unwrap(),
            ),
            Self::PeerAgents => (
                SettingsScope::Peer,
                PeerSettingsCategory::ALL
                    .iter()
                    .position(|c| *c == PeerSettingsCategory::Agents)
                    .unwrap(),
            ),
            Self::Mcp => (
                SettingsScope::Peer,
                PeerSettingsCategory::ALL
                    .iter()
                    .position(|c| *c == PeerSettingsCategory::Mcp)
                    .unwrap(),
            ),
            Self::SessionAgents => (
                SettingsScope::Session,
                SessionSettingsCategory::ALL
                    .iter()
                    .position(|c| *c == SessionSettingsCategory::Agents)
                    .unwrap(),
            ),
            Self::Models => (
                SettingsScope::Session,
                SessionSettingsCategory::ALL
                    .iter()
                    .position(|c| *c == SessionSettingsCategory::Models)
                    .unwrap(),
            ),
        };
        app.mode = TuiMode::Settings(scope);
        app.set_settings_index(scope, category);
        app.settings_focus = SettingsFocus::Detail;
    }
    fn cursor(self, app: &App) -> usize {
        match self {
            Self::Defaults => app.peer_defaults_cursor,
            Self::PeerAgents => app.peer_agents_cursor,
            Self::Mcp => app.peer_mcp_cursor,
            Self::SessionAgents => app.session_agents_cursor,
            Self::Models => app.session_models_cursor,
        }
    }
    fn label(self, row: usize) -> String {
        match self {
            Self::Mcp => format!("mcp-{row:02}"),
            Self::Models if row == 0 => "Session".into(),
            Self::Models => format!("agent-{:02}", row - 1),
            _ => format!("agent-{row:02}"),
        }
    }
}

fn assert_rows(list: List, app: &App, rows: &[String], width: u16) {
    let hits: Vec<_> = app
        .click_regions
        .iter()
        .filter_map(|r| match r.target {
            ClickTarget::SettingsDetailRow(i) => Some((r, i)),
            _ => None,
        })
        .collect();
    assert!(!hits.is_empty(), "{list:?}: {rows:?}");
    for (r, i) in &hits {
        assert!(r.x + r.w <= width && r.y + r.h < rows.len() as u16);
        assert!(
            rows[r.y as usize].contains(&list.label(*i)),
            "{list:?} row {i}: {rows:?}"
        );
    }
    let selected = hits
        .iter()
        .find(|(_, i)| *i == list.cursor(app))
        .expect("selected row must be clickable")
        .0;
    assert!(
        rows[selected.y as usize].contains("> "),
        "{list:?}: {rows:?}"
    );
}

async fn scrolling_case(list: List) {
    let (_instance, server, backend, mut app) = fixture().await;
    list.open(&mut app);
    for width in [80, 36] {
        let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
        draw(&mut terminal, &mut app, &server, &backend);
        for _ in 0..8 {
            let before = list.cursor(&app);
            input::handle_mouse(
                &mut app,
                mouse(
                    MouseEventKind::ScrollDown,
                    super::super::SETTINGS_SIDEBAR_W,
                    4,
                ),
            );
            assert_eq!(
                list.cursor(&app),
                (before + 3).min(if matches!(list, List::Models) { 30 } else { 29 })
            );
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert_rows(list, &app, &rows, width);
        }
        assert!(
            app.click_regions
                .iter()
                .all(|r| !matches!(r.target, ClickTarget::SettingsDetailRow(0)))
        );
        let TuiMode::Settings(scope) = app.mode else {
            unreachable!()
        };
        input::handle_settings_key(
            &mut app,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            scope,
        );
        let rows = draw(&mut terminal, &mut app, &server, &backend);
        assert_rows(list, &app, &rows, width);
        let hit = *app
            .click_regions
            .iter()
            .find(
                |r| matches!(r.target, ClickTarget::SettingsDetailRow(i) if i != list.cursor(&app)),
            )
            .expect("another visible row");
        let ClickTarget::SettingsDetailRow(clicked) = hit.target else {
            unreachable!()
        };
        input::handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), hit.x, hit.y),
        );
        assert_eq!(list.cursor(&app), clicked);
        let rows = draw(&mut terminal, &mut app, &server, &backend);
        assert_rows(list, &app, &rows, width);
        assert_eq!(app.active().scroll_offset, 0);
    }
    server.shutdown().await;
}

#[tokio::test]
async fn defaults_frame_scroll_and_click() {
    scrolling_case(List::Defaults).await;
}
#[tokio::test]
async fn peer_agents_frame_scroll_and_click() {
    scrolling_case(List::PeerAgents).await;
}
#[tokio::test]
async fn mcp_frame_scroll_and_click() {
    scrolling_case(List::Mcp).await;
}
#[tokio::test]
async fn session_agents_frame_scroll_and_click() {
    scrolling_case(List::SessionAgents).await;
}
#[tokio::test]
async fn models_frame_scroll_and_click() {
    scrolling_case(List::Models).await;
}

#[tokio::test]
async fn variable_height_frames_align_workers_and_model_headers() {
    let (_instance, server, backend, mut app) = fixture().await;
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
    List::PeerAgents.open(&mut app);
    app.peer_agents_cursor = 6;
    let rows = draw(&mut terminal, &mut app, &server, &backend);
    assert_rows(List::PeerAgents, &app, &rows, 80);
    let owner = app
        .click_regions
        .iter()
        .find(|r| matches!(r.target, ClickTarget::SettingsDetailRow(6)))
        .unwrap();
    assert!(rows[owner.y as usize + 1].contains("worker-6"));
    assert!(
        !app.click_regions
            .iter()
            .any(|r| matches!(r.target, ClickTarget::SettingsDetailRow(_)) && r.y == owner.y + 1)
    );
    List::Models.open(&mut app);
    app.session_models_cursor = 1;
    let rows = draw(&mut terminal, &mut app, &server, &backend);
    assert_rows(List::Models, &app, &rows, 80);
    let session = app
        .click_regions
        .iter()
        .find(|r| matches!(r.target, ClickTarget::SettingsDetailRow(0)))
        .unwrap();
    let agent = app
        .click_regions
        .iter()
        .find(|r| matches!(r.target, ClickTarget::SettingsDetailRow(1)))
        .unwrap();
    assert_eq!(agent.y - session.y, 4); // resolves-to, blank, per-agent header
    assert!(rows[agent.y as usize - 1].contains("Per-agent overrides"));
    server.shutdown().await;
}

#[tokio::test]
async fn footer_frames_reserve_hints_without_starving_rows() {
    let (_instance, server, backend, mut app) = fixture().await;
    for (list, hint) in [
        (List::Defaults, "Persisted to chaz_peer"),
        (List::SessionAgents, "[a] add agent"),
        (List::Models, "Enter — open picker"),
    ] {
        list.open(&mut app);
        // Full layout consumes two fixed rows; the list has 4 header lines.
        for height in [14, 11, 10, 9, 8, 7] {
            let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert_rows(list, &app, &rows, 80);
            if height
                >= if matches!(list, List::Models) {
                    11
                } else if matches!(list, List::Defaults) {
                    10
                } else {
                    9
                }
            {
                assert!(
                    rows[..rows.len() - 1].iter().any(|r| r.contains(hint)),
                    "{list:?}, height {height}: {rows:?}"
                );
            }
        }
        // Overflow still leaves both the cursor and footer visible.
        for _ in 0..8 {
            input::handle_mouse(
                &mut app,
                mouse(
                    MouseEventKind::ScrollDown,
                    super::super::SETTINGS_SIDEBAR_W,
                    4,
                ),
            );
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        let rows = draw(&mut terminal, &mut app, &server, &backend);
        assert_rows(list, &app, &rows, 80);
        assert!(
            rows[..rows.len() - 1].iter().any(|r| r.contains(hint)),
            "{list:?}: {rows:?}"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn wheel_ownership_stays_with_modal_or_visible_list() {
    use super::super::{SettingsPicker, SettingsPickerIntent};
    let (_instance, server, backend, mut app) = fixture().await;
    List::PeerAgents.open(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
    draw(&mut terminal, &mut app, &server, &backend);
    let wheel = mouse(
        MouseEventKind::ScrollDown,
        super::super::SETTINGS_SIDEBAR_W,
        4,
    );
    input::handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 1, 4));
    assert_eq!(app.peer_agents_cursor, 0, "sidebar is not the list");
    app.overlay = Some(Overlay::Help { scroll: 0 });
    input::handle_mouse(&mut app, wheel);
    assert!(matches!(app.overlay, Some(Overlay::Help { scroll: 3 })));
    assert_eq!(app.peer_agents_cursor, 0);
    app.overlay = Some(Overlay::RenamePrompt {
        session_db_id: "test".into(),
        title: "Rename".into(),
        input: String::new(),
        cursor: 0,
    });
    input::handle_mouse(&mut app, wheel);
    assert_eq!(app.peer_agents_cursor, 0);
    app.overlay = None;
    app.agent_diff = Some(AgentDiffView {
        agent_name: "agent-00".into(),
        diff: chaz_core::agent_diff::diff_agent(
            &AgentDbConfig::default(),
            &AgentDbConfig::default(),
        ),
        mode: AgentDiffMode::View,
        cursor: 0,
        picks: Vec::new(),
    });
    input::handle_mouse(&mut app, wheel);
    assert_eq!(app.peer_agents_cursor, 0, "diff modal hides the list");
    app.agent_diff = None;
    app.settings_picker = Some(SettingsPicker {
        label: "Add".into(),
        filter: String::new(),
        cursor: 0,
        candidates: (0..10).map(|i| format!("agent-{i}")).collect(),
        selected: 0,
        intent: SettingsPickerIntent::AddSessionAgent,
    });
    input::handle_mouse(&mut app, wheel);
    assert_eq!(app.settings_picker.as_ref().unwrap().selected, 3);
    assert_eq!(app.peer_agents_cursor, 0);
    app.settings_picker = None;
    input::handle_mouse(&mut app, wheel);
    assert_eq!(app.peer_agents_cursor, 3);
    assert_eq!(app.active().scroll_offset, 0);
    server.shutdown().await;
}

#[tokio::test]
async fn toolbar_context_reopen_honors_pins_and_refreshes_runtime_budget() {
    use chaz_core::backends::ModelInfo;
    use chaz_core::config::{Backend, BackendType, Model};
    use chaz_core::session::Session;
    let mut configured = Backend::new(BackendType::OpenAICompatible);
    configured.name = Some("primary".into());
    configured.models = Some(vec![Model {
        name: "small-model".into(),
        reasoning: None,
        price_input: None,
        price_output: None,
        price_cache_read: None,
        context_window: Some(64_000),
    }]);
    let mut secondary = Backend::new(BackendType::OpenAICompatible);
    secondary.name = Some("secondary".into());
    let (_instance, server, backend, mut app) =
        fixture_with_backends(Some(vec![configured, secondary])).await;
    let db = app.active().session_db.clone();
    let id = app.active().session_db_id.clone();
    let session = Session::new(chaz_core::types::ConversationId(id.clone()), db.clone()).await;
    session
        .update_meta(|meta| {
            meta.agent_name = Some("agent-00".into());
            meta.model = Some("secondary:large-model".into());
        })
        .await
        .unwrap();
    server
        .cache_model_info(&ModelInfo {
            id: "secondary:large-model".into(),
            context_window: Some(1_050_000),
            ..Default::default()
        })
        .await;
    let tab = super::super::build_tab(&server, &backend, db.clone(), id.clone()).await;
    assert_eq!(tab.effective_model, "large-model");
    assert_eq!(tab.context_budget, 1_050_000);
    assert_eq!(tab.model_pin.as_deref(), Some("secondary:large-model"));
    app.tabs[0] = tab;
    // No session write: a learned-window update must reach the very next frame.
    server
        .cache_model_info(&ModelInfo {
            id: "secondary:large-model".into(),
            context_window: Some(2_000_000),
            ..Default::default()
        })
        .await;
    let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
    let screen = draw(&mut terminal, &mut app, &server, &backend).join("\n");
    assert!(screen.contains("ctx unknown/2000000 tok"), "{screen}");
    server.agents().upsert(Agent::from_db_config(
        "agent-00",
        &AgentDbConfig {
            max_context_tokens: Some(16_000),
            ..Default::default()
        },
    ));
    let screen = draw(&mut terminal, &mut app, &server, &backend).join("\n");
    assert!(screen.contains("ctx unknown/16000 tok"), "{screen}");
    // A per-agent pin wins over the session pin; explicit YAML windows win
    // over learned windows. Neither source is an output-token limit.
    session
        .update_meta(|meta| {
            meta.agent_models
                .insert("agent-00".into(), "primary:small-model".into());
        })
        .await
        .unwrap();
    server
        .cache_model_info(&ModelInfo {
            id: "primary:small-model".into(),
            context_window: Some(1_050_000),
            ..Default::default()
        })
        .await;
    server
        .agents()
        .upsert(Agent::from_db_config("agent-00", &AgentDbConfig::default()));
    let tab = super::super::build_tab(&server, &backend, db.clone(), id.clone()).await;
    assert_eq!(tab.effective_model, "small-model");
    assert_eq!(tab.context_budget, 64_000);
    app.tabs[0] = tab;
    server.agents().upsert(Agent::from_db_config(
        "agent-00",
        &AgentDbConfig {
            model: Some("secondary:large-model".into()),
            ..Default::default()
        },
    ));
    let screen = draw(&mut terminal, &mut app, &server, &backend).join("\n");
    assert!(screen.contains("ctx unknown/64000 tok"), "{screen}");
    // Without session pins, a live agent-default edit must also reach the next
    // frame. AgentSet updates the registry, not the session DB's on_write hook.
    session
        .update_meta(|meta| {
            meta.model = None;
            meta.agent_models.clear();
        })
        .await
        .unwrap();
    app.tabs[0] = super::super::build_tab(&server, &backend, db, id).await;
    let screen = draw(&mut terminal, &mut app, &server, &backend).join("\n");
    assert!(screen.contains("ctx unknown/2000000 tok"), "{screen}");
    server.agents().upsert(Agent::from_db_config(
        "agent-00",
        &AgentDbConfig {
            model: Some("primary:small-model".into()),
            ..Default::default()
        },
    ));
    let screen = draw(&mut terminal, &mut app, &server, &backend).join("\n");
    assert!(screen.contains("ctx unknown/64000 tok"), "{screen}");
    assert_eq!(app.active().effective_model, "small-model");
    server.shutdown().await;
}
