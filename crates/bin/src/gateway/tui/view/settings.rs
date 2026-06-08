//! Settings-page rendering for the TUI. Extracted from `view/mod.rs`.
//!
//! Pure functions over the shared widget primitives: a category sidebar
//! plus per-category Peer/Session detail renderers. Child module of `view`
//! so it can use the parent's render helpers via `use super::*`.

use super::*;

/// Stage 1+ Settings page — sidebar of categories + per-category detail.
/// Composition style A (pure functions over the shared widget primitives).
/// Each category routes to its own renderer; categories that haven't been
/// implemented yet fall through to the `(coming soon)` placeholder.
pub(super) fn ui_settings(
    f: &mut ratatui::Frame,
    app: &mut App,
    scope: SettingsScope,
    server: &Arc<Server>,
    backend: &BackendManager,
    config: &Config,
) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(1),    // sidebar + detail
        Constraint::Length(1), // status strip
    ])
    .split(f.area());

    let (title, subtitle): (&str, Option<String>) = match scope {
        SettingsScope::Peer => ("Peer Settings", None),
        SettingsScope::Session => ("Session Settings", Some(app.active().title())),
    };

    widgets::header(f, chunks[0], title, subtitle.as_deref(), Some("[Esc back]"));

    let (sidebar_area, detail_area) = widgets::sidebar_detail_layout(chunks[1], 16);
    let selected = app.settings_index(scope);
    let labels: Vec<&str> = match scope {
        SettingsScope::Peer => PeerSettingsCategory::ALL
            .iter()
            .map(|c| c.label())
            .collect(),
        SettingsScope::Session => SessionSettingsCategory::ALL
            .iter()
            .map(|c| c.label())
            .collect(),
    };
    let sidebar_focused = matches!(app.settings_focus, super::super::SettingsFocus::Sidebar);
    widgets::sidebar(f, sidebar_area, &labels, selected, sidebar_focused);
    // Click regions for each sidebar row so users can mouse-switch
    // categories. One terminal line per item, starting at sidebar_area.y.
    for i in 0..labels.len().min(sidebar_area.height as usize) {
        app.click_regions.push(ClickRegion {
            x: sidebar_area.x,
            y: sidebar_area.y + i as u16,
            w: sidebar_area.width,
            h: 1,
            target: ClickTarget::SettingsSidebarItem(i),
        });
    }

    match scope {
        SettingsScope::Peer => {
            let category = PeerSettingsCategory::ALL
                .get(selected)
                .copied()
                .unwrap_or(PeerSettingsCategory::About);
            render_peer_category(f, detail_area, app, category, server, backend, config);
        }
        SettingsScope::Session => {
            let category = SessionSettingsCategory::ALL
                .get(selected)
                .copied()
                .unwrap_or(SessionSettingsCategory::Overview);
            render_session_category(f, detail_area, app, category, server, backend);
        }
    }

    // Bottom strip is normally the status hints; an inline prompt
    // takes over while typing, and a one-shot status message wins over
    // hints when set (until the next nav keypress clears it). When a
    // picker is open (multi-line, rendered inside the detail area), the
    // strip degrades to a static hint reminder.
    match (
        &app.settings_prompt,
        app.settings_picker.is_some(),
        &app.settings_status,
    ) {
        (Some(prompt), _, _) => {
            widgets::inline_edit_prompt(f, chunks[2], &prompt.label, &prompt.input, prompt.cursor)
        }
        (None, true, _) => {
            widgets::status_strip(
                f,
                chunks[2],
                " type to filter · ↑↓ select · enter add · esc cancel ",
            );
        }
        (None, false, Some(msg)) => widgets::status_strip(f, chunks[2], msg),
        (None, false, None) => {
            let hint = settings_status_hint(app, scope);
            widgets::status_strip(f, chunks[2], hint);
        }
    }
}

/// Per-category status hint shown at the bottom of the Settings page.
/// Categories that own action keys advertise them here so users don't
/// have to remember which page exposes which actions. Wording varies
/// by focus so the user sees which keys are live right now.
fn settings_status_hint(app: &App, scope: SettingsScope) -> &'static str {
    let cur = app.settings_index(scope);
    let detail = matches!(app.settings_focus, super::super::SettingsFocus::Detail);
    match scope {
        SettingsScope::Session => match SessionSettingsCategory::ALL.get(cur) {
            Some(SessionSettingsCategory::Agents) => {
                if detail {
                    " ↑↓ select · ← back · [a] add · [d] remove · Esc back "
                } else {
                    " ↑↓/Tab category · → list · [a] add · [d] remove · Esc back "
                }
            }
            Some(SessionSettingsCategory::Models) => {
                " ↑↓/Tab category · Enter open picker · Esc back "
            }
            _ => " ↑↓/Tab category · 1-9 jump · Esc back ",
        },
        SettingsScope::Peer => match PeerSettingsCategory::ALL.get(cur) {
            Some(PeerSettingsCategory::Agents) => {
                if detail {
                    " ↑↓ select · ← back · [r] reload yaml · Esc back "
                } else {
                    " ↑↓/Tab category · → list · [r] reload yaml · Esc back "
                }
            }
            Some(PeerSettingsCategory::Defaults) => {
                if detail {
                    " ↑↓ select · ← back · [a]/[d] · Ctrl+↑↓ reorder · Esc back "
                } else {
                    " ↑↓/Tab category · → list · [a]/[d] · Ctrl+↑↓ reorder · Esc back "
                }
            }
            _ => " ↑↓/Tab category · 1-9 jump · Esc back ",
        },
    }
}

