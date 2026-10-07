//! Full Settings frame/event checks; fixtures never submit a turn or fetch models.
use super::super::{SettingsFocus, SettingsPrompt, SettingsPromptIntent, input};
use super::list_tests::{draw, fixture, send_mouse, settings_key as key, settings_picker};
use super::*;
use chaz_core::agent::Agent;
use chaz_core::agent_db::{AgentDbConfig, WorkerDbConfig};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

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

fn approval(app: &mut App) -> tokio::sync::oneshot::Receiver<chaz_core::bridge::ApprovalDecision> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.active_mut().pending_approval = Some(chaz_core::bridge::ApprovalExchange {
        info: chaz_core::tool::ToolApprovalInfo {
            name: "review_tool".into(),
            arguments_display: "fixture arguments".into(),
            risk_level: chaz_core::tool::RiskLevel::High,
        },
        decision_tx: tx,
    });
    rx
}

async fn deny(
    app: &mut App,
    by_mouse: bool,
    rx: tokio::sync::oneshot::Receiver<chaz_core::bridge::ApprovalDecision>,
) {
    if by_mouse {
        let hit = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ApprovalDeny))
            .unwrap();
        send_mouse(app, MouseEventKind::Down(MouseButton::Left), hit.x, hit.y);
    } else {
        key(app, KeyCode::Char('n'));
    }
    assert!(
        app.active().pending_approval.is_none(),
        "approval owns input"
    );
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap(),
        chaz_core::bridge::ApprovalDecision::Deny
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
            send_mouse(
                &mut app,
                MouseEventKind::ScrollDown,
                body.right() - 1,
                body.bottom() - 1,
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
    for (width, height) in [(40, 8), (64, 14), (80, 24), (120, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        for (cursor, sentinel) in [(0, "FINAL_TOOL_39"), (1, "FINAL_ERROR")] {
            app.peer_mcp_cursor = cursor;
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert!(!contains(&rows, sentinel));
            app.settings_focus = SettingsFocus::Content;
            draw(&mut terminal, &mut app, &server, &backend);
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
    use SettingsFocus::{Category, Content, List};
    let (_instance, server, backend, mut app) = fixture().await;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    for scope in [SettingsScope::Peer, SettingsScope::Session] {
        for category in 0..app.settings_category_count(scope) {
            open(&mut app, scope, category);
            draw(&mut terminal, &mut app, &server, &backend);
            let list = match scope {
                SettingsScope::Peer => [0, 2, 5].contains(&category),
                SettingsScope::Session => [1, 2].contains(&category),
            };
            let body = !list || (scope == SettingsScope::Peer && [0, 5].contains(&category));
            let entry = if list { List } else { Content };
            for (code, expected) in [
                (KeyCode::Right, entry),
                (KeyCode::Right, if body { Content } else { List }),
                (KeyCode::Left, if list && body { List } else { Category }),
                (KeyCode::Left, Category),
            ] {
                key(&mut app, code);
                assert_eq!(
                    app.settings_focus, expected,
                    "{scope:?} category={category} key={code:?}"
                );
            }
            let _ = key(&mut app, KeyCode::Enter);
            if !(scope == SettingsScope::Session && category == 2) {
                assert_eq!(
                    app.settings_focus, entry,
                    "{scope:?} category={category} Enter"
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
        matches!(outcome, input::SettingsKey::EditPeerDefaults(super::super::DefaultsEdit::Move { name, neighbor }) if name == "agent-04" && neighbor == "agent-03")
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
        send_mouse(&mut app, MouseEventKind::ScrollDown, x, y);
        assert_eq!(app.peer_agents_cursor, 0);
        assert_eq!(
            app.settings_focus,
            SettingsFocus::Category,
            "wheel does not focus"
        );
    }
    assert_eq!(app.settings_reader.offset, 6);
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        body.right() - 1,
        body.bottom() - 1,
    );
    assert_eq!(app.settings_focus, SettingsFocus::Content);
    assert_eq!(app.settings_reader.offset, 6);
    // Every cell outside list/body, plus off-frame coordinates, is swallowed.
    for (x, y) in (0..=80).flat_map(|x| (0..=24).map(move |y| (x, y))) {
        let point = ratatui::layout::Position::new(x, y);
        if !body.contains(point) && !list.contains(point) {
            send_mouse(&mut app, MouseEventKind::ScrollDown, x, y);
            assert_eq!(app.peer_agents_cursor, 0);
            assert_eq!(app.settings_reader.offset, 6);
        }
    }
    send_mouse(&mut app, MouseEventKind::ScrollDown, list.x, list.y);
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
    send_mouse(&mut app, MouseEventKind::ScrollDown, hit.x, hit.y + 1);
    assert_eq!(app.peer_agents_cursor, 6);
    draw(&mut terminal, &mut app, &server, &backend);
    for _ in 0..20 {
        send_mouse(
            &mut app,
            MouseEventKind::ScrollDown,
            list.right() - 1,
            list.bottom() - 1,
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
    // A live receiver: a dropped one means the executor already settled it.
    let mut _approval_rx = None;
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
                app.settings_picker = Some({
                    let mut picker = settings_picker(
                        "add",
                        (0..10).map(|i| format!("candidate-{i}")).collect(),
                        0,
                    ); // Simulate a stale region from a previously visible picker.
                    picker.viewport = list;
                    picker
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
                let (tx, rx) = tokio::sync::oneshot::channel();
                _approval_rx = Some(rx);
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
        send_mouse(&mut app, MouseEventKind::ScrollDown, list.x, list.y);
        if modal == 2 {
            let picker = app.settings_picker.as_ref().unwrap();
            assert_eq!(
                picker.viewport,
                Rect::default(),
                "off-page picker is hidden"
            );
            assert_eq!(picker.selected, 0, "hidden picker cannot receive the wheel");
        }
        key(&mut app, KeyCode::Home);
        send_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            list.x,
            list.y,
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
            let rx = approval(&mut app);
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
            send_mouse(&mut app, MouseEventKind::ScrollDown, list.x, list.y);
            send_mouse(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                list.x,
                list.y,
            );
            assert!(app.active().pending_approval.is_some());
            deny(&mut app, by_mouse, rx).await;
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
    let _rx = approval(&mut app);
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
async fn settings_approval_owns_visible_picker_input_and_preserves_filter() {
    let (_instance, server, backend, mut app) = fixture().await;
    open(&mut app, SettingsScope::Session, 1);
    app.settings_focus = SettingsFocus::List;
    app.input = "chat draft".into();
    for (width, height) in [(80, 24), (120, 40)] {
        for by_mouse in [false, true] {
            app.settings_picker = Some({
                let mut picker = settings_picker(
                    "add agent",
                    (0..30).map(|i| format!("candidate-{i:02}")).collect(),
                    29,
                );
                picker.filter = "candidate".into();
                picker.cursor = 9;
                picker
            });
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert!(contains(&rows, "> candidate-29"), "{rows:?}");
            let picker_area = app.settings_picker.as_ref().unwrap().viewport;
            let rx = approval(&mut app);
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert!(contains(&rows, "review_tool") && contains(&rows, "[n] deny"));
            assert_regions(&app, width, height);
            key(&mut app, KeyCode::Home);
            assert!(matches!(
                key(&mut app, KeyCode::Enter),
                input::SettingsKey::None
            ));
            send_mouse(
                &mut app,
                MouseEventKind::ScrollUp,
                picker_area.x,
                picker_area.y,
            );
            deny(&mut app, by_mouse, rx).await;
            let picker = app.settings_picker.as_ref().unwrap();
            assert_eq!(picker.filter, "candidate");
            assert_eq!(picker.cursor, 9);
            assert_eq!(picker.selected, 29);
            assert_eq!(app.settings_focus, SettingsFocus::List);
            assert_eq!(app.session_agents_cursor, 0);
            assert_eq!(app.input, "chat draft");
            assert_eq!(app.mode, TuiMode::Settings(SettingsScope::Session));
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert!(contains(&rows, "> candidate-29"), "{rows:?}");
            key(&mut app, KeyCode::Esc);
            assert!(app.settings_picker.is_none());
        }
    }
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

#[tokio::test]
async fn settings_geometry_boundaries_show_every_category_and_selected_row() {
    use super::super::SettingsLayout;
    let (_instance, server, backend, mut app) = fixture().await;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let sizes: Vec<_> = [39, 40, 41, 63, 64, 65, 80]
        .into_iter()
        .flat_map(|w| [7, 8, 9, 13, 14, 15, 24].map(|h| (w, h)))
        .chain([
            (0, 0),
            (0, 8),
            (40, 0),
            (1, 1),
            (1, 8),
            (40, 1),
            (15, 14),
            (16, 14),
            (17, 14),
        ])
        .collect();
    for scope in [SettingsScope::Peer, SettingsScope::Session] {
        for category in 0..app.settings_category_count(scope) {
            open(&mut app, scope, category);
            app.peer_agents_cursor = 29;
            app.peer_defaults_cursor = 29;
            app.peer_mcp_cursor = 29;
            app.session_agents_cursor = 29;
            app.session_models_cursor = 30;
            let label = match scope {
                SettingsScope::Peer => PeerSettingsCategory::ALL[category].label(),
                SettingsScope::Session => SessionSettingsCategory::ALL[category].label(),
            };
            for &(width, height) in &sizes {
                terminal.backend_mut().resize(width, height);
                terminal.resize(Rect::new(0, 0, width, height)).unwrap();
                app.settings_focus = SettingsFocus::Category;
                let rows = draw(&mut terminal, &mut app, &server, &backend);
                assert_regions(&app, width, height);
                let expected = if width < 40 || height < 8 {
                    SettingsLayout::TooSmall
                } else if width < 64 || height < 14 {
                    SettingsLayout::Compact
                } else {
                    SettingsLayout::Normal
                };
                assert_eq!(app.settings_layout, expected, "{width}×{height}");
                if expected == SettingsLayout::TooSmall {
                    assert!(app.click_regions.is_empty());
                    assert!(
                        app.settings_list_area.is_none() && app.settings_reader.viewport.is_none()
                    );
                    continue;
                }
                assert!(
                    contains(&rows, label),
                    "{scope:?}/{label} {width}×{height}: {rows:?}"
                );
                if expected == SettingsLayout::Compact {
                    assert!(rows[1].contains(&format!(
                        "{label} {}/{}",
                        category + 1,
                        app.settings_category_count(scope)
                    )));
                }
                let list = app.settings_list_area.is_some();
                if list {
                    let hit = app.click_regions.iter().find(|r| matches!(r.target,
                        ClickTarget::SettingsDetailRow(i) if i == if scope == SettingsScope::Session && category == 2 { 30 } else { 29 }
                    )).expect("selected row is visible");
                    let name = if scope == SettingsScope::Peer && category == 5 {
                        "mcp-29"
                    } else {
                        "agent-29"
                    };
                    assert!(
                        rows[hit.y as usize].contains(name) && rows[hit.y as usize].contains('>'),
                        "{rows:?}"
                    );
                    key(&mut app, KeyCode::Right);
                    assert_eq!(app.settings_focus, SettingsFocus::List);
                    key(&mut app, KeyCode::Right);
                } else {
                    key(&mut app, KeyCode::Right);
                    assert_eq!(app.settings_focus, SettingsFocus::Content);
                }
                draw(&mut terminal, &mut app, &server, &backend);
                if app.settings_focus == SettingsFocus::Content {
                    assert!(app.settings_reader.visible_rows >= 1);
                    assert!(app.settings_reader.viewport.is_some());
                    if expected == SettingsLayout::Compact {
                        assert!(app.settings_list_area.is_none());
                        assert!(
                            !app.click_regions
                                .iter()
                                .any(|r| matches!(r.target, ClickTarget::SettingsDetailRow(_)))
                        );
                    }
                }
                assert_regions(&app, width, height);
            }
        }
    }
    server.shutdown().await;
}

#[tokio::test]
async fn settings_compact_empty_missing_and_static_pages_use_full_content_slot() {
    let (_instance, server, backend, mut app) = fixture().await;
    for name in server.agents().names() {
        server.agents().unregister(&name);
    }
    server.set_default_agents(Vec::new());
    server.mcp_registry().shutdown().await;
    for missing in [false, true] {
        app.session_settings_snapshot = (!missing).then(|| super::super::SessionMetaSnapshot {
            session_db_id: app.active().session_db_id.clone(),
            model_pin: None,
            agent_models: Default::default(),
            agents: Vec::new(),
            host_agent_db_id: None,
            created_at: None,
            entry_count: 0,
        });
        for scope in [SettingsScope::Peer, SettingsScope::Session] {
            for category in 0..app.settings_category_count(scope) {
                open(&mut app, scope, category);
                for (width, height) in [(40, 8), (64, 14), (80, 24)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    draw(&mut terminal, &mut app, &server, &backend);
                    // The empty Models page still has its editable Session scope.
                    if scope == SettingsScope::Session && category == 2 && !missing {
                        let hit = *app
                            .click_regions
                            .iter()
                            .find(|r| matches!(r.target, ClickTarget::SettingsDetailRow(0)))
                            .unwrap();
                        assert!(
                            draw(&mut terminal, &mut app, &server, &backend)[hit.y as usize]
                                .contains("> Session")
                        );
                        continue;
                    }
                    key(&mut app, KeyCode::Right);
                    let rows = draw(&mut terminal, &mut app, &server, &backend);
                    assert_eq!(app.settings_focus, SettingsFocus::Content);
                    let body = app.settings_reader.viewport.unwrap();
                    assert!(body.height >= 1);
                    if (width, height) == (40, 8) {
                        assert_eq!(body, Rect::new(0, 3, 40, 4));
                    }
                    assert!(
                        !app.click_regions
                            .iter()
                            .any(|r| matches!(r.target, ClickTarget::SettingsDetailRow(_))),
                        "{rows:?}"
                    );
                    key(&mut app, KeyCode::End);
                    let tail = draw(&mut terminal, &mut app, &server, &backend);
                    let sentinel = match (scope, category) {
                        (SettingsScope::Peer, 0) => "yaml",
                        (SettingsScope::Peer, 2) => "restart.",
                        (SettingsScope::Peer, 8) => "agent groups",
                        (SettingsScope::Peer, 1 | 3 | 5) => "view-only in v1",
                        (SettingsScope::Session, 0) => "agent)",
                        (SettingsScope::Session, 1) if !missing => "selected",
                        _ => "coming soon",
                    };
                    assert!(
                        contains(&tail, sentinel),
                        "missing {sentinel} {scope:?}/{category} {width}×{height}: {tail:?}"
                    );
                    assert_eq!(app.settings_reader.offset, app.settings_reader.max_offset);
                    assert_regions(&app, width, height);
                    key(&mut app, KeyCode::Left);
                    assert_eq!(app.settings_focus, SettingsFocus::Category);
                }
            }
        }
    }
    server.shutdown().await;
}

#[tokio::test]
async fn settings_compact_breadcrumb_keys_and_pane_wheel_ownership() {
    let (_instance, server, backend, mut app) = fixture().await;
    agent(&server, "agent-00", 20, true);
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
    for scope in [SettingsScope::Peer, SettingsScope::Session] {
        open(&mut app, scope, 0);
        let n = app.settings_category_count(scope);
        for category in 0..n {
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert_eq!(app.settings_index(scope), category);
            assert!(rows[1].contains(&format!(" {}/{} ", category + 1, n)));
            let next = *app.click_regions.iter().find(|r| matches!(r.target, ClickTarget::SettingsSidebarItem(i) if i == (category + 1) % n)).unwrap();
            send_mouse(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                next.x,
                next.y,
            );
            assert_eq!(app.settings_focus, SettingsFocus::Category);
        }
        assert_eq!(app.settings_index(scope), 0);
        draw(&mut terminal, &mut app, &server, &backend);
        send_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 1);
        assert_eq!(app.settings_index(scope), n - 1);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.settings_index(scope), 0);
        key(&mut app, KeyCode::BackTab);
        assert_eq!(app.settings_index(scope), n - 1);
        key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.settings_index(scope), 0);
    }
    open(&mut app, SettingsScope::Peer, 0);
    app.active_mut().scroll_offset = 7;
    let rows = draw(&mut terminal, &mut app, &server, &backend);
    assert!(contains(&rows, "20 workers → details"));
    assert!(!contains(&rows, "└ worker-"));
    let list = app.settings_list_area.unwrap();
    assert_eq!(list, Rect::new(0, 3, 40, 4));
    send_mouse(
        &mut app,
        MouseEventKind::ScrollDown,
        list.x,
        list.bottom() - 1,
    );
    assert_eq!(app.peer_agents_cursor, 3);
    send_mouse(&mut app, MouseEventKind::ScrollUp, list.x, list.y);
    assert_eq!(app.peer_agents_cursor, 0);
    key(&mut app, KeyCode::Right);
    draw(&mut terminal, &mut app, &server, &backend);
    let window = app.scroll.peer_agents;
    key(&mut app, KeyCode::Right);
    draw(&mut terminal, &mut app, &server, &backend);
    assert_eq!(app.settings_focus, SettingsFocus::Content);
    assert!(app.settings_list_area.is_none());
    assert_eq!(app.scroll.peer_agents, window);
    let body = app.settings_reader.viewport.unwrap();
    for (x, y) in (0..=40).flat_map(|x| (0..=8).map(move |y| (x, y))) {
        let before = app.settings_reader.offset;
        send_mouse(&mut app, MouseEventKind::ScrollDown, x, y);
        assert_eq!(
            app.settings_reader.offset,
            if body.contains(ratatui::layout::Position::new(x, y)) {
                before.saturating_add(3).min(app.settings_reader.max_offset)
            } else {
                before
            }
        );
        assert_eq!(app.peer_agents_cursor, 0);
    }
    key(&mut app, KeyCode::Home);
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        body.y,
    );
    key(&mut app, KeyCode::PageDown);
    assert_eq!(app.settings_reader.offset, 3);
    key(&mut app, KeyCode::PageUp);
    assert_eq!(app.settings_reader.offset, 0);
    key(&mut app, KeyCode::End);
    let tail = draw(&mut terminal, &mut app, &server, &backend);
    assert!(contains(&tail, "FINAL_WORKER_PROMPT"), "{tail:?}");
    let offset = app.settings_reader.offset;
    key(&mut app, KeyCode::Left);
    let rows = draw(&mut terminal, &mut app, &server, &backend);
    assert!(contains(&rows, "20 workers → details"));
    assert!(app.settings_reader.viewport.is_none());
    assert_eq!(app.settings_reader.offset, offset);
    key(&mut app, KeyCode::Right);
    draw(&mut terminal, &mut app, &server, &backend);
    assert_eq!(app.settings_reader.offset, offset);
    assert_eq!(app.active().scroll_offset, 7);
    server.shutdown().await;
}

#[tokio::test]
async fn settings_resize_through_too_small_retains_reader_selection_and_recovers_tail() {
    let (_instance, server, backend, mut app) = fixture().await;
    agent(&server, "agent-15", 20, true);
    open(&mut app, SettingsScope::Peer, 0);
    app.peer_agents_cursor = 15;
    app.settings_focus = SettingsFocus::Content;
    app.input = "chat draft".into();
    app.active_mut().scroll_offset = 12;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut app, &server, &backend);
    key(&mut app, KeyCode::End);
    for (width, height) in [(40, 8), (39, 7), (120, 40), (80, 24), (0, 0), (64, 14)] {
        let before = app.settings_reader.offset;
        let max_before = app.settings_reader.max_offset;
        terminal.backend_mut().resize(width, height);
        terminal.resize(Rect::new(0, 0, width, height)).unwrap();
        draw(&mut terminal, &mut app, &server, &backend);
        if width < 40 || height < 8 {
            assert!(app.click_regions.is_empty());
            assert!(app.settings_reader.viewport.is_none() && app.settings_list_area.is_none());
            for code in [
                KeyCode::Enter,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Down,
                KeyCode::PageDown,
                KeyCode::Tab,
                KeyCode::Char('r'),
            ] {
                assert!(matches!(key(&mut app, code), input::SettingsKey::None));
            }
            send_mouse(&mut app, MouseEventKind::ScrollDown, 20, 5);
            send_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 20, 5);
            assert_eq!(app.settings_reader.offset, before);
            assert_eq!(app.settings_reader.max_offset, max_before);
        } else {
            assert_eq!(
                app.settings_reader.offset,
                before.min(app.settings_reader.max_offset)
            );
            key(&mut app, KeyCode::End);
            let rows = draw(&mut terminal, &mut app, &server, &backend);
            assert!(contains(&rows, "FINAL_WORKER_PROMPT"), "{rows:?}");
            assert!(contains(&rows, "agent-15"), "pinned identity: {rows:?}");
            key(&mut app, KeyCode::Home);
            assert!(contains(
                &draw(&mut terminal, &mut app, &server, &backend),
                "default model"
            ));
            key(&mut app, KeyCode::End);
        }
        assert_eq!(app.settings_focus, SettingsFocus::Content);
        assert_eq!(app.peer_agents_cursor, 15);
        assert_eq!(app.peer_settings_index, 0);
        assert_eq!(app.input, "chat draft");
        assert_eq!(app.active().scroll_offset, 12);
        assert_regions(&app, width, height);
    }
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.settings_reader.offset, 0);
    server.shutdown().await;
}

#[tokio::test]
async fn settings_too_small_freezes_modals_and_refuses_invisible_submit_or_confirmation() {
    let (_instance, server, backend, mut app) = fixture().await;
    agent(&server, "agent-00", 20, true);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    for modal in 0..7 {
        open(&mut app, SettingsScope::Peer, 0);
        app.settings_focus = SettingsFocus::Content;
        terminal.backend_mut().resize(80, 24);
        terminal.resize(Rect::new(0, 0, 80, 24)).unwrap();
        draw(&mut terminal, &mut app, &server, &backend);
        key(&mut app, KeyCode::End);
        let mut rx = None;
        match modal {
            0 => {
                app.agent_diff = Some(AgentDiffView {
                    agent_name: "agent-00".into(),
                    diff: chaz_core::agent_diff::diff_agent(
                        &AgentDbConfig::default(),
                        &AgentDbConfig::default(),
                    ),
                    mode: AgentDiffMode::ConfirmReseed,
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
                app.set_settings_index(SettingsScope::Session, 1);
                app.mode = TuiMode::Settings(SettingsScope::Session);
                app.settings_picker = Some(settings_picker(
                    "add",
                    (0..30).map(|i| format!("candidate-{i:02}")).collect(),
                    29,
                ));
            }
            3 => {
                app.overlay = Some(Overlay::RenamePrompt {
                    session_db_id: "fixture".into(),
                    title: "Rename".into(),
                    input: "draft".into(),
                    cursor: 5,
                })
            }
            4 => app.overlay = Some(Overlay::Help { scroll: 2 }),
            5 => rx = Some(approval(&mut app)),
            _ => {
                app.model_picker_caller = app.mode;
                app.mode = TuiMode::ModelPicker;
                app.model_search = "draft".into();
                app.model_picker_index = 0;
            }
        }
        // Supported compact modals keep ownership; covered Settings cannot scroll.
        terminal.backend_mut().resize(40, 8);
        terminal.resize(Rect::new(0, 0, 40, 8)).unwrap();
        let rows = draw(&mut terminal, &mut app, &server, &backend);
        assert_regions(&app, 40, 8);
        assert!(app.settings_list_area.is_none() && app.settings_reader.viewport.is_none());
        if modal == 2 {
            assert!(contains(&rows, "> candidate-29"), "{rows:?}");
            let picker = app.settings_picker.as_ref().unwrap().viewport;
            send_mouse(&mut app, MouseEventKind::ScrollUp, picker.x, picker.y);
            assert_eq!(app.settings_picker.as_ref().unwrap().selected, 26);
        } else if modal == 5 {
            assert!(
                contains(&rows, "review_tool") && contains(&rows, "[n] deny"),
                "{rows:?}"
            );
        }
        let offset = app.settings_reader.offset;
        let focus = app.settings_focus;
        let mode = app.mode;
        let picker_index = app.settings_picker.as_ref().map(|p| p.selected);
        terminal.backend_mut().resize(39, 7);
        terminal.resize(Rect::new(0, 0, 39, 7)).unwrap();
        let rows = draw(&mut terminal, &mut app, &server, &backend);
        assert!(contains(&rows, "Too small: need 40×8"), "{rows:?}");
        assert!(app.click_regions.is_empty());
        assert!(app.settings_list_area.is_none() && app.settings_reader.viewport.is_none());
        for code in [
            KeyCode::Enter,
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('r'),
            KeyCode::Char('R'),
            KeyCode::Char('a'),
            KeyCode::Char('d'),
            KeyCode::Char(' '),
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Tab,
            KeyCode::Backspace,
        ] {
            let event = KeyEvent::new(code, KeyModifiers::NONE);
            match modal {
                3 | 4 => assert!(matches!(
                    input::handle_overlay_key(&mut app, event),
                    input::OverlayKey::Consumed
                )),
                6 => assert!(matches!(
                    input::handle_model_picker_key(&mut app, event),
                    input::ModelPickerKey::None
                )),
                _ => assert!(matches!(key(&mut app, code), input::SettingsKey::None)),
            }
        }
        for kind in [
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::Down(MouseButton::Left),
        ] {
            for (x, y) in [(0, 0), (20, 5), (38, 6), (40, 8)] {
                send_mouse(&mut app, kind, x, y);
            }
        }
        assert_eq!(app.settings_reader.offset, offset);
        assert_eq!(app.settings_focus, focus);
        assert_eq!(app.mode, mode);
        assert_eq!(app.peer_agents_cursor, 0);
        assert_eq!(app.active().scroll_offset, 0);
        match modal {
            0 => assert_eq!(
                app.agent_diff.as_ref().unwrap().mode,
                AgentDiffMode::ConfirmReseed
            ),
            1 => assert_eq!(app.settings_prompt.as_ref().unwrap().input, "draft"),
            2 => {
                let picker = app.settings_picker.as_ref().unwrap();
                assert_eq!(picker.filter, "");
                assert_eq!(Some(picker.selected), picker_index);
                assert_eq!(picker.viewport, Rect::default());
            }
            3 => assert!(
                matches!(&app.overlay, Some(Overlay::RenamePrompt { input, cursor: 5, .. }) if input == "draft")
            ),
            4 => assert!(matches!(app.overlay, Some(Overlay::Help { scroll: 2 }))),
            5 => assert!(matches!(
                rx.as_mut().unwrap().try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            )),
            _ => assert_eq!(app.model_search, "draft"),
        }
        // Resizing restores the same owner and draft before safe cancellation.
        terminal.backend_mut().resize(40, 8);
        terminal.resize(Rect::new(0, 0, 40, 8)).unwrap();
        draw(&mut terminal, &mut app, &server, &backend);
        assert_eq!(app.settings_focus, focus);
        terminal.backend_mut().resize(1, 1);
        terminal.resize(Rect::new(0, 0, 1, 1)).unwrap();
        draw(&mut terminal, &mut app, &server, &backend);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        match modal {
            3 | 4 => {
                input::handle_overlay_key(&mut app, esc);
            }
            6 => {
                input::handle_model_picker_key(&mut app, esc);
            }
            _ => {
                key(&mut app, KeyCode::Esc);
            }
        }
        if modal == 0 {
            assert_eq!(app.agent_diff.as_ref().unwrap().mode, AgentDiffMode::View);
            key(&mut app, KeyCode::Esc);
        }
        if modal == 5 {
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(1), rx.unwrap())
                    .await
                    .unwrap()
                    .unwrap(),
                chaz_core::bridge::ApprovalDecision::Deny
            ));
        }
        assert!(
            app.overlay.is_none()
                && app.agent_diff.is_none()
                && app.settings_prompt.is_none()
                && app.settings_picker.is_none()
                && app.active().pending_approval.is_none()
        );
        assert!(
            matches!(app.mode, TuiMode::Settings(_)),
            "Esc cancels owner, not Settings"
        );
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, TuiMode::SessionPicker);
    }
    server.shutdown().await;
}

#[tokio::test]
async fn settings_compact_models_keep_selected_action_scopes() {
    let (_instance, server, backend, mut app) = fixture().await;
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
    open(&mut app, SettingsScope::Session, 2);
    for cursor in [0, 1, 15, 30] {
        app.session_models_cursor = cursor;
        let rows = draw(&mut terminal, &mut app, &server, &backend);
        let hit = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::SettingsDetailRow(i) if i == cursor))
            .unwrap();
        let name = if cursor == 0 {
            "Session".to_string()
        } else {
            format!("agent-{:02}", cursor - 1)
        };
        assert!(rows[hit.y as usize].contains(&name), "{rows:?}");
        send_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            hit.x,
            hit.y,
        );
        assert_eq!(app.settings_focus, SettingsFocus::List);
        assert_eq!(app.session_models_cursor, cursor);
        assert!(matches!(
            key(&mut app, KeyCode::Enter),
            input::SettingsKey::OpenModelPicker(None)
        ));
        // This is the same helper the async picker dispatcher uses for None.
        let scope = super::super::models_scope_from_cursor(&app);
        if cursor == 0 {
            assert!(matches!(scope, super::super::ModelPickerScope::Session));
        } else {
            assert!(matches!(scope, super::super::ModelPickerScope::Agent(n) if n == name));
        }
    }
    open(&mut app, SettingsScope::Peer, 0);
    app.peer_agents_cursor = 29;
    draw(&mut terminal, &mut app, &server, &backend);
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right);
    draw(&mut terminal, &mut app, &server, &backend);
    assert!(
        matches!(key(&mut app, KeyCode::Enter), input::SettingsKey::OpenModelPicker(Some(super::super::ModelPickerScope::AgentGlobal(n))) if n == "agent-29")
    );
    server.shutdown().await;
}

