//! Full Settings frame/event checks; fixtures never submit a turn or fetch models.
use super::super::{
    SettingsFocus, SettingsPicker, SettingsPickerIntent, SettingsPrompt, SettingsPromptIntent,
    input,
};
use super::list_tests::{draw, fixture, mouse};
use super::*;
use chaz_core::agent::Agent;
use chaz_core::agent_db::{AgentDbConfig, WorkerDbConfig};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

fn key(app: &mut App, code: KeyCode) -> input::SettingsKey {
    let TuiMode::Settings(scope) = app.mode else {
        panic!("not Settings")
    };
    input::handle_settings_key(app, KeyEvent::new(code, KeyModifiers::NONE), scope)
}

fn open(app: &mut App, scope: SettingsScope, category: usize) {
    app.open_settings(scope, TuiMode::SessionPicker);
    app.mode = TuiMode::Settings(scope);
    app.set_settings_index(scope, category);
    app.settings_focus = SettingsFocus::Category;
}

fn agent(server: &Arc<Server>, name: &str, workers: usize, long: bool) {
    server.agents().upsert(Agent::from_db_config(
        name,
        &AgentDbConfig {
            tools: long.then(|| vec!["wide界 e\u{301} tool ".repeat(80)]),
            system_prompt: "Deliberately abbreviated preview\nNOT_A_FULL_PROMPT_VIEWER".into(),
            workers: (0..workers)
                .map(|i| WorkerDbConfig {
                    name: format!("worker-{i:02}"),
                    model: Some(format!("model-{i:02}")),
                    max_spawn_depth: Some(3),
                    tools: Some(vec![format!("tool-{i:02}")]),
                    system_prompt: if i + 1 == workers {
                        "FINAL_WORKER_PROMPT".into()
                    } else {
                        format!("prompt-{i:02}")
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
    ));
}

fn contains(rows: &[String], text: &str) -> bool {
    rows.iter().any(|r| r.contains(text))
}

fn assert_regions(app: &App, width: u16, height: u16) {
    for r in &app.click_regions {
        assert!(
            r.w > 0 && r.h > 0 && r.x + r.w <= width && r.y + r.h <= height,
            "{r:?}"
        );
    }
    for r in [app.settings_list_area, app.settings_reader.viewport]
        .into_iter()
        .flatten()
    {
        assert!(
            r.width > 0 && r.height > 0 && r.right() <= width && r.bottom() <= height,
            "{r:?}"
        );
    }
}

#[tokio::test]
async fn settings_worker_groups_collapse_only_above_actual_budget() {
    let (_instance, server, backend, mut app) = fixture().await;
    open(&mut app, SettingsScope::Peer, 0);
    for (width, height) in [(80, 24), (120, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        draw(&mut terminal, &mut app, &server, &backend);
        let budget = app.settings_list_area.unwrap().height as usize;
        for group_height in [budget - 1, budget, budget + 1, 21] {
            for cursor in [0, 15, 29] {
                let name = format!("agent-{cursor:02}");
                agent(&server, &name, group_height - 1, false);
                app.peer_agents_cursor = cursor;
                let rows = draw(&mut terminal, &mut app, &server, &backend);
                let hit = app
                    .click_regions
                    .iter()
                    .find(|r| matches!(r.target, ClickTarget::SettingsDetailRow(i) if i == cursor))
                    .unwrap();
                assert!(rows[hit.y as usize].contains(&name), "{rows:?}");
                let list = app.settings_list_area.unwrap();
                if group_height <= budget {
                    assert!(!rows[hit.y as usize].contains("→ details"));
                    assert!(
                        rows[hit.y as usize + group_height - 1]
                            .contains(&format!("worker-{:02}", group_height - 2)),
                        "{rows:?}"
                    );
                    for y in hit.y + 1..hit.y + group_height as u16 {
                        assert!(
                            !app.click_regions
                                .iter()
                                .any(|r| matches!(r.target, ClickTarget::SettingsDetailRow(_))
                                    && r.y == y)
                        );
                    }
                } else {
                    assert!(
                        rows[hit.y as usize]
                            .contains(&format!("[{} workers → details]", group_height - 1)),
                        "{rows:?}"
                    );
                    let next_agent_y = app
                        .click_regions
                        .iter()
                        .filter(|r| {
                            matches!(r.target, ClickTarget::SettingsDetailRow(_)) && r.y > hit.y
                        })
                        .map(|r| r.y)
                        .min()
                        .unwrap_or(list.bottom());
                    assert!(
                        !rows[hit.y as usize..next_agent_y as usize]
                            .iter()
                            .any(|r| r.contains("└ worker-")),
                        "{rows:?}"
                    );
                }
                assert_regions(&app, width, height);
            }
        }
    }
    server.shutdown().await;
}

#[tokio::test]
async fn settings_agent_end_page_and_stationary_wheel_reach_final_worker_fields() {
    let (_instance, server, backend, mut app) = fixture().await;
    agent(&server, "agent-00", 20, true);
    open(&mut app, SettingsScope::Peer, 0);
    app.active_mut().scroll_offset = 9;
    for (width, height) in [(80, 24), (120, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let home = draw(&mut terminal, &mut app, &server, &backend);
        assert!(!contains(&home, "FINAL_WORKER_PROMPT"));
        key(&mut app, KeyCode::Right);
        assert_eq!(app.settings_focus, SettingsFocus::List);
        key(&mut app, KeyCode::Right);
        assert_eq!(app.settings_focus, SettingsFocus::Content);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.settings_reader.offset, 1);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.settings_reader.offset, 0);
        key(&mut app, KeyCode::End);
        let rows = draw(&mut terminal, &mut app, &server, &backend);
        for text in [
            "agent-00",
            "worker-19",
            "model-19",
            "spawn depth",
            "tool-19",
            "FINAL_WORKER_PROMPT",
        ] {
            assert!(contains(&rows, text), "missing {text}: {rows:?}");
        }
        assert_eq!(app.settings_reader.offset, app.settings_reader.max_offset);
        assert!(
            matches!(key(&mut app, KeyCode::Enter), input::SettingsKey::OpenModelPicker(Some(super::super::ModelPickerScope::AgentGlobal(n))) if n == "agent-00")
        );
        assert_eq!(app.peer_agents_cursor, 0);
        assert_eq!(app.peer_settings_index, 0);
        key(&mut app, KeyCode::Home);
        let rows = draw(&mut terminal, &mut app, &server, &backend);
        assert!(contains(&rows, "default model"));
        assert!(!contains(&rows, "NOT_A_FULL_PROMPT_VIEWER"));
        let page = app.settings_reader.visible_rows.saturating_sub(1).max(1);
        key(&mut app, KeyCode::PageDown);
        assert_eq!(app.settings_reader.offset, page);
        key(&mut app, KeyCode::PageUp);
        assert_eq!(app.settings_reader.offset, 0);
        for _ in 0..=app.settings_reader.max_offset / page {
            key(&mut app, KeyCode::PageDown);
            draw(&mut terminal, &mut app, &server, &backend);
        }
        assert!(contains(
            &draw(&mut terminal, &mut app, &server, &backend),
            "FINAL_WORKER_PROMPT"
        ));
        key(&mut app, KeyCode::Home);
        let body = app.settings_reader.viewport.unwrap();
        for _ in 0..=app.settings_reader.max_offset / 3 {
            input::handle_mouse(
                &mut app,
                mouse(
                    MouseEventKind::ScrollDown,
                    body.right() - 1,
                    body.bottom() - 1,
                ),
            );
            draw(&mut terminal, &mut app, &server, &backend);
        }
        assert!(contains(
            &draw(&mut terminal, &mut app, &server, &backend),
            "FINAL_WORKER_PROMPT"
        ));
        assert_eq!(app.peer_agents_cursor, 0);
        assert_eq!(app.active().scroll_offset, 9);
        key(&mut app, KeyCode::Home);
        app.settings_focus = SettingsFocus::Category;
    }
    server.shutdown().await;
}

#[tokio::test]
async fn settings_mcp_native_wrapping_reaches_tools_and_error_tail() {
    let (_instance, server, backend, mut app) = fixture().await;
    // A local, bounded stdio stub populates the real MCP metadata cache.
    let script = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method')
    if method == 'initialize':
        result = {'protocolVersion':'2025-11-25','capabilities':{'tools':{}}}
    elif method == 'tools/list':
        result = {'tools':[{'name': ('tool-%02d' % i) + ('-FINAL_TOOL_39' if i == 39 else ''), 'inputSchema':{'type':'object'}} for i in range(40)]}
    else:
        continue
    print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}), flush=True)
"#;
    let cfg = chaz_core::config::McpServerConfig {
        name: "a-tools".into(),
        command: "python3".into(),
        args: Some(vec!["-u".into(), "-c".into(), script.into()]),
        env: None,
        url: None,
        default_policy: None,
        startup_timeout_secs: 5,
    };
    let mcp = Arc::new(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            chaz_core::mcp::server::McpServer::start(&cfg),
        )
        .await
        .unwrap()
        .unwrap(),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        mcp.discover_and_wrap_tools("a-tools"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(mcp.tool_count(), 40);
    server
        .mcp_registry()
        .insert_running("a-tools".into(), mcp, None);
    server.mcp_registry().insert_failed(
        "b-error".into(),
        format!("{} FINAL_ERROR", "wide界 e\u{301} failure ".repeat(200)),
    );
    open(&mut app, SettingsScope::Peer, 5);
    for (width, height) in [(80, 24), (120, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        for (cursor, sentinel) in [(0, "FINAL_TOOL_39"), (1, "FINAL_ERROR")] {
            app.peer_mcp_cursor = cursor;
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert!(!contains(&rows, sentinel));
            app.settings_focus = SettingsFocus::Content;
            key(&mut app, KeyCode::End);
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert!(contains(&rows, sentinel), "{rows:?}");
            assert_eq!(app.peer_mcp_cursor, cursor);
            assert_eq!(app.peer_settings_index, 5);
            assert_eq!(app.active().scroll_offset, 0);
            assert_regions(&app, width, height);
        }
    }
    server.shutdown().await;
}

#[tokio::test]
async fn settings_focus_ladder_category_keys_and_existing_actions() {
    let (_instance, server, backend, mut app) = fixture().await;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    for scope in [SettingsScope::Peer, SettingsScope::Session] {
        for category in 0..app.settings_category_count(scope) {
            open(&mut app, scope, category);
            draw(&mut terminal, &mut app, &server, &backend);
            key(&mut app, KeyCode::Right);
            let list = match scope {
                SettingsScope::Peer => [0, 2, 5].contains(&category),
                SettingsScope::Session => [1, 2].contains(&category),
            };
            assert_eq!(
                app.settings_focus,
                if list {
                    SettingsFocus::List
                } else {
                    SettingsFocus::Content
                }
            );
            key(&mut app, KeyCode::Right);
            let body = !list || (scope == SettingsScope::Peer && [0, 5].contains(&category));
            assert_eq!(
                app.settings_focus,
                if body {
                    SettingsFocus::Content
                } else {
                    SettingsFocus::List
                }
            );
            key(&mut app, KeyCode::Left);
            assert_eq!(
                app.settings_focus,
                if list && body {
                    SettingsFocus::List
                } else {
                    SettingsFocus::Category
                }
            );
            key(&mut app, KeyCode::Left);
            assert_eq!(app.settings_focus, SettingsFocus::Category);
            let _ = key(&mut app, KeyCode::Enter);
            if !(scope == SettingsScope::Session && category == 2) {
                assert_eq!(
                    app.settings_focus,
                    if list {
                        SettingsFocus::List
                    } else {
                        SettingsFocus::Content
                    }
                );
            }
        }
    }
    open(&mut app, SettingsScope::Peer, 0);
    draw(&mut terminal, &mut app, &server, &backend);
    app.settings_focus = SettingsFocus::List;
    key(&mut app, KeyCode::Up);
    assert_eq!(
        app.peer_agents_cursor, 29,
        "list arrows still wrap agents, not workers"
    );
    key(&mut app, KeyCode::Down);
    assert_eq!(app.peer_agents_cursor, 0);
    key(&mut app, KeyCode::End);
    assert_eq!(
        app.peer_settings_index, 8,
        "Home/End outside Content still select categories"
    );
    key(&mut app, KeyCode::Home);
    assert_eq!(app.peer_settings_index, 0);
    for code in [KeyCode::Tab, KeyCode::BackTab, KeyCode::Char('6')] {
        app.settings_focus = SettingsFocus::Content;
        let before = app.peer_settings_index;
        key(&mut app, code);
        assert_eq!(app.settings_focus, SettingsFocus::Category);
        assert_eq!(
            app.peer_settings_index,
            match code {
                KeyCode::Tab => (before + 1) % 9,
                KeyCode::BackTab => (before + 8) % 9,
                _ => 5,
            }
        );
    }
    open(&mut app, SettingsScope::Session, 2);
    app.session_models_cursor = 10;
    assert!(matches!(
        key(&mut app, KeyCode::Enter),
        input::SettingsKey::OpenModelPicker(None)
    ));
    assert_eq!(app.session_models_cursor, 10);
    open(&mut app, SettingsScope::Peer, 2);
    app.peer_defaults_cursor = 4;
    let outcome = input::handle_settings_key(
        &mut app,
        KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL),
        SettingsScope::Peer,
    );
    assert!(
        matches!(outcome, input::SettingsKey::WritePeerDefaults(names) if names[3] == "agent-04")
    );
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, TuiMode::SessionPicker);
    server.shutdown().await;
}

#[tokio::test]
async fn settings_wheel_and_click_use_visible_rectangles_not_columns() {
    let (_instance, server, backend, mut app) = fixture().await;
    agent(&server, "agent-00", 20, true);
    open(&mut app, SettingsScope::Peer, 0);
    app.active_mut().scroll_offset = 11;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut app, &server, &backend);
    let body = app.settings_reader.viewport.unwrap();
    let list = app.settings_list_area.unwrap();
    for (x, y) in [(body.x, body.y), (body.right() - 1, body.bottom() - 1)] {
        input::handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, x, y));
        assert_eq!(app.peer_agents_cursor, 0);
        assert_eq!(
            app.settings_focus,
            SettingsFocus::Category,
            "wheel does not focus"
        );
    }
    assert_eq!(app.settings_reader.offset, 6);
    input::handle_mouse(
        &mut app,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            body.right() - 1,
            body.bottom() - 1,
        ),
    );
    assert_eq!(app.settings_focus, SettingsFocus::Content);
    assert_eq!(app.settings_reader.offset, 6);
    // Every cell outside list/body, plus off-frame coordinates, is swallowed.
    for (x, y) in (0..=80).flat_map(|x| (0..=24).map(move |y| (x, y))) {
        let point = ratatui::layout::Position::new(x, y);
        if !body.contains(point) && !list.contains(point) {
            input::handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, x, y));
            assert_eq!(app.peer_agents_cursor, 0);
            assert_eq!(app.settings_reader.offset, 6);
        }
    }
    input::handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, list.x, list.y));
    assert_eq!(app.peer_agents_cursor, 3);
    assert_eq!(
        app.settings_reader.offset, 0,
        "selection resets reading, not focus"
    );
    assert_eq!(app.settings_focus, SettingsFocus::Content);
    draw(&mut terminal, &mut app, &server, &backend);
    // Decorative worker line belongs to the agent list but has no entity hit.
    let hit = *app
        .click_regions
        .iter()
        .find(|r| matches!(r.target, ClickTarget::SettingsDetailRow(3)))
        .unwrap();
    assert!(
        !app.click_regions
            .iter()
            .any(|r| r.y == hit.y + 1 && matches!(r.target, ClickTarget::SettingsDetailRow(_)))
    );
    input::handle_mouse(
        &mut app,
        mouse(MouseEventKind::ScrollDown, hit.x, hit.y + 1),
    );
    assert_eq!(app.peer_agents_cursor, 6);
    draw(&mut terminal, &mut app, &server, &backend);
    for _ in 0..20 {
        input::handle_mouse(
            &mut app,
            mouse(
                MouseEventKind::ScrollDown,
                list.right() - 1,
                list.bottom() - 1,
            ),
        );
    }
    assert_eq!(
        app.peer_agents_cursor, 29,
        "wheel clamps rather than wrapping"
    );
    assert_eq!(app.active().scroll_offset, 11);
    assert_regions(&app, 80, 24);
    server.shutdown().await;
}