/// Right-pane router for Peer categories. Categories without a real
/// renderer fall through to the `(coming soon)` placeholder so navigation
/// stays linear even on partially-implemented stages.
fn render_peer_category(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &mut App,
    category: PeerSettingsCategory,
    server: &Arc<Server>,
    backend: &BackendManager,
    config: &Config,
) {
    match category {
        PeerSettingsCategory::About => render_peer_about(f, area, server, backend, config),
        PeerSettingsCategory::Agents => render_peer_agents(f, area, app, server, backend),
        PeerSettingsCategory::Backends => render_peer_backends(f, area, backend, config),
        PeerSettingsCategory::Bridges => render_peer_bridges(f, area, config),
        PeerSettingsCategory::Defaults => render_peer_defaults(f, area, app, server),
        PeerSettingsCategory::Mcp => render_peer_mcp(f, area, app),
        _ => render_settings_detail_placeholder(f, area, category.label()),
    }
}

/// Peer → Defaults — ordered editable list of agents auto-attached to
/// every new session. First entry is the routing host. Edits persist
/// to chaz_peer; reads come from the live `peer_defaults` cache that
/// `ui()` refreshes each frame.
fn render_peer_defaults(f: &mut ratatui::Frame, area: Rect, app: &mut App, server: &Arc<Server>) {
    let known: std::collections::HashSet<String> = server.agents().names().into_iter().collect();
    let defaults_count = app.peer_defaults.len();
    let cursor = app
        .peer_defaults_cursor
        .min(defaults_count.saturating_sub(1));

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Default agents", theme::accent_bold()),
            Span::styled(
                format!("    {} configured", app.peer_defaults.len()),
                Style::default().fg(theme::DIM),
            ),
        ]),
        Line::from(vec![Span::styled(
            "  ─────",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(""),
    ];

    if app.peer_defaults.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (none — falls back to first registered agent on new sessions)",
            Style::default().fg(theme::DIM),
        )]));
    } else {
        for (i, name) in app.peer_defaults.iter().enumerate() {
            let is_selected = i == cursor;
            let is_host = i == 0;
            let marker = if is_selected { "> " } else { "  " };
            let host_tag = if is_host { "  [host]" } else { "" };
            let missing_tag = if known.contains(name) {
                ""
            } else {
                "  (unregistered)"
            };
            let style = if is_selected {
                theme::selected()
            } else if !known.contains(name) {
                theme::error()
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(vec![Span::styled(
                format!("{marker}{i:>2}. {name}{host_tag}{missing_tag}"),
                style,
            )]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  [a] add · [d] remove · Ctrl+↑↓ reorder",
        Style::default().fg(theme::DIM),
    )]));
    lines.push(Line::from(vec![Span::styled(
        "  Persisted to chaz_peer; survives restart.",
        Style::default().fg(theme::DIM),
    )]));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);

    // Per-row click regions. Header is 4 lines (blank, title, dashes,
    // blank); each default is one line after that.
    let rows_y0 = area.y.saturating_add(4);
    for i in 0..defaults_count {
        let y = rows_y0.saturating_add(i as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        app.click_regions.push(ClickRegion {
            x: area.x,
            y,
            w: area.width,
            h: 1,
            target: ClickTarget::SettingsDetailRow(i),
        });
    }
}