#[tokio::test]
async fn settings_static_populated_unicode_content_reaches_last_fields_after_reflow() {
    let (_instance, server, backend, mut app) = fixture().await;
    let mut configured =
        chaz_core::config::Backend::new(chaz_core::config::BackendType::OpenAICompatible);
    configured.name = Some("fixture".into());
    configured.api_base = Some(format!("{} FINAL_URL", "wide界 e\u{301} ".repeat(200)));
    let config = Config {
        backends: Some(vec![configured]),
        state_dir: Some(format!("{} FINAL_PATH", "wide界 e\u{301} ".repeat(200))),
        agent_groups: Some([("FINAL_GROUP".into(), Vec::new())].into_iter().collect()),
        ..Default::default()
    };
    app.session_settings_snapshot
        .as_mut()
        .unwrap()
        .host_agent_db_id = Some(format!("{} FINAL_HOST", "wide界 e\u{301} ".repeat(200)));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    for (scope, category, sentinel) in [
        (SettingsScope::Peer, 1, "FINAL_URL"),
        (SettingsScope::Peer, 8, "FINAL_GROUP"),
        (SettingsScope::Session, 0, "FINAL_HOST"),
    ] {
        open(&mut app, scope, category);
        app.settings_focus = SettingsFocus::Content;
        for (width, height) in [(80, 24), (40, 8), (64, 14), (41, 9), (80, 24)] {
            terminal.backend_mut().resize(width, height);
            terminal.resize(Rect::new(0, 0, width, height)).unwrap();
            terminal
                .draw(|f| ui(f, &mut app, &server, &backend, &config))
                .unwrap();
            assert!(app.settings_reader.visible_rows > 0);
            assert!(app.settings_reader.max_offset > 0);
            let page = app.settings_reader.visible_rows.saturating_sub(1).max(1);
            key(&mut app, KeyCode::Home);
            let mut found = false;
            for _ in 0..=app.settings_reader.max_offset / page + 1 {
                terminal
                    .draw(|f| ui(f, &mut app, &server, &backend, &config))
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                found |= text.contains(sentinel);
                key(&mut app, KeyCode::PageDown);
            }
            assert!(
                found,
                "{scope:?}/{category} {width}×{height} lost {sentinel}"
            );
            assert_eq!(app.settings_reader.offset, app.settings_reader.max_offset);
            assert_regions(&app, width, height);
        }
    }
    server.shutdown().await;
}