#[tokio::test]
async fn settings_reader_resize_clamps_and_identity_changes_reset() {
    let (_instance, server, backend, mut app) = fixture().await;
    agent(&server, "agent-00", 20, true);
    open(&mut app, SettingsScope::Peer, 0);
    app.settings_focus = SettingsFocus::Content;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut app, &server, &backend);
    key(&mut app, KeyCode::End);
    for (width, height) in [(120, 40), (80, 24), (40, 10), (120, 40)] {
        let before = app.settings_reader.offset;
        terminal.backend_mut().resize(width, height);
        terminal.resize(Rect::new(0, 0, width, height)).unwrap();
        draw(&mut terminal, &mut app, &server, &backend);
        assert_eq!(
            app.settings_reader.offset,
            before.min(app.settings_reader.max_offset)
        );
        assert_eq!(app.settings_focus, SettingsFocus::Content);
        assert_eq!(app.peer_agents_cursor, 0);
        assert_regions(&app, width, height);
        key(&mut app, KeyCode::End);
        let end = draw(&mut terminal, &mut app, &server, &backend);
        if width >= 64 && height >= 14 {
            assert!(contains(&end, "FINAL_WORKER_PROMPT"), "{end:?}");
        }
    }
    key(&mut app, KeyCode::Left);
    let retained = app.settings_reader.offset;
    key(&mut app, KeyCode::Right);
    assert_eq!(app.settings_reader.offset, retained);
    agent(&server, "agent-00", 0, false);
    let rows = draw(&mut terminal, &mut app, &server, &backend);
    assert_eq!(app.settings_reader.offset, 0, "shrinking content now fits");
    assert!(
        !contains(&rows, "DETAILS ["),
        "fitting decoration is restored"
    );
    agent(&server, "agent-00", 20, true);
    draw(&mut terminal, &mut app, &server, &backend);
    key(&mut app, KeyCode::End);
    server.agents().unregister("agent-00");
    draw(&mut terminal, &mut app, &server, &backend);
    assert_eq!(app.peer_agents_cursor, 0);
    assert_eq!(app.peer_agents_names[0], "agent-01");
    assert_eq!(app.settings_reader.offset, 0, "same index, new identity");
    agent(&server, "agent-01", 20, true);
    draw(&mut terminal, &mut app, &server, &backend);
    key(&mut app, KeyCode::End);
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.settings_reader.offset, 0);
    key(&mut app, KeyCode::BackTab);
    draw(&mut terminal, &mut app, &server, &backend);
    app.settings_focus = SettingsFocus::Content;
    key(&mut app, KeyCode::End);
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.settings_reader.offset, 0);
    open(&mut app, SettingsScope::Peer, 0);
    draw(&mut terminal, &mut app, &server, &backend);
    assert_eq!(app.settings_reader.offset, 0);
    server.shutdown().await;
}