/// Peer → Backends (read-only). One row per configured backend: name,
/// api_base, configured-model count, known-model count from the manager.
fn render_peer_backends(
    f: &mut ratatui::Frame,
    area: Rect,
    backend: &BackendManager,
    config: &Config,
) {
    let known = backend.list_known_backends();
    let known_models = backend.list_known_models();
    let configured = config.backends.as_deref().unwrap_or(&[]);

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Backends", theme::accent_bold()),
            Span::styled(
                format!("    {} configured", configured.len()),
                Style::default().fg(theme::DIM),
            ),
        ]),
        Line::from(vec![Span::styled(
            "  ─────",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(""),
    ];

    if configured.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (no backends configured)",
            Style::default().fg(theme::DIM),
        )]));
    } else {
        for b in configured {
            let name = b.name.clone().unwrap_or_else(|| "(unnamed)".to_string());
            let api_base = b
                .api_base
                .clone()
                .unwrap_or_else(|| "(backend default)".to_string());
            let configured_models = b.models.as_ref().map(|m| m.len()).unwrap_or(0);
            // Live count: models the BackendManager reports for this backend
            // name. In multi-backend setups the manager prefixes ids; in
            // single-backend setups it doesn't.
            let live_count = if known.len() <= 1 {
                known_models.len()
            } else {
                let prefix = format!("{name}:");
                known_models
                    .iter()
                    .filter(|m| m.starts_with(&prefix))
                    .count()
            };

            lines.push(Line::from(vec![Span::styled(
                format!("  {name}"),
                theme::accent(),
            )]));
            lines.push(about_kv("    api_base", &api_base));
            lines.push(about_kv(
                "    models",
                &format!("{configured_models} configured · {live_count} known"),
            ));
            lines.push(Line::from(""));
        }
    }

    lines.push(Line::from(vec![Span::styled(
        "  (view-only in v1)",
        Style::default().fg(theme::DIM),
    )]));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Peer → Bridges (read-only). v1: tui + cli are always-on; matrix is
/// enabled when a homeserver_url is set in config.
fn render_peer_bridges(f: &mut ratatui::Frame, area: Rect, config: &Config) {
    let matrix_active = !config.homeserver_url.is_empty();
    let matrix_status = if matrix_active {
        format!("active ({})", config.homeserver_url)
    } else {
        "(homeserver_url unset)".to_string()
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled("  Bridges", theme::accent_bold())]),
        Line::from(vec![Span::styled(
            "  ─────",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(""),
        bridge_row("tui", "active"),
        bridge_row("cli", "available (`chaz -p`)"),
        bridge_row("matrix", &matrix_status),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  (view-only in v1)",
            Style::default().fg(theme::DIM),
        )]),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// One row in the bridges list: name in accent, status in white.
fn bridge_row(name: &str, status: &str) -> Line<'static> {
    let active = status == "active" || status.starts_with("active ");
    let status_style = if active {
        theme::accent()
    } else {
        Style::default().fg(theme::DIM)
    };
    Line::from(vec![
        Span::styled(format!("  {name:<10}"), Style::default().fg(Color::White)),
        Span::styled(status.to_string(), status_style),
    ])
}

/// Peer → Agents (read-only). Top half is a one-line-per-agent list with
/// a selection marker; bottom half is the expanded detail of the selected
/// agent. ↑↓ moves the cursor (see `input::handle_settings_key`).
fn render_peer_agents(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
) {
    // Use the per-frame cache populated by `ui()` so this matches what
    // the input handler indexed when computing `[r]`.
    let names = app.peer_agents_names.clone();

    let cursor = app.peer_agents_cursor.min(names.len().saturating_sub(1));

    // List rows take ~1/3 of the right pane, detail gets the rest.
    let list_h = ((area.height as usize / 3).max(3) as u16).min(area.height.saturating_sub(2));
    let chunks =
        Layout::vertical([Constraint::Length(list_h.max(1)), Constraint::Min(1)]).split(area);

    let mut lines: Vec<Line> = Vec::with_capacity(names.len() + 3);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Agents", theme::accent_bold()),
        Span::styled(
            format!("    {} known", names.len()),
            Style::default().fg(theme::DIM),
        ),
    ]));
    lines.push(Line::from(vec![Span::styled(
        "  ─────",
        Style::default().fg(theme::DIM),
    )]));

    // Map each agent index to the line offset where its row lands, so
    // click regions can target the right row even with variable-height
    // worker nesting.
    let mut agent_line_offsets: Vec<u16> = Vec::with_capacity(names.len());

    if names.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (no agents configured)",
            Style::default().fg(theme::DIM),
        )]));
    } else {
        for (i, name) in names.iter().enumerate() {
            let agent = server.agents().get(name);
            let resolved = agent
                .as_ref()
                .map(|a| backend.resolve_model_name(a.default_model.as_deref()))
                .unwrap_or_default();
            let model_label = if resolved.is_empty() {
                "(no model)".to_string()
            } else {
                resolved
            };
            let is_selected = i == cursor;
            let marker = if is_selected { "> " } else { "  " };
            let style = if is_selected {
                theme::selected()
            } else {
                Style::default().fg(Color::White)
            };
            let worker_count = agent.as_ref().map(|a| a.workers.len()).unwrap_or(0);
            let worker_badge = if worker_count == 0 {
                String::new()
            } else {
                format!("  [{worker_count} workers]")
            };
            agent_line_offsets.push(lines.len() as u16);
            lines.push(Line::from(vec![Span::styled(
                format!("{marker}{name:<14}  {model_label}{worker_badge}"),
                style,
            )]));
            // Render Workers as nested decorative rows. Cursor still indexes
            // Agents only — these don't move the selection.
            if let Some(agent) = agent {
                let mut worker_names: Vec<&str> =
                    agent.workers.keys().map(String::as_str).collect();
                worker_names.sort_unstable();
                for wname in worker_names {
                    lines.push(Line::from(vec![Span::styled(
                        format!("      └ {wname}"),
                        Style::default().fg(theme::DIM),
                    )]));
                }
            }
        }
    }

    f.render_widget(Paragraph::new(lines), chunks[0]);

    // Per-agent click regions. Skip rows that fall outside the list pane
    // (the Paragraph clips them too — no point in a hit region you can't
    // see).
    let list_bottom = chunks[0].y.saturating_add(chunks[0].height);
    for (i, offset) in agent_line_offsets.iter().enumerate() {
        let y = chunks[0].y.saturating_add(*offset);
        if y >= list_bottom {
            break;
        }
        app.click_regions.push(ClickRegion {
            x: chunks[0].x,
            y,
            w: chunks[0].width,
            h: 1,
            target: ClickTarget::SettingsDetailRow(i),
        });
    }

    // Detail pane — selected agent's fields, or a hint when no agents.
    let mut detail = names
        .get(cursor)
        .and_then(|n| server.agents().get(n))
        .map(|a| agent_detail_lines(&a))
        .unwrap_or_else(|| {
            vec![Line::from(vec![Span::styled(
                "  (select an agent)",
                Style::default().fg(theme::DIM),
            )])]
        });
    detail.push(Line::from(""));
    detail.push(Line::from(vec![Span::styled(
        "  [r] reload this agent from yaml",
        Style::default().fg(theme::DIM),
    )]));
    f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }), chunks[1]);
}

/// Per-agent detail block — Agent fields then a nested sub-block for each
/// Worker template the Agent owns. Workers inherit the Agent's defaults
/// where their own fields are unset; "(inherit)" is shown so the
/// resolution path is visible.
fn agent_detail_lines(a: &chaz_core::agent::Agent) -> Vec<Line<'static>> {
    let default_model = a.default_model.as_deref().unwrap_or("(backend default)");
    let tools = match &a.allowed_tools {
        None => "all".to_string(),
        Some(v) if v.is_empty() => "(none)".to_string(),
        Some(v) => v.join(", "),
    };
    let prompt_preview = if a.system_prompt.is_empty() {
        "(empty)".to_string()
    } else {
        let first = a
            .system_prompt
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        ellipsize(first.trim(), 80)
    };
    let max_iter = format!("{}", a.max_iterations);
    let autonomous = if a.autonomous { "yes" } else { "no" };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("  {}", a.name),
            theme::accent_bold(),
        )]),
        Line::from(""),
        about_kv("  default model", default_model),
        about_kv("  max iter", &max_iter),
        about_kv("  autonomous", autonomous),
        about_kv("  tools", &tools),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  system prompt",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(vec![Span::styled(
            format!("    {prompt_preview}"),
            Style::default().fg(Color::White),
        )]),
    ];

    // Workers sub-block — each Worker rendered as a small indented detail
    // group. Unset override fields render as "(inherit)" to show the
    // fallback path to the Agent.
    lines.push(Line::from(""));
    let worker_header = if a.workers.is_empty() {
        "  workers  (none)".to_string()
    } else {
        format!("  workers  ({})", a.workers.len())
    };
    lines.push(Line::from(vec![Span::styled(
        worker_header,
        Style::default().fg(theme::DIM),
    )]));

    let mut worker_names: Vec<&str> = a.workers.keys().map(String::as_str).collect();
    worker_names.sort_unstable();
    for wname in worker_names {
        let w = match a.workers.get(wname) {
            Some(w) => w,
            None => continue,
        };
        let w_model = w
            .default_model
            .as_deref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "(inherit)".to_string());
        let w_tools = match &w.allowed_tools {
            None => "(inherit)".to_string(),
            Some(v) if v.is_empty() => "(none)".to_string(),
            Some(v) => v.join(", "),
        };
        let w_max_iter = w
            .max_iterations
            .map(|n| n.to_string())
            .unwrap_or_else(|| "(inherit)".to_string());
        let w_prompt_preview = if w.system_prompt.is_empty() {
            "(inherit)".to_string()
        } else {
            let first = w
                .system_prompt
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            ellipsize(first.trim(), 76)
        };

        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("    └ {wname}"),
            theme::accent_bold(),
        )]));
        lines.push(about_kv("        model", &w_model));
        lines.push(about_kv("        max iter", &w_max_iter));
        lines.push(about_kv("        tools", &w_tools));
        lines.push(about_kv("        prompt", &w_prompt_preview));
    }

    lines
}