#[tokio::test]
async fn settings_modals_keep_reader_position_and_own_background_input() {
    let (_instance, server, backend, mut app) = fixture().await;
    agent(&server, "agent-00", 20, true);
    open(&mut app, SettingsScope::Peer, 0);
    app.settings_focus = SettingsFocus::Content;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut app, &server, &backend);
    key(&mut app, KeyCode::End);
    draw(&mut terminal, &mut app, &server, &backend);
    let offset = app.settings_reader.offset;
    let list = app.settings_list_area.unwrap();
    for modal in 0..5 {
        match modal {
            0 => {
                app.agent_diff = Some(AgentDiffView {
                    agent_name: "agent-00".into(),
                    diff: chaz_core::agent_diff::diff_agent(
                        &AgentDbConfig::default(),
                        &AgentDbConfig::default(),
                    ),
                    mode: AgentDiffMode::View,
                    cursor: 0,
                    picks: Vec::new(),
                })
            }
            1 => {
                app.settings_prompt = Some(SettingsPrompt {
                    label: "add".into(),
                    input: "draft".into(),
                    cursor: 5,
                    intent: SettingsPromptIntent::AddPeerDefault,
                })
            }
            2 => {
                app.settings_picker = Some(SettingsPicker {
                    label: "add".into(),
                    filter: String::new(),
                    cursor: 0,
                    candidates: (0..10).map(|i| format!("candidate-{i}")).collect(),
                    selected: 0,
                    intent: SettingsPickerIntent::AddSessionAgent,
                })
            }
            3 => {
                app.overlay = Some(Overlay::RenamePrompt {
                    session_db_id: "test".into(),
                    title: "Rename".into(),
                    input: "draft".into(),
                    cursor: 5,
                })
            }
            _ => {
                let (tx, _rx) = tokio::sync::oneshot::channel();
                app.active_mut().pending_approval = Some(chaz_core::bridge::ApprovalExchange {
                    info: chaz_core::tool::ToolApprovalInfo {
                        name: "test".into(),
                        arguments_display: "{}".into(),
                        risk_level: chaz_core::tool::RiskLevel::High,
                    },
                    decision_tx: tx,
                });
            }
        }
        draw(&mut terminal, &mut app, &server, &backend);
        assert!(app.settings_list_area.is_none() && app.settings_reader.viewport.is_none());
        assert!(!app.click_regions.iter().any(|r| matches!(
            r.target,
            ClickTarget::SettingsContent
                | ClickTarget::SettingsDetailRow(_)
                | ClickTarget::SettingsSidebarItem(_)
        )));
        input::handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, list.x, list.y));
        if modal == 2 {
            assert_eq!(app.settings_picker.as_ref().unwrap().selected, 3);
        }
        key(&mut app, KeyCode::Home);
        input::handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), list.x, list.y),
        );
        assert_eq!(app.settings_reader.offset, offset);
        assert_eq!(app.peer_agents_cursor, 0);
        assert_eq!(app.active().scroll_offset, 0);
        if modal == 1 {
            assert_eq!(app.settings_prompt.as_ref().unwrap().input, "draft");
        }
        if modal <= 2 {
            key(&mut app, KeyCode::Esc);
        } else if modal == 4 {
            key(&mut app, KeyCode::Char('n'));
        }
        assert!(
            app.agent_diff.is_none()
                && app.settings_prompt.is_none()
                && app.settings_picker.is_none()
                && app.overlay.is_none()
                && app.active().pending_approval.is_none()
        );
        draw(&mut terminal, &mut app, &server, &backend);
        assert_eq!(app.settings_reader.offset, offset);
    }
    app.model_picker_caller = app.mode;
    app.mode = TuiMode::ModelPicker;
    draw(&mut terminal, &mut app, &server, &backend);
    assert!(app.settings_list_area.is_none() && app.settings_reader.viewport.is_none());
    input::handle_model_picker_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    draw(&mut terminal, &mut app, &server, &backend);
    assert_eq!(app.settings_reader.offset, offset);
    assert_eq!(app.settings_focus, SettingsFocus::Content);
    server.shutdown().await;
}