/// Peer → MCP (read-only). Top half is a one-line-per-server list with
/// a selection marker; bottom half is the expanded detail of the
/// selected server. Mirrors the Peer → Agents structure. Failed servers
/// surface in red so operators can spot a misconfigured server without
/// digging through logs.
fn render_peer_mcp(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let servers = app.peer_mcp_servers.clone();
    let cursor = app.peer_mcp_cursor.min(servers.len().saturating_sub(1));

    let list_h = ((area.height as usize / 3).max(3) as u16).min(area.height.saturating_sub(2));
    let chunks =
        Layout::vertical([Constraint::Length(list_h.max(1)), Constraint::Min(1)]).split(area);

    let running = servers
        .iter()
        .filter(|e| matches!(e.status, McpServerStatus::Running { .. }))
        .count();
    let failed = servers.len() - running;
    let header_count = if failed == 0 {
        format!("    {running} running")
    } else {
        format!("    {running} running · {failed} failed")
    };

    let mut lines: Vec<Line> = Vec::with_capacity(servers.len() + 3);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  MCP servers", theme::accent_bold()),
        Span::styled(header_count, Style::default().fg(theme::DIM)),
    ]));
    lines.push(Line::from(vec![Span::styled(
        "  ─────",
        Style::default().fg(theme::DIM),
    )]));

    let mut row_offsets: Vec<u16> = Vec::with_capacity(servers.len());

    if servers.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (no MCP servers configured)",
            Style::default().fg(theme::DIM),
        )]));
    } else {
        for (i, entry) in servers.iter().enumerate() {
            let is_selected = i == cursor;
            let marker = if is_selected { "> " } else { "  " };
            let style = if is_selected {
                theme::selected()
            } else if matches!(entry.status, McpServerStatus::Failed { .. }) {
                theme::error()
            } else {
                Style::default().fg(Color::White)
            };
            let suffix = match &entry.status {
                McpServerStatus::Running { server } => mcp_row_suffix(server),
                McpServerStatus::Failed { .. } => "  failed".to_string(),
            };
            row_offsets.push(lines.len() as u16);
            lines.push(Line::from(vec![Span::styled(
                format!("{marker}{name:<14}{suffix}", name = entry.name),
                style,
            )]));
        }
    }

    f.render_widget(Paragraph::new(lines), chunks[0]);

    // Per-server click regions; same clamp pattern as Peer → Agents.
    let list_bottom = chunks[0].y.saturating_add(chunks[0].height);
    for (i, offset) in row_offsets.iter().enumerate() {
        let y = chunks[0].y.saturating_add(*offset);
        if y >= list_bottom {
            break;
        }
        app.click_regions.push(ClickRegion {
            x: chunks[0].x,
            y,
            w: chunks[0].width,
            h: 1,
            target: ClickTarget::SettingsDetailRow(i),
        });
    }

    // Detail pane — selected server's status + capabilities + tools.
    let mut detail = if let Some(entry) = servers.get(cursor) {
        mcp_detail_lines(entry)
    } else {
        vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                "  No MCP servers configured. Add servers via `mcp_servers:` in chaz.yaml \
                 or drop manifests in the directory pointed at by `mcp_server_dir:`.",
                Style::default().fg(theme::DIM),
            )]),
        ]
    };
    detail.push(Line::from(""));
    detail.push(Line::from(vec![Span::styled(
        "  (view-only in v1)",
        Style::default().fg(theme::DIM),
    )]));
    f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }), chunks[1]);
}

/// One-line capability summary for a running server's list row.
/// Tool count comes from the live cache; resources/prompts are shown as
/// presence badges (live counts would need an async call we don't run
/// from a render frame).
fn mcp_row_suffix(server: &chaz_core::mcp::server::McpServer) -> String {
    let caps = server.capabilities();
    let mut badges: Vec<String> = Vec::new();
    if caps.tools {
        badges.push(format!("tools:{}", server.tool_count()));
    }
    if caps.resources {
        badges.push("resources".to_string());
    }
    if caps.prompts {
        badges.push("prompts".to_string());
    }
    if badges.is_empty() {
        "  (no capabilities advertised)".to_string()
    } else {
        format!("  [{}]", badges.join(", "))
    }
}

/// Detail block for a selected MCP server. Running servers show advertised
/// capabilities + cached tool names; failed servers show the start error
/// so operators can act on it without grepping logs.
fn mcp_detail_lines(entry: &McpRegistryEntry) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        format!("  {}", entry.name),
        theme::accent_bold(),
    )]));
    lines.push(Line::from(""));

    match &entry.status {
        McpServerStatus::Running { server } => {
            let caps = server.capabilities();
            let tool_count = server.tool_count();
            let tools_value = if caps.tools {
                format!("supported · {tool_count} cached")
            } else {
                "not supported".to_string()
            };
            let resources_value = if caps.resources {
                "supported"
            } else {
                "not supported"
            };
            let prompts_value = if caps.prompts {
                "supported"
            } else {
                "not supported"
            };
            lines.push(about_kv("    status", "running"));
            lines.push(about_kv("    tools", &tools_value));
            lines.push(about_kv("    resources", resources_value));
            lines.push(about_kv("    prompts", prompts_value));

            let names = server.tool_names();
            if !names.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![Span::styled(
                    "    Tools",
                    Style::default().fg(theme::DIM),
                )]));
                for n in names {
                    lines.push(Line::from(vec![Span::styled(
                        format!("      · {n}"),
                        Style::default().fg(Color::White),
                    )]));
                }
            }
        }
        McpServerStatus::Failed { error } => {
            lines.push(about_kv("    status", "failed"));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "    Error",
                Style::default().fg(theme::DIM),
            )]));
            lines.push(Line::from(vec![Span::styled(
                format!("      {error}"),
                theme::error(),
            )]));
        }
    }

    lines
}

/// Static peer info — version, paths, env summary. Pure read; refresh is
/// implicit per-frame.
fn render_peer_about(
    f: &mut ratatui::Frame,
    area: Rect,
    server: &Arc<Server>,
    backend: &BackendManager,
    config: &Config,
) {
    let version = env!("CARGO_PKG_VERSION");
    let state_dir = config
        .state_dir
        .as_deref()
        .unwrap_or("~/.local/share/chaz (default)");

    let agent_count = server.agents().names().len();
    let backend_count = backend.list_known_backends().len();
    let model_count = backend.list_known_models().len();
    let default_agents = config
        .default_agents
        .as_ref()
        .map(|v| v.join(", "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none — falls back to first agent)".to_string());

    // Matrix is "enabled" when a homeserver_url has been configured. Other
    // bridges (CLI, TUI) are always wired in chaz.
    let matrix_enabled = !config.homeserver_url.is_empty();
    let bridges = if matrix_enabled {
        "tui, cli, matrix"
    } else {
        "tui, cli"
    };

    let agent_count_s = agent_count.to_string();
    let backend_s = format!("{backend_count} ({model_count} known models)");
    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled("  About", theme::accent_bold())]),
        Line::from(vec![Span::styled(
            "  ─────",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(""),
        about_kv("  version", version),
        about_kv("  state dir", state_dir),
        about_kv("  bridges", bridges),
        Line::from(""),
        about_kv("  agents", &agent_count_s),
        about_kv("  backends", &backend_s),
        about_kv("  default agents", &default_agents),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

/// Single labelled row used by the About / agent-detail panes. Owns its
/// content (returns a `'static` line) so callers can build a vec of these
/// without lifetime games with their local Strings.
fn about_kv(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<18}"), Style::default().fg(theme::DIM)),
        Span::styled(value.to_string(), Style::default().fg(Color::White)),
    ])
}

/// Right-pane router for Session categories. Reads the seeded snapshot
/// from `app.session_settings_snapshot`; falls through to placeholder when
/// no snapshot is present (shouldn't happen in normal flow — page is only
/// reachable after seed_session_settings_snapshot ran).
fn render_session_category(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &mut App,
    category: SessionSettingsCategory,
    _server: &Arc<Server>,
    backend: &BackendManager,
) {
    match category {
        SessionSettingsCategory::Overview => render_session_overview(f, area, app),
        SessionSettingsCategory::Models => render_session_models(f, area, app, backend),
        SessionSettingsCategory::Agents => render_session_agents(f, area, app, backend),
        _ => render_settings_detail_placeholder(f, area, category.label()),
    }
}