#[tokio::test]
async fn settings_approval_is_visible_and_denial_preserves_reader_and_draft() {
    let (_instance, server, backend, mut app) = fixture().await;
    agent(&server, "agent-00", 20, true);
    open(&mut app, SettingsScope::Peer, 0);
    app.settings_focus = SettingsFocus::Content;
    for (width, height) in [(80, 24), (120, 40)] {
        for by_mouse in [false, true] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            draw(&mut terminal, &mut app, &server, &backend);
            key(&mut app, KeyCode::End);
            let offset = app.settings_reader.offset;
            let list = app.settings_list_area.unwrap();
            app.settings_prompt = Some(SettingsPrompt {
                label: "add".into(),
                input: "retained draft".into(),
                cursor: 14,
                intent: SettingsPromptIntent::AddPeerDefault,
            });
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.active_mut().pending_approval = Some(chaz_core::bridge::ApprovalExchange {
                info: chaz_core::tool::ToolApprovalInfo {
                    name: "review_tool".into(),
                    arguments_display: "fixture arguments".into(),
                    risk_level: chaz_core::tool::RiskLevel::High,
                },
                decision_tx: tx,
            });
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            for text in [
                "Tool approval",
                "review_tool",
                "fixture arguments",
                "[n] deny",
            ] {
                assert!(
                    contains(&rows, text),
                    "missing visible approval {text}: {rows:?}"
                );
            }
            assert_regions(&app, width, height);
            assert!(app.settings_list_area.is_none() && app.settings_reader.viewport.is_none());
            key(&mut app, KeyCode::Home);
            input::handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, list.x, list.y));
            input::handle_mouse(
                &mut app,
                mouse(MouseEventKind::Down(MouseButton::Left), list.x, list.y),
            );
            assert!(app.active().pending_approval.is_some());
            if by_mouse {
                let deny = *app
                    .click_regions
                    .iter()
                    .find(|r| matches!(r.target, ClickTarget::ApprovalDeny))
                    .unwrap();
                input::handle_mouse(
                    &mut app,
                    mouse(MouseEventKind::Down(MouseButton::Left), deny.x, deny.y),
                );
            } else {
                key(&mut app, KeyCode::Char('n'));
            }
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(1), rx)
                    .await
                    .unwrap()
                    .unwrap(),
                chaz_core::bridge::ApprovalDecision::Deny
            ));
            assert_eq!(
                app.settings_prompt.as_ref().unwrap().input,
                "retained draft"
            );
            assert_eq!(app.settings_reader.offset, offset);
            assert_eq!(app.settings_focus, SettingsFocus::Content);
            assert_eq!(app.peer_agents_cursor, 0);
            assert_eq!(app.mode, TuiMode::Settings(SettingsScope::Peer));
            key(&mut app, KeyCode::Esc);
            draw(&mut terminal, &mut app, &server, &backend);
            assert!(app.settings_reader.viewport.is_some());
            assert_eq!(app.settings_reader.offset, offset);
        }
    }
    let (tx, _rx) = tokio::sync::oneshot::channel();
    app.active_mut().pending_approval = Some(chaz_core::bridge::ApprovalExchange {
        info: chaz_core::tool::ToolApprovalInfo {
            name: "review_tool".into(),
            arguments_display: "fixture arguments".into(),
            risk_level: chaz_core::tool::RiskLevel::High,
        },
        decision_tx: tx,
    });
    for (width, height) in [
        (0, 0),
        (1, 1),
        (15, 7),
        (16, 8),
        (17, 13),
        (40, 8),
        (64, 14),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        draw(&mut terminal, &mut app, &server, &backend);
        assert_regions(&app, width, height);
    }
    key(&mut app, KeyCode::Char('n'));
    server.shutdown().await;
}