/// Session → Agents — `meta.agents` list with [a]/[d] add/remove via the
/// bottom-strip prompt and direct dispatch respectively.
fn render_session_agents(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &mut App,
    backend: &BackendManager,
) {
    // If a picker is open, carve the bottom of the detail area for it.
    // The agents list renders into the top region.
    let (list_area, picker_area) = if app.settings_picker.is_some() {
        let picker_h = 8u16.min(area.height.saturating_sub(1)).max(3);
        let list_h = area.height.saturating_sub(picker_h);
        (
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: list_h,
            },
            Some(Rect {
                x: area.x,
                y: area.y.saturating_add(list_h),
                width: area.width,
                height: picker_h,
            }),
        )
    } else {
        (area, None)
    };
    let area = list_area;

    let snapshot = match app.session_settings_snapshot.as_ref() {
        Some(s) => s,
        None => {
            render_settings_detail_placeholder(f, area, "Agents");
            if let Some(p_area) = picker_area {
                render_session_agents_picker(f, p_area, app);
            }
            return;
        }
    };
    let agent_count = snapshot.agents.len();
    let cursor = app.session_agents_cursor.min(agent_count.saturating_sub(1));

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Agents", theme::accent_bold()),
            Span::styled(
                format!("    {} attached", snapshot.agents.len()),
                Style::default().fg(theme::DIM),
            ),
        ]),
        Line::from(vec![Span::styled(
            "  ─────",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(""),
    ];

    if snapshot.agents.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (no agents attached — press [a] to add)",
            Style::default().fg(theme::DIM),
        )]));
    } else {
        for (i, agent) in snapshot.agents.iter().enumerate() {
            let resolved_override = snapshot
                .agent_models
                .get(&agent.display_name)
                .cloned()
                .or_else(|| snapshot.model_pin.clone())
                .map(|m| backend.resolve_model_name(Some(m.as_str())))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(no model)".to_string());
            let is_selected = i == cursor;
            let marker = if is_selected { "> " } else { "  " };
            let style = if is_selected {
                theme::selected()
            } else {
                Style::default().fg(Color::White)
            };
            let is_host = snapshot
                .host_agent_db_id
                .as_deref()
                .is_some_and(|h| h == agent.db_id);
            let host_tag = if is_host { "  [host]" } else { "" };
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "{marker}{name:<14}  {resolved_override}{host_tag}",
                    name = agent.display_name,
                ),
                style,
            )]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  [a] add agent · [d] remove selected",
        Style::default().fg(theme::DIM),
    )]));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);

    // One click region per agent row (header is 4 lines: blank, title,
    // dashes, blank). Click focuses the detail pane and moves the cursor.
    let rows_y0 = area.y.saturating_add(4);
    for i in 0..agent_count {
        let y = rows_y0.saturating_add(i as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        app.click_regions.push(ClickRegion {
            x: area.x,
            y,
            w: area.width,
            h: 1,
            target: ClickTarget::SettingsDetailRow(i),
        });
    }

    if let Some(p_area) = picker_area {
        render_session_agents_picker(f, p_area, app);
    }
}

/// Render the "add agent" picker into the carved-out bottom of the
/// Session→Agents detail area. Sources the visible candidate slice from
/// the live filter so the displayed list always matches what Enter would
/// commit.
fn render_session_agents_picker(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(picker) = app.settings_picker.as_ref() else {
        return;
    };
    let filtered_indices = picker.filtered();
    let items: Vec<&str> = filtered_indices
        .iter()
        .filter_map(|i| picker.candidates.get(*i).map(|s| s.as_str()))
        .collect();
    widgets::picker(
        f,
        area,
        &picker.label,
        &picker.filter,
        picker.cursor,
        &items,
        picker.selected,
    );
}

/// Static snapshot of the active session — name, id, created_at, message
/// count, attached agents, current agent + effective model. All values
/// come from the per-tab `Tab` plus the seeded session meta snapshot.
fn render_session_overview(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let tab = app.active();
    let snapshot = app.session_settings_snapshot.as_ref();

    let id_short = super::short_session_id(&tab.session_db_id);
    let name = tab
        .session_name
        .clone()
        .unwrap_or_else(|| "(unnamed)".to_string());
    let created = snapshot
        .and_then(|s| s.created_at)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let message_count = snapshot
        .map(|s| s.entry_count.to_string())
        .unwrap_or_else(|| tab.entries.len().to_string());
    let attached_agents = snapshot.map(|s| s.agents.len()).unwrap_or(0).to_string();
    let host_agent = snapshot
        .and_then(|s| s.host_agent_db_id.as_deref())
        .unwrap_or("(routes to first attached agent)")
        .to_string();
    let current_agent = if tab.current_agent.is_empty() {
        "(none)".to_string()
    } else {
        tab.current_agent.clone()
    };
    let effective_model = if tab.effective_model.is_empty() {
        "(no model configured)".to_string()
    } else {
        tab.effective_model.clone()
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled("  Overview", theme::accent_bold())]),
        Line::from(vec![Span::styled(
            "  ─────",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(""),
        about_kv("  name", &name),
        about_kv("  id", &id_short),
        about_kv("  created", &created),
        about_kv("  messages", &message_count),
        Line::from(""),
        about_kv("  attached agents", &attached_agents),
        about_kv("  current agent", &current_agent),
        about_kv("  effective model", &effective_model),
        about_kv("  host agent", &host_agent),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Session pin + per-agent overrides. Press Enter (handled upstream) to
/// open the model picker for the active scope. v1 picker doesn't pre-
/// select the scope based on this row; user picks via the picker's own
/// scope strip.
/// Session → Models — cursor list of scopes (row 0 = Session pin,
/// rows 1..n = each attached agent). The selected row's scope is what
/// Enter opens the picker for.
fn render_session_models(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &mut App,
    backend: &BackendManager,
) {
    let snapshot = match app.session_settings_snapshot.as_ref() {
        Some(s) => s,
        None => {
            render_settings_detail_placeholder(f, area, "Models");
            return;
        }
    };

    let total_rows = 1 + snapshot.agents.len();
    let cursor = app.session_models_cursor.min(total_rows.saturating_sub(1));

    let session_pin_label = snapshot
        .model_pin
        .clone()
        .unwrap_or_else(|| "(unset)".to_string());
    let session_resolved = {
        let effective = backend.resolve_model_name(snapshot.model_pin.as_deref());
        if effective.is_empty() {
            "(no backend default)".to_string()
        } else {
            effective
        }
    };

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled("  Models", theme::accent_bold())]),
        Line::from(vec![Span::styled(
            "  ─────",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(""),
    ];

    // Row 0 — Session pin. Selection marker + resolved-to suffix so the
    // user sees what the pin maps to without flipping to /agents.
    let row_offsets_start = lines.len() as u16;

    let session_marker = if cursor == 0 { "> " } else { "  " };
    let session_style = if cursor == 0 {
        theme::selected()
    } else {
        Style::default().fg(Color::White)
    };
    lines.push(Line::from(vec![Span::styled(
        format!(
            "{session_marker}{name:<14}  {pin}",
            name = "Session",
            pin = session_pin_label
        ),
        session_style,
    )]));
    lines.push(Line::from(vec![Span::styled(
        format!("      resolves to {session_resolved}"),
        Style::default().fg(theme::DIM),
    )]));
    lines.push(Line::from(""));

    // Rows 1..n — per-agent overrides. Display the override id when set
    // or "(uses session pin)" otherwise.
    if snapshot.agents.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (no agents attached — only Session is editable)",
            Style::default().fg(theme::DIM),
        )]));
    } else {
        lines.push(Line::from(vec![Span::styled(
            "  Per-agent overrides",
            Style::default().fg(theme::DIM),
        )]));
        for (i, agent) in snapshot.agents.iter().enumerate() {
            let row = i + 1;
            let is_selected = row == cursor;
            let marker = if is_selected { "> " } else { "  " };
            let style = if is_selected {
                theme::selected()
            } else {
                Style::default().fg(Color::White)
            };
            let pin_label = snapshot
                .agent_models
                .get(&agent.display_name)
                .cloned()
                .unwrap_or_else(|| "(uses session pin)".to_string());
            lines.push(Line::from(vec![Span::styled(
                format!("{marker}{name:<14}  {pin_label}", name = agent.display_name),
                style,
            )]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Enter — open picker for selected scope",
        Style::default().fg(theme::DIM),
    )]));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);

    // Click regions — one row per scope. Row 0 (Session) takes 1 line;
    // each agent row takes 1 line further down. Row 0 sits at
    // `row_offsets_start`; agents sit at offset + 3 + i (Session row +
    // resolved-to subline + blank line + per-agent header line).
    let area_bottom = area.y.saturating_add(area.height);
    let session_y = area.y.saturating_add(row_offsets_start);
    if session_y < area_bottom {
        app.click_regions.push(ClickRegion {
            x: area.x,
            y: session_y,
            w: area.width,
            h: 1,
            target: ClickTarget::SettingsDetailRow(0),
        });
    }
    if !snapshot.agents.is_empty() {
        let agents_start = row_offsets_start + 4;
        for (i, _) in snapshot.agents.iter().enumerate() {
            let y = area.y.saturating_add(agents_start + i as u16);
            if y >= area_bottom {
                break;
            }
            app.click_regions.push(ClickRegion {
                x: area.x,
                y,
                w: area.width,
                h: 1,
                target: ClickTarget::SettingsDetailRow(i + 1),
            });
        }
    }
}

/// Placeholder right-pane: shows the active category's name and a
/// `(coming soon)` line. Replaced category-by-category in subsequent
/// stages.
fn render_settings_detail_placeholder(f: &mut ratatui::Frame, area: Rect, category: &str) {
    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("  {category}"),
            theme::accent_bold(),
        )]),
        Line::from(vec![Span::styled(
            "  ─────",
            Style::default().fg(theme::DIM),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  (coming soon)",
            Style::default().fg(theme::DIM),
        )]),
    ];
    f.render_widget(Paragraph::new(lines), area);
}