#[tokio::test]
async fn settings_static_empty_and_degenerate_frames_have_safe_regions() {
    let (_instance, server, backend, mut app) = fixture().await;
    // Missing snapshots use the same read-only placeholder path.
    app.session_settings_snapshot = None;
    for scope in [SettingsScope::Peer, SettingsScope::Session] {
        for category in 0..app.settings_category_count(scope) {
            open(&mut app, scope, category);
            for (width, height) in [
                (0, 0),
                (1, 1),
                (15, 7),
                (16, 8),
                (17, 13),
                (40, 8),
                (80, 24),
                (120, 40),
            ] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                draw(&mut terminal, &mut app, &server, &backend);
                assert_regions(&app, width, height);
            }
        }
    }
    server.mcp_registry().shutdown().await;
    for name in server.agents().names() {
        server.agents().unregister(&name);
    }
    server.set_default_agents(Vec::new());
    app.session_settings_snapshot = Some(super::super::SessionMetaSnapshot {
        session_db_id: app.active().session_db_id.clone(),
        model_pin: None,
        agent_models: Default::default(),
        agents: Vec::new(),
        host_agent_db_id: None,
        created_at: None,
        entry_count: 0,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    for (scope, category) in [
        (SettingsScope::Peer, 0),
        (SettingsScope::Peer, 2),
        (SettingsScope::Peer, 5),
        (SettingsScope::Session, 1),
    ] {
        open(&mut app, scope, category);
        draw(&mut terminal, &mut app, &server, &backend);
        key(&mut app, KeyCode::Right);
        assert_eq!(app.settings_focus, SettingsFocus::Content);
        assert!(app.settings_reader.viewport.is_some());
        assert!(
            !app.click_regions
                .iter()
                .any(|r| matches!(r.target, ClickTarget::SettingsDetailRow(_)))
        );
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Left);
        assert_eq!(app.settings_focus, SettingsFocus::Category);
    }
    server.shutdown().await;
}
