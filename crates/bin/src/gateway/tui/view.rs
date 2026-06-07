//! Ratatui rendering for the two TUI modes (chat + session picker).
//! Pure view functions — no mutation, no async.

use chaz_core::backends::BackendManager;
use chaz_core::config::Config;
use chaz_core::mcp::{McpRegistryEntry, McpServerStatus};
use chaz_core::server::Server;
use chaz_core::session::EntryType;
use chaz_core::util::truncate_chars;

use std::sync::Arc;

use chrono::{DateTime, Utc};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::App;
use super::ClickRegion;
use super::ClickTarget;
use super::Overlay;
use super::PeerSettingsCategory;
use super::SessionSettingsCategory;
use super::SettingsScope;
use super::TuiMode;
use super::short_session_id;
use super::theme;
use super::widgets;

// Palette lives in `theme.rs`. Local aliases here keep the existing render
// code terse (`COLOR_USER` → `theme::USER`) without churn at every call site.
use theme::ACCENT as COLOR_ACCENT;
use theme::ASSISTANT as COLOR_ASSISTANT;
use theme::DIM as COLOR_DIM;
use theme::ERROR as COLOR_ERROR;
use theme::SYSTEM as COLOR_SYSTEM;
use theme::TOOL as COLOR_TOOL;
use theme::USER as COLOR_USER;

/// Last `/`-separated segment of a model id (`anthropic/claude-opus-4-7` →
/// `claude-opus-4-7`). Bare ids without `/` are returned as-is. Used for the
/// status bar so the slug stays readable without the provider prefix.
fn model_slug(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Percent of the context budget occupied by the latest turn's prompt, for
/// the `ctx N%` status segment. `None` when the budget is unknown (zero) so
/// the caller hides the segment rather than dividing by zero. Can exceed 100
/// if a turn was packed past a later-lowered budget — reported honestly, not
/// clamped, so an over-budget session is visible.
fn ctx_pct(prompt_tokens: u32, budget: usize) -> Option<u64> {
    if budget == 0 {
        return None;
    }
    Some((prompt_tokens as f64 / budget as f64 * 100.0).round() as u64)
}

/// One-line preview of a ToolCall entry's content. Server writes ToolCall
/// content as `{name}({json_args})` (see `server.rs`). Returns
/// `(tool_name, args_preview)` — args collapsed to single line, truncated.
fn summarize_tool_call(content: &str) -> (String, String) {
    let (name, rest) = content.split_once('(').unwrap_or((content, ""));
    let args = rest.strip_suffix(')').unwrap_or(rest);
    let oneline: String = args
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let trimmed = oneline.split_whitespace().collect::<Vec<_>>().join(" ");
    (name.trim().to_string(), trimmed)
}

/// One-line preview of a ToolResult entry's content. Server writes
/// `{name}: {output}` or `{name}: ERROR: {output}`. Returns
/// `(tool_name, summary, is_error)`.
fn summarize_tool_result(content: &str) -> (String, String, bool) {
    let (name, rest) = content.split_once(": ").unwrap_or((content, ""));
    let (is_error, body) = match rest.strip_prefix("ERROR: ") {
        Some(b) => (true, b),
        None => (false, rest),
    };
    let first = body.lines().next().unwrap_or("");
    let oneline = first.split_whitespace().collect::<Vec<_>>().join(" ");
    (name.trim().to_string(), oneline, is_error)
}

/// Truncate a String to at most `n` chars, appending `…` if shortened.
fn ellipsize(s: &str, n: usize) -> String {
    let t = truncate_chars(s, n);
    if t.len() < s.len() {
        format!("{t}…")
    } else {
        t.to_string()
    }
}

/// Prepare arbitrary content for rendering as ratatui `Line`s. Truncates
/// char-wise if requested (appending `…`), then splits on `\n`. A `Line`
/// must not contain embedded newlines — `WordWrapper` treats `\n` as
/// zero-width whitespace, concatenating adjacent words and corrupting
/// layout.
fn display_lines(content: &str, max_chars: Option<usize>) -> Vec<String> {
    let owned;
    let src: &str = match max_chars {
        Some(n) => {
            let t = truncate_chars(content, n);
            if t.len() < content.len() {
                owned = format!("{t}…");
                &owned
            } else {
                content
            }
        }
        None => content,
    };
    let out: Vec<String> = src.split('\n').map(str::to_owned).collect();
    if out.is_empty() {
        vec![String::new()]
    } else {
        out
    }
}

pub(super) fn ui(
    f: &mut ratatui::Frame,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    config: &Config,
) {
    // Click regions are rebuilt from scratch each frame so coordinates match
    // what the user is currently seeing.
    app.click_regions.clear();

    // Refresh peer-side caches whenever any Settings page is up. The Peer
    // Settings views index into these directly for action keys ([r]
    // reload, [d] remove, Ctrl+↑↓ reorder); the Session→Agents picker
    // also reads `peer_agents_names` to populate its candidate list, so
    // it has to stay fresh in Session scope too.
    if matches!(app.mode, TuiMode::Settings(_)) {
        let mut names = server.agents().names();
        names.sort();
        app.peer_agents_names = names;
        app.peer_defaults = server.default_agents();
        app.peer_mcp_servers = server.mcp_registry().snapshot();
    }

    match app.mode {
        TuiMode::Chat => ui_chat(f, app),
        TuiMode::SessionPicker => ui_picker(f, app),
        TuiMode::ModelPicker => ui_model_picker(f, app),
        TuiMode::Settings(scope) => ui_settings(f, app, scope, server, backend, config),
    }

    if app.overlay.is_some() {
        ui_overlay(f, app);
    }
}

/// Centered popup rect: `percent_x%` wide × `percent_y%` tall, at least 20×5.
fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let w = area.width.saturating_mul(percent_x) / 100;
    let h = area.height.saturating_mul(percent_y) / 100;
    let w = w.max(20).min(area.width);
    let h = h.max(5).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn ui_overlay(f: &mut ratatui::Frame, app: &mut App) {
    match &app.overlay {
        Some(Overlay::Help { scroll }) => {
            let scroll = *scroll;
            ui_help_overlay(f, app, scroll);
        }
        Some(Overlay::RenamePrompt { .. }) => ui_rename_overlay(f, app),
        None => {}
    }
}

/// Grouped help catalog — the shared command catalog (see
/// `input::command_catalog`). A `#`-prefixed entry is a section header; every
/// other row is a clickable command that inserts its template on click.
fn help_entries() -> Vec<(&'static str, &'static str)> {
    super::input::command_catalog()
}

fn ui_help_overlay(f: &mut ratatui::Frame, app: &mut App, scroll: u16) {
    let area = f.area();
    let popup = centered_rect(area, 80, 80);

    // Dim/disable-click backdrop: clicks here dismiss the overlay.
    app.click_regions.push(ClickRegion {
        x: area.x,
        y: area.y,
        w: area.width,
        h: area.height,
        target: ClickTarget::OverlayDismiss,
    });

    f.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_ACCENT))
        .title(" Help — Esc to close · ↑↓/PgUp/PgDn/wheel scroll · click a row to insert ")
        .title_style(
            Style::default()
                .fg(COLOR_SYSTEM)
                .add_modifier(Modifier::BOLD),
        );

    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let entries = help_entries();
    let mut lines: Vec<Line> = Vec::new();
    // y cursor relative to `inner`: start at 0, advance per line. We push a
    // click region for each command row, using the post-scroll absolute y.
    for (row_idx, (cmd, desc)) in entries.iter().enumerate() {
        let abs_y_i = inner.y as i32 + row_idx as i32 - scroll as i32;
        if cmd.starts_with('#') {
            let header = cmd.trim_start_matches('#').trim();
            lines.push(Line::from(vec![Span::styled(
                format!("  {header}"),
                Style::default().fg(COLOR_TOOL).add_modifier(Modifier::BOLD),
            )]));
        } else {
            // Only register hit-tests for rows that are visible inside the
            // popup after scrolling — off-screen rows shouldn't capture clicks.
            if abs_y_i >= inner.y as i32 && abs_y_i < (inner.y as i32 + inner.height as i32) {
                app.click_regions.push(ClickRegion {
                    x: inner.x,
                    y: abs_y_i as u16,
                    w: inner.width,
                    h: 1,
                    target: ClickTarget::HelpCommand(cmd),
                });
            }
            lines.push(Line::from(vec![
                Span::styled(format!("  {cmd}"), Style::default().fg(COLOR_ACCENT)),
                Span::raw(" "),
                Span::styled(*desc, Style::default().fg(COLOR_DIM)),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(paragraph, inner);
}

/// Modal text input for renaming the highlighted session from the picker.
/// Empty submission clears the alias. Esc cancels. Clicks outside the popup
/// dismiss.
fn ui_rename_overlay(f: &mut ratatui::Frame, app: &mut App) {
    // Pull the overlay fields out by clone so we don't hold an immutable
    // borrow of `app` while pushing click regions below.
    let (title, input, cursor) = match &app.overlay {
        Some(Overlay::RenamePrompt {
            title,
            input,
            cursor,
            ..
        }) => (title.clone(), input.clone(), *cursor),
        _ => return,
    };

    let area = f.area();
    // Compact popup — one line for the title bar, one for the input, one for
    // the help footer, plus borders.
    let w = area.width.saturating_mul(60) / 100;
    let w = w.max(30).min(area.width);
    let h: u16 = 5;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    // Click anywhere outside the popup → dismiss. Inside the popup we don't
    // register fine-grained regions; keyboard owns the editing UX.
    app.click_regions.push(ClickRegion {
        x: area.x,
        y: area.y,
        w: area.width,
        h: area.height,
        target: ClickTarget::OverlayDismiss,
    });

    f.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_ACCENT))
        .title(format!(" {title} "))
        .title_style(
            Style::default()
                .fg(COLOR_SYSTEM)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    let input_widget = Paragraph::new(input.as_str());
    f.render_widget(input_widget, chunks[0]);

    let help = Paragraph::new(" [Enter] save · empty = clear · [Esc] cancel")
        .style(Style::default().fg(COLOR_DIM));
    f.render_widget(help, chunks[1]);

    let cursor_x = chunks[0].x + cursor as u16;
    let cursor_y = chunks[0].y;
    f.set_cursor_position((cursor_x, cursor_y));
}

fn ui_chat(f: &mut ratatui::Frame, app: &mut App) {
    // 4-line approval panel when a tool is waiting on the user; 0 otherwise.
    let has_approval = app.active().pending_approval.is_some();
    let approval_h: u16 = if has_approval { 4 } else { 0 };
    // 1-line tab bar at the top. Always present even with one tab so the user
    // has a consistent affordance.
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(approval_h),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .split(f.area());

    render_tab_bar(f, app, chunks[0]);

    let messages_area = chunks[1];
    let inner_x = messages_area.x.saturating_add(1);
    let inner_y = messages_area.y.saturating_add(1);
    let inner_width = messages_area.width.saturating_sub(2);
    let messages_height = messages_area.height.saturating_sub(2);

    let mut lines: Vec<Line> = Vec::new();
    // (logical line idx, x col offset from inner messages area, target).
    // Translated to absolute ClickRegions after wrap math below.
    let mut pending_clicks: Vec<(usize, u16, ClickTarget)> = Vec::new();

    let tab = app.active();
    for (entry_idx, entry) in tab.entries.iter().enumerate() {
        let debug_prefix = if app.debug_mode {
            let ts = entry.timestamp.format("%H:%M:%S");
            let typ = format!("{:?}", entry.entry_type);
            format!("[{ts} {typ:<10}] ")
        } else {
            String::new()
        };
        let dim = Style::default().fg(COLOR_DIM);

        match entry.entry_type {
            EntryType::Message => {
                let is_agent = app.agent_names.contains(&entry.sender);
                let is_system = entry.sender == "system";
                let sender_style = if is_system {
                    Style::default()
                        .fg(COLOR_SYSTEM)
                        .add_modifier(Modifier::BOLD)
                } else if is_agent {
                    Style::default()
                        .fg(COLOR_ASSISTANT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(COLOR_USER).add_modifier(Modifier::BOLD)
                };

                // Horizontal separator above each user turn so blocks of
                // tool work / agent replies group visually. Skip for the
                // very first entry — no preceding turn to separate from.
                if !is_agent && !is_system && !lines.is_empty() {
                    let rule: String = "─".repeat(inner_width as usize);
                    lines.push(Line::from(vec![Span::styled(rule, dim)]));
                }

                let label = format!("{}{}:", debug_prefix, entry.sender);

                lines.push(Line::from(vec![Span::styled(label, sender_style)]));

                for content_line in entry.content.lines() {
                    lines.push(Line::from(format!("  {content_line}")));
                }
                lines.push(Line::from(""));
            }
            // Directives, ToolCall, ToolResult are collapsible. Per-entry
            // override flips the global default (`app.expand_all`).
            EntryType::Directive => {
                let expanded = app.expand_all != tab.expanded_entries.contains(&entry_idx);
                let icon = if expanded { "▾" } else { "▸" };
                let icon_col = debug_prefix.chars().count() as u16 + 2; // "  " before icon
                let first = entry.content.lines().next().unwrap_or("");
                let head_label = format!("{} (directive)", entry.sender);
                let header_spans: Vec<Span> = if expanded {
                    vec![
                        Span::styled(format!("{debug_prefix}  "), dim),
                        Span::styled(icon, dim),
                        Span::styled(format!(" {head_label}"), dim),
                    ]
                } else {
                    let preview = ellipsize(&first.replace('\t', " "), 80);
                    vec![
                        Span::styled(format!("{debug_prefix}  "), dim),
                        Span::styled(icon, dim),
                        Span::styled(format!(" {head_label}: {preview}"), dim),
                    ]
                };
                lines.push(Line::from(header_spans));
                pending_clicks.push((
                    lines.len() - 1,
                    icon_col,
                    ClickTarget::ToggleEntryExpanded(entry_idx),
                ));
                if expanded {
                    for l in display_lines(&entry.content, None) {
                        lines.push(Line::from(vec![Span::styled(format!("      {l}"), dim)]));
                    }
                }
            }
            EntryType::Ack => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}{} thinking...", entry.sender),
                    dim,
                )]));
            }
            EntryType::ToolCall => {
                let (name, args) = summarize_tool_call(&entry.content);
                let tool_style = Style::default().fg(COLOR_TOOL);
                let expanded = app.expand_all != tab.expanded_entries.contains(&entry_idx);
                let icon = if expanded { "▾" } else { "▸" };
                let icon_col = debug_prefix.chars().count() as u16 + 2;
                let header_spans: Vec<Span> = if expanded {
                    vec![
                        Span::styled(format!("{debug_prefix}  "), dim),
                        Span::styled(icon, dim),
                        Span::styled(" ", dim),
                        Span::styled(name, tool_style),
                    ]
                } else {
                    let preview = ellipsize(&args, 90);
                    vec![
                        Span::styled(format!("{debug_prefix}  "), dim),
                        Span::styled(icon, dim),
                        Span::styled(" ", dim),
                        Span::styled(name, tool_style),
                        Span::styled(format!(" {preview}"), dim),
                    ]
                };
                lines.push(Line::from(header_spans));
                pending_clicks.push((
                    lines.len() - 1,
                    icon_col,
                    ClickTarget::ToggleEntryExpanded(entry_idx),
                ));
                if expanded {
                    for l in display_lines(&args, None) {
                        lines.push(Line::from(vec![Span::styled(format!("      {l}"), dim)]));
                    }
                }
            }
            EntryType::ToolResult => {
                let (name, summary, is_error) = summarize_tool_result(&entry.content);
                let tool_style = Style::default().fg(COLOR_TOOL);
                let expanded = app.expand_all != tab.expanded_entries.contains(&entry_idx);
                let icon = if is_error {
                    "✗"
                } else if expanded {
                    "▾"
                } else {
                    "▸"
                };
                let icon_style = if is_error {
                    Style::default().fg(COLOR_ERROR)
                } else {
                    dim
                };
                let icon_col = debug_prefix.chars().count() as u16 + 2;
                let header_spans: Vec<Span> = if expanded {
                    vec![
                        Span::styled(format!("{debug_prefix}  "), dim),
                        Span::styled(icon, icon_style),
                        Span::styled(" ", dim),
                        Span::styled(name, tool_style),
                    ]
                } else {
                    let preview = ellipsize(&summary, 90);
                    vec![
                        Span::styled(format!("{debug_prefix}  "), dim),
                        Span::styled(icon, icon_style),
                        Span::styled(" ", dim),
                        Span::styled(name, tool_style),
                        Span::styled(format!(" {preview}"), dim),
                    ]
                };
                lines.push(Line::from(header_spans));
                pending_clicks.push((
                    lines.len() - 1,
                    icon_col,
                    ClickTarget::ToggleEntryExpanded(entry_idx),
                ));
                if expanded {
                    let body = entry
                        .content
                        .split_once(": ")
                        .map(|(_, b)| b)
                        .unwrap_or(&entry.content);
                    for l in display_lines(body, None) {
                        lines.push(Line::from(vec![Span::styled(format!("      {l}"), dim)]));
                    }
                }
            }
            EntryType::Error => {
                let red = Style::default().fg(COLOR_ERROR);
                for (i, l) in display_lines(&entry.content, None).into_iter().enumerate() {
                    let text = if i == 0 {
                        format!("{debug_prefix}  ERROR {}: {l}", entry.sender)
                    } else {
                        format!("    {l}")
                    };
                    lines.push(Line::from(vec![Span::styled(text, red)]));
                }
                lines.push(Line::from(""));
            }
            EntryType::Summary => {
                let label = format!("{debug_prefix}--- context summary ---");
                lines.push(Line::from(vec![Span::styled(
                    label,
                    Style::default()
                        .fg(COLOR_ACCENT)
                        .add_modifier(Modifier::BOLD),
                )]));
                for content_line in entry.content.lines() {
                    lines.push(Line::from(vec![Span::styled(
                        format!("  {content_line}"),
                        Style::default().fg(COLOR_ACCENT),
                    )]));
                }
                lines.push(Line::from(""));
            }
        }
    }

    if tab.waiting {
        lines.push(Line::from(vec![Span::styled(
            "  thinking...",
            Style::default().fg(COLOR_DIM),
        )]));
    }

    // Snapshot what we need from `tab` before releasing the borrow so the
    // approval panel can take a &mut borrow of app.click_regions.
    let scroll_offset = tab.scroll_offset;
    // Never surface the full session DB id here — it's long and noisy. Show
    // the alias if set, otherwise a short id prefix; the full id is available
    // via /info and /share when actually needed.
    let session_label = match &tab.session_name {
        Some(name) => name.clone(),
        None => short_session_id(&tab.session_db_id),
    };
    let current_agent = tab.current_agent.clone();
    let effective_model = tab.effective_model.clone();
    let roster = tab.roster.clone();
    // Aggregate this session's LLM usage for the status bar. Mirrors
    // `commands::session::format_usage_summary`: only entries carrying
    // response metadata count, and `cached` is the cache-read subset of
    // prompt tokens.
    let usage_segment = {
        let (mut prompt, mut completion, mut cached) = (0u64, 0u64, 0u64);
        let mut cost = 0.0f64;
        let mut saw_cost = false;
        let mut calls = 0u32;
        for e in &tab.entries {
            let Some(m) = &e.metadata else { continue };
            calls += 1;
            prompt += m.usage.prompt_tokens as u64;
            completion += m.usage.completion_tokens as u64;
            cached += m.usage.cached_tokens.unwrap_or(0) as u64;
            if let Some(c) = m.usage.cost_usd {
                cost += c;
                saw_cost = true;
            }
        }
        if calls == 0 {
            String::new()
        } else {
            let pct = if prompt > 0 {
                (cached as f64 / prompt as f64 * 100.0).round() as u64
            } else {
                0
            };
            let cost_part = if saw_cost {
                format!(" • ${cost:.4}")
            } else {
                String::new()
            };
            format!(
                " | {}/{} tok • {pct}% cached{cost_part}",
                human_tokens(prompt),
                human_tokens(completion)
            )
        }
    };
    // Current context occupancy — distinct from `usage_segment`'s lifetime
    // totals. Numerator is the most recent turn's `context_tokens` (the input
    // size of that turn's final LLM call = how full the window was), NOT
    // `usage.prompt_tokens`, which sums every ReAct iteration and so overshoots
    // the window on multi-tool-call turns. Hidden until a turn carries it, or
    // when no budget is known.
    let ctx_segment = {
        let last_ctx = tab
            .entries
            .iter()
            .rev()
            .find_map(|e| e.metadata.as_ref().and_then(|m| m.context_tokens));
        match last_ctx.and_then(|p| ctx_pct(p, tab.context_budget)) {
            Some(pct) => format!(" | ctx {pct}%"),
            None => String::new(),
        }
    };
    let approval_info = tab.pending_approval.as_ref().map(|ex| {
        (
            ex.info.name.clone(),
            ex.info.risk_level.to_string(),
            ex.info.arguments_display.clone(),
        )
    });
    let _ = tab;

    // Per-line visual heights, accumulated. Used to translate
    // logical-line positions of pending click regions into screen rows that
    // account for wrap. Mirrors ratatui's word-wrap by running each line
    // through its own line_count probe.
    let mut visual_offsets: Vec<u16> = Vec::with_capacity(lines.len() + 1);
    visual_offsets.push(0);
    for l in &lines {
        let probe = Paragraph::new(l.clone()).wrap(Wrap { trim: false });
        let h = probe.line_count(inner_width).min(u16::MAX as usize) as u16;
        visual_offsets.push(visual_offsets.last().unwrap().saturating_add(h.max(1)));
    }
    let content_height = *visual_offsets.last().unwrap();
    let scroll = if content_height > messages_height {
        content_height
            .saturating_sub(messages_height)
            .saturating_sub(scroll_offset)
    } else {
        0
    };

    // Translate pending header clicks into absolute screen regions, skipping
    // any whose line is currently scrolled out of view.
    for (logical_idx, x_offset, target) in pending_clicks {
        let visual_row = visual_offsets[logical_idx];
        if visual_row < scroll {
            continue;
        }
        let row_relative = visual_row - scroll;
        if row_relative >= messages_height {
            continue;
        }
        app.click_regions.push(ClickRegion {
            x: inner_x.saturating_add(x_offset),
            y: inner_y.saturating_add(row_relative),
            w: 1,
            h: 1,
            target,
        });
    }

    let messages = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_DIM))
                .title(Span::styled(" Chaz ", Style::default().fg(COLOR_ACCENT))),
        );
    f.render_widget(messages, messages_area);

    if let Some((tool_name, risk, args)) = approval_info {
        render_approval_panel(
            f,
            &mut app.click_regions,
            chunks[2],
            &tool_name,
            &risk,
            &args,
        );
    }

    let debug_indicator = if app.debug_mode { " | DEBUG" } else { "" };
    let expand_indicator = if app.expand_all { " | EXP" } else { "" };

    // Agent/model segment. Single-agent (or roster-less) sessions render the
    // original ` | agent: X | model: Y` so their bar stays byte-identical;
    // multi-agent sessions list the whole roster with the host marked (`*`)
    // and each agent's effective model.
    let multi_agent = roster.len() > 1;
    let agent_segment = if multi_agent {
        let list = roster
            .iter()
            .map(|r| {
                let host = if r.is_host { "*" } else { "" };
                if r.model.is_empty() {
                    format!("{}{host}", r.name)
                } else {
                    format!("{}{host}→{}", r.name, model_slug(&r.model))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(" | agents: {list}")
    } else {
        let model_segment = if effective_model.is_empty() {
            " | model: —".to_string()
        } else {
            format!(" | model: {}", model_slug(&effective_model))
        };
        format!(" | agent: {current_agent}{model_segment}")
    };

    let make_status = |agent_segment: &str| {
        format!(
            " {session_label}{agent_segment}{ctx_segment}{usage_segment}{debug_indicator}{expand_indicator}"
        )
    };
    let mut status_text = make_status(&agent_segment);

    // If the full roster would overflow the status line, collapse it to a
    // count plus the host, leaving the per-agent detail to the Settings page.
    if multi_agent && status_text.chars().count() > chunks[3].width as usize {
        let collapsed = match roster.iter().find(|r| r.is_host) {
            Some(h) if !h.model.is_empty() => format!(
                " | agents: {} · host {}→{}",
                roster.len(),
                h.name,
                model_slug(&h.model)
            ),
            Some(h) => format!(" | agents: {} · host {}", roster.len(), h.name),
            None => format!(" | agents: {}", roster.len()),
        };
        status_text = make_status(&collapsed);
    }
    let status = Paragraph::new(status_text).style(
        Style::default()
            .bg(Color::Rgb(0x1a, 0x1d, 0x26))
            .fg(Color::White),
    );
    f.render_widget(status, chunks[3]);

    let input = Paragraph::new(app.input.as_str()).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_DIM))
            .title(Span::styled(" > ", Style::default().fg(COLOR_ACCENT))),
    );
    f.render_widget(input, chunks[4]);

    // Completion popup floats just above the input box, over the bottom of
    // the transcript. Drawn after the transcript so it sits on top.
    render_completion_popup(f, app, chunks[1], chunks[4]);

    let cursor_x = chunks[4].x + app.cursor as u16 + 1;
    let cursor_y = chunks[4].y + 1;
    f.set_cursor_position((cursor_x, cursor_y));
}

/// Slash-command completion dropdown. Anchored to the bottom-left of the input
/// box (`input_area`), growing upward, clamped to the transcript region
/// (`msg_area`). Renders nothing when no completion is active. Records a click
/// region per visible row so a click accepts that command.
fn render_completion_popup(
    f: &mut ratatui::Frame,
    app: &mut App,
    msg_area: Rect,
    input_area: Rect,
) {
    // Snapshot the cheap-to-copy completion state so we don't hold a borrow
    // of `app.completion` while pushing into `app.click_regions` below
    // (the `&'static str` pairs are just pointers — clone is trivial).
    let (matches, selected): (Vec<(&'static str, &'static str)>, usize) = match &app.completion {
        Some(c) if !c.matches.is_empty() => (c.matches.clone(), c.selected),
        _ => return,
    };

    const MAX_ROWS: usize = 8;
    let total = matches.len();
    let visible = total.min(MAX_ROWS);

    // Scroll the window so the selected row stays in view.
    let start = if selected >= visible {
        selected - visible + 1
    } else {
        0
    };

    // Box height = rows + top/bottom border. Clamp to available space above
    // the input box so it never overruns the transcript.
    let max_h = input_area.y.saturating_sub(msg_area.y);
    let h = ((visible as u16) + 2).min(max_h.max(3));
    let inner_rows = h.saturating_sub(2) as usize;
    let w = input_area.width;
    let x = input_area.x;
    let y = input_area.y.saturating_sub(h);

    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_DIM))
        .title(" commands — ↑↓ select · Tab insert · Esc dismiss ")
        .title_style(Style::default().fg(COLOR_SYSTEM));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let cmd_w = matches
        .iter()
        .map(|(c, _)| c.len())
        .max()
        .unwrap_or(0)
        .min(28);

    let mut lines: Vec<Line> = Vec::new();
    for (i, (cmd, desc)) in matches.iter().enumerate().skip(start).take(inner_rows) {
        let is_sel = i == selected;
        let marker = if is_sel { "▸ " } else { "  " };
        let cmd_style = if is_sel {
            Style::default()
                .fg(Color::Black)
                .bg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(COLOR_ACCENT)
        };
        let desc_style = if is_sel {
            Style::default().fg(Color::Black).bg(COLOR_ACCENT)
        } else {
            Style::default().fg(COLOR_DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(marker, cmd_style),
            Span::styled(format!("{cmd:<cmd_w$}"), cmd_style),
            Span::raw("  "),
            Span::styled(*desc, desc_style),
        ]));

        // Click region for this visible row (absolute terminal coords).
        let row_y = inner.y + (i - start) as u16;
        if row_y < inner.y + inner.height {
            app.click_regions.push(ClickRegion {
                x: inner.x,
                y: row_y,
                w: inner.width,
                h: 1,
                target: ClickTarget::CompletionSelect(i),
            });
        }
    }

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

/// Render the tab bar: one line across the top showing each tab's title,
/// active tab highlighted, with a clickable × close marker on each tab when
/// there's more than one tab. Also records click regions for tab-activate
/// and tab-close.
fn render_tab_bar(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let n = app.tabs.len();
    let show_close = n > 1;
    let bar_bg = Color::Rgb(0x1a, 0x1d, 0x26); // status-bar dark
    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;
    let row_y = area.y;
    for (i, tab) in app.tabs.iter().enumerate() {
        let is_active = i == app.active_tab;
        let title = tab.title();
        // Active tab: bright accent; inactive: dim. Single bar separator
        // between adjacent tabs instead of a bg switch.
        let title_label = format!(" {title} ");
        let close_label = if show_close { "× " } else { "" };
        let title_style = if is_active {
            Style::default()
                .fg(COLOR_ACCENT)
                .bg(bar_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(COLOR_DIM).bg(bar_bg)
        };
        let close_style = if is_active {
            Style::default().fg(COLOR_ERROR).bg(bar_bg)
        } else {
            Style::default().fg(COLOR_DIM).bg(bar_bg)
        };
        spans.push(Span::styled(title_label.clone(), title_style));
        if show_close {
            spans.push(Span::styled(close_label.to_string(), close_style));
        }
        // Record hit regions.
        let title_w = title_label.chars().count() as u16;
        if x + title_w <= area.x + area.width {
            app.click_regions.push(ClickRegion {
                x,
                y: row_y,
                w: title_w,
                h: 1,
                target: ClickTarget::TabActivate(i),
            });
        }
        x = x.saturating_add(title_w);
        if show_close {
            let close_w = close_label.chars().count() as u16;
            if x + close_w <= area.x + area.width {
                app.click_regions.push(ClickRegion {
                    x,
                    y: row_y,
                    w: close_w,
                    h: 1,
                    target: ClickTarget::TabClose(i),
                });
            }
            x = x.saturating_add(close_w);
        }
        // Divider between adjacent tabs (skipped after the last).
        if i + 1 < n {
            spans.push(Span::styled("│", Style::default().fg(COLOR_DIM).bg(bar_bg)));
            x = x.saturating_add(1);
        }
    }
    // Hint text at the right side if space allows.
    let hint = " Ctrl+, settings · Ctrl+PgUp/PgDn · Ctrl+W";
    let used = x.saturating_sub(area.x);
    let remaining = area.width.saturating_sub(used);
    if remaining as usize >= hint.len() {
        let pad = remaining as usize - hint.len();
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bar_bg)));
        spans.push(Span::styled(
            hint.to_string(),
            Style::default().fg(COLOR_DIM).bg(bar_bg),
        ));
    }
    let line = Line::from(spans);
    let paragraph = Paragraph::new(vec![line]).style(Style::default().bg(bar_bg).fg(Color::White));
    f.render_widget(paragraph, area);
}

/// Render the tool-approval panel in the row reserved for it and push
/// clickable regions for the three buttons.
fn render_approval_panel(
    f: &mut ratatui::Frame,
    click_regions: &mut Vec<ClickRegion>,
    area: Rect,
    tool_name: &str,
    risk: &str,
    args: &str,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_SYSTEM))
        .title(format!(" Tool approval — {tool_name} ({risk}) "))
        .title_style(
            Style::default()
                .fg(COLOR_SYSTEM)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Two rows: args preview, then button row.
    let args_preview = truncate_chars(args, inner.width as usize * 2);
    let args_line = Line::from(vec![
        Span::styled("args: ", Style::default().fg(COLOR_DIM)),
        Span::raw(args_preview.replace('\n', " ")),
    ]);
    let buttons = Line::from(vec![
        Span::styled(
            " [y] approve ",
            Style::default()
                .fg(Color::Black)
                .bg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            " [n] deny ",
            Style::default()
                .fg(Color::Black)
                .bg(COLOR_ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            " [a] approve all ",
            Style::default()
                .fg(Color::Black)
                .bg(COLOR_SYSTEM)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let paragraph =
        Paragraph::new(vec![args_line, buttons]).style(Style::default().fg(Color::White));
    f.render_widget(paragraph, inner);

    // Record click regions for the buttons. Widths here must match the label
    // literals above (including leading/trailing spaces).
    let row_y = inner.y + 1;
    let mut x = inner.x;
    let w_yes: u16 = 13;
    let w_sep: u16 = 2;
    let w_no: u16 = 10;
    let w_all: u16 = 17;
    click_regions.push(ClickRegion {
        x,
        y: row_y,
        w: w_yes,
        h: 1,
        target: ClickTarget::ApprovalApprove,
    });
    x += w_yes + w_sep;
    click_regions.push(ClickRegion {
        x,
        y: row_y,
        w: w_no,
        h: 1,
        target: ClickTarget::ApprovalDeny,
    });
    x += w_no + w_sep;
    click_regions.push(ClickRegion {
        x,
        y: row_y,
        w: w_all,
        h: 1,
        target: ClickTarget::ApprovalApproveAll,
    });
}

/// Compact token count for the status bar: `942`, `12.3k`, `1.5M`.
fn human_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// "5m ago", "3h ago", "2d ago", "5w ago" — coarse age for the picker.
/// Returns `"—"` for legacy sessions that predate the catalog.
fn humanize_age(created_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(t) = created_at else {
        return "—".to_string();
    };
    let secs = (now - t).num_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 14 {
        return format!("{days}d ago");
    }
    let weeks = days / 7;
    format!("{weeks}w ago")
}

fn ui_picker(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());

    let list_area = chunks[0];
    // Inner area inside the bordered block is 1 inset on each side.
    let inner_x = list_area.x + 1;
    let inner_y = list_area.y + 1;
    let inner_w = list_area.width.saturating_sub(2);
    let inner_h = list_area.height.saturating_sub(2);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    // y offset inside inner area — starts at 1 because of the leading blank.
    let mut y_off: u16 = 1;

    // Virtual "New session" row — always pinned at the top and visually
    // distinct from real sessions. Display index 0; opens a new session.
    {
        let is_selected = app.picker_index == 0;
        let marker = if is_selected { "> " } else { "  " };
        let style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD)
        };
        if y_off < inner_h {
            let clipped_h = 2u16.min(inner_h - y_off);
            if clipped_h > 0 {
                app.click_regions.push(ClickRegion {
                    x: inner_x,
                    y: inner_y + y_off,
                    w: inner_w,
                    h: clipped_h,
                    target: ClickTarget::PickerNew,
                });
            }
        }
        lines.push(Line::from(vec![Span::styled(
            format!("{marker}+ New session"),
            style,
        )]));
        lines.push(Line::from(""));
        y_off = y_off.saturating_add(2);
    }

    if app.session_list.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  No saved sessions yet — select \"New session\" above.",
            Style::default().fg(COLOR_DIM),
        )]));
    } else {
        let current_session_db_id = app.active().session_db_id.clone();
        let now = Utc::now();
        for (i, info) in app.session_list.iter().enumerate() {
            let is_selected = i + 1 == app.picker_index;
            let is_current = info.session_db_id == current_session_db_id;

            let marker = if is_selected { "> " } else { "  " };
            let current_marker = if is_current { " *" } else { "" };

            let agent_str = info.agent_name.as_deref().unwrap_or("default");
            let title = match &info.name {
                Some(n) => format!("\"{n}\""),
                None => short_session_id(&info.session_db_id),
            };
            let gateway = info.gateway.as_str();
            let age = humanize_age(info.created_at, now);
            let closed_suffix = match info.status {
                chaz_core::session::SessionStatus::Closed => " (closed)",
                chaz_core::session::SessionStatus::Active => "",
            };

            // Show cost only when the backend reported one. Sessions whose
            // entries predate the metadata commit (or backends that don't
            // surface cost) just omit the suffix rather than printing $0.00.
            let cost_suffix = if info.cost_reported {
                format!(" • ${:.4}", info.total_cost_usd)
            } else {
                String::new()
            };
            let header = format!(
                "{marker}{title}{current_marker} [{gateway}] {agent_str} • {} entries • {age}{cost_suffix}{closed_suffix}",
                info.entry_count
            );

            let is_closed = matches!(info.status, chaz_core::session::SessionStatus::Closed);
            let style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if is_closed {
                Style::default().fg(COLOR_DIM)
            } else if is_current {
                Style::default().fg(COLOR_ACCENT)
            } else {
                Style::default().fg(COLOR_USER)
            };

            // One click region per session spanning its header + optional
            // preview + trailing blank. Blank line padding at the bottom isn't
            // captured, but clicking the gap between rows resolves to the
            // row immediately above, which feels natural.
            let row_h: u16 = if info.last_message.is_some() { 3 } else { 2 };
            if y_off < inner_h {
                let clipped_h = row_h.min(inner_h - y_off);
                if clipped_h > 0 {
                    app.click_regions.push(ClickRegion {
                        x: inner_x,
                        y: inner_y + y_off,
                        w: inner_w,
                        h: clipped_h,
                        target: ClickTarget::PickerSelect(i),
                    });
                }
            }

            lines.push(Line::from(vec![Span::styled(header, style)]));

            if let Some(ref preview) = info.last_message {
                lines.push(Line::from(vec![Span::styled(
                    format!("    {preview}"),
                    Style::default().fg(COLOR_DIM),
                )]));
            }

            lines.push(Line::from(""));
            y_off = y_off.saturating_add(row_h);
        }
    }

    let list = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_DIM))
            .title(Span::styled(
                " Sessions ",
                Style::default().fg(COLOR_ACCENT),
            )),
    );
    f.render_widget(list, chunks[0]);

    let help = Paragraph::new(
        " [Up/Down] navigate | [Enter] open/new | [n] new | [r] rename | [s] settings | [Esc/Ctrl+P] cancel",
    )
    .style(
        Style::default()
            .bg(Color::Rgb(0x1a, 0x1d, 0x26))
            .fg(Color::White),
    );
    f.render_widget(help, chunks[1]);
}

/// Format a price (input $/Mtok) for the picker. `—` when missing so all
/// rows align even if pricing isn't populated.
fn format_price(price: Option<f64>) -> String {
    match price {
        Some(p) if p < 1.0 => format!("${p:.2}"),
        Some(p) => format!("${p:.1}"),
        None => "—".to_string(),
    }
}

/// Fixed column widths for the picker price/caps columns. ID is dynamic.
const COL_W_PRICE: usize = 8;
const COL_W_CAPS: usize = 6;

fn ui_model_picker(f: &mut ratatui::Frame, app: &mut App) {
    // search bar | list | help. The picker mounts pre-scoped from its
    // caller (Models settings row) so there's no in-picker scope
    // switching — the scope shows in the list block's title instead.
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(f.area());

    render_model_search_bar(f, chunks[0], app);
    render_model_list_block(f, chunks[1], app);
    render_model_help_bar(f, chunks[2], app);
}

fn render_model_search_bar(f: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let total = app.model_list.len();
    let shown = app.model_picker_filtered.len();
    let counter = if app.model_search.is_empty() {
        format!("{total} models")
    } else {
        format!("{shown}/{total}")
    };
    let title = format!(" Search models ({counter}) ");
    let bar = Paragraph::new(Line::from(vec![
        Span::styled("  > ", Style::default().fg(COLOR_DIM)),
        Span::styled(
            app.model_search.clone(),
            Style::default().fg(COLOR_USER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "▎",
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
    ]))
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_DIM))
            .title(Span::styled(title, Style::default().fg(COLOR_ACCENT))),
    );
    f.render_widget(bar, area);
}

fn render_model_list_block(f: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &mut App) {
    let inner_x = area.x + 1;
    let inner_y = area.y + 1;
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);

    let mut lines: Vec<Line> = Vec::new();

    // Errors and empty-state shortcuts.
    if let Some(err) = app.model_picker_error.as_ref() {
        lines.push(Line::from(vec![Span::styled(
            format!("  Catalog fetch failed: {err}"),
            Style::default().fg(Color::Red),
        )]));
        lines.push(Line::from(vec![Span::styled(
            "  Press Ctrl+R to retry.",
            Style::default().fg(COLOR_DIM),
        )]));
    }

    if app.model_picker_filtered.is_empty() {
        let msg = if app.model_picker_loading && app.model_list.is_empty() {
            "  Loading OpenRouter catalog…"
        } else if app.model_list.is_empty() {
            "  No models known — populate `models:` under a backend or press Ctrl+R."
        } else {
            "  No models match the search."
        };
        lines.push(Line::from(vec![Span::styled(
            msg,
            Style::default().fg(COLOR_DIM),
        )]));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_DIM))
            .title(Span::styled(
                " Select model ",
                Style::default().fg(COLOR_ACCENT),
            ));
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }

    // Dynamic id-column width: longest id among visible rows, capped so a
    // pathological 80-char id doesn't push the price columns off-screen.
    let id_w = app
        .model_picker_filtered
        .iter()
        .filter_map(|&i| app.model_list.get(i))
        .map(|m| m.id.chars().count())
        .max()
        .unwrap_or(24)
        .clamp(24, 56);

    // Column header — dim, single line at the top of the interior.
    lines.push(model_picker_header_line(id_w));

    // Adjust scroll so the selected row is visible. `inner_h` includes
    // the header line we just pushed; rows get `inner_h - 1`.
    let visible_rows = inner_h.saturating_sub(1).max(1) as usize;
    let sel = app.model_picker_index;
    let mut scroll = app.model_picker_scroll as usize;
    if sel < scroll {
        scroll = sel;
    } else if sel >= scroll + visible_rows {
        scroll = sel + 1 - visible_rows;
    }
    let max_scroll = app.model_picker_filtered.len().saturating_sub(visible_rows);
    scroll = scroll.min(max_scroll);
    app.model_picker_scroll = scroll as u16;

    // Highlight whichever model is pinned in the active scope. Falls
    // back to the tab's resolved effective model so the picker still
    // surfaces something useful when the Session scope has no pin.
    let current = app
        .active_scope_pin()
        .map(str::to_string)
        .unwrap_or_else(|| app.active().effective_model.clone());
    let end = (scroll + visible_rows).min(app.model_picker_filtered.len());
    // Header occupies y_off=1 (after the top border at 0); rows start at 2.
    let mut y_off: u16 = 2;
    for filtered_i in scroll..end {
        let model_idx = app.model_picker_filtered[filtered_i];
        let Some(info) = app.model_list.get(model_idx) else {
            continue;
        };
        let is_selected = filtered_i == sel;
        let is_current = info.id == current;

        let marker = if is_selected { "▸ " } else { "  " };
        let current_suffix = if is_current { "  (current)" } else { "" };
        let id_disp = if info.id.chars().count() > id_w {
            let truncated: String = info.id.chars().take(id_w.saturating_sub(1)).collect();
            format!("{truncated}…")
        } else {
            info.id.clone()
        };
        let caps = super::model_caps_badge(info);
        let row = format!(
            "{marker}{id:<id_w$}  {pin:>pw$}  {pout:>pw$}  {pcache:>pw$}  {caps:<cw$}{current_suffix}",
            id = id_disp,
            id_w = id_w,
            pin = format_price(info.price_input),
            pout = format_price(info.price_output),
            pcache = format_price(info.price_cache_read),
            pw = COL_W_PRICE,
            caps = caps,
            cw = COL_W_CAPS,
        );

        let style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::default().fg(COLOR_ACCENT)
        } else {
            Style::default().fg(COLOR_USER)
        };

        // Register click region against the row's screen y. `filtered_i`
        // is what the click handler expects (it indexes into
        // `model_picker_filtered`, not `model_list`).
        if y_off < inner_h {
            app.click_regions.push(ClickRegion {
                x: inner_x,
                y: inner_y + y_off,
                w: inner_w,
                h: 1,
                target: ClickTarget::ModelPickerSelect(filtered_i),
            });
        }
        lines.push(Line::from(vec![Span::styled(row, style)]));
        y_off = y_off.saturating_add(1);
    }

    // Scroll indicators in the title so the user knows there's more.
    // Scope label is part of the title so the user always sees which
    // scope they're editing — no separate strip needed.
    let scroll_hint = scroll_indicator(scroll, end, app.model_picker_filtered.len());
    let scope_label = app.model_picker_scope.label();
    let title_text = format!(" Pick model — {scope_label}{scroll_hint} ");

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_DIM))
        .title(Span::styled(title_text, Style::default().fg(COLOR_ACCENT)));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn render_model_help_bar(f: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let help_text = if app.model_picker_loading {
        " type to filter | ↑↓ PgUp/Dn Home/End | Enter select | Esc cancel | fetching catalog…"
            .to_string()
    } else {
        " type to filter | ↑↓ PgUp/Dn Home/End | Enter select | Ctrl+R refresh | Ctrl+U clear | Esc cancel"
            .to_string()
    };
    widgets::status_strip(f, area, &help_text);
}

/// Stage 1+ Settings page — sidebar of categories + per-category detail.
/// Composition style A (pure functions over the shared widget primitives).
/// Each category routes to its own renderer; categories that haven't been
/// implemented yet fall through to the `(coming soon)` placeholder.
fn ui_settings(
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
    let sidebar_focused = matches!(app.settings_focus, super::SettingsFocus::Sidebar);
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
    let detail = matches!(app.settings_focus, super::SettingsFocus::Detail);
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

fn model_picker_header_line(id_w: usize) -> Line<'static> {
    // Match the spacing in the row format string so columns line up.
    let header = format!(
        "  {id:<id_w$}  {pin:>pw$}  {pout:>pw$}  {pcache:>pw$}  {caps:<cw$}",
        id = "MODEL",
        id_w = id_w,
        pin = "IN",
        pout = "OUT",
        pcache = "CACHE",
        pw = COL_W_PRICE,
        caps = "CAPS",
        cw = COL_W_CAPS,
    );
    Line::from(vec![Span::styled(
        header,
        Style::default().fg(COLOR_DIM).add_modifier(Modifier::BOLD),
    )])
}

fn scroll_indicator(scroll: usize, end: usize, total: usize) -> String {
    if total == 0 {
        return String::new();
    }
    let above = scroll > 0;
    let below = end < total;
    match (above, below) {
        (false, false) => String::new(),
        (true, false) => " ▲".to_string(),
        (false, true) => " ▼".to_string(),
        (true, true) => " ▲▼".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_lines_splits_on_newlines() {
        let out = display_lines("one\ntwo\nthree", None);
        assert_eq!(out, vec!["one", "two", "three"]);
    }

    #[test]
    fn human_tokens_scales() {
        assert_eq!(human_tokens(0), "0");
        assert_eq!(human_tokens(942), "942");
        assert_eq!(human_tokens(12_345), "12.3k");
        assert_eq!(human_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn display_lines_empty_content_yields_single_blank() {
        // Rendering sites rely on at least one line so the first-line prefix
        // (e.g. "  < ") always shows, even for empty tool output.
        assert_eq!(display_lines("", None), vec![String::new()]);
    }

    #[test]
    fn display_lines_preserves_trailing_empty_line() {
        // split('\n') on "a\n" yields ["a", ""]; lines() would drop the
        // trailing empty. Keep split semantics so the blank shows.
        assert_eq!(display_lines("a\n", None), vec!["a", ""]);
    }

    #[test]
    fn display_lines_truncates_before_splitting() {
        let out = display_lines("aaaa\nbbbb\ncccc", Some(5));
        assert_eq!(out, vec!["aaaa".to_string(), "…".to_string()]);
    }

    #[test]
    fn model_slug_strips_provider_prefix() {
        assert_eq!(model_slug("anthropic/claude-opus-4-7"), "claude-opus-4-7");
        assert_eq!(model_slug("openai/gpt-5-mini"), "gpt-5-mini");
    }

    #[test]
    fn model_slug_passes_through_bare_id() {
        assert_eq!(model_slug("gpt-5-mini"), "gpt-5-mini");
        assert_eq!(model_slug(""), "");
    }

    #[test]
    fn model_slug_uses_last_segment_for_nested_ids() {
        // OpenRouter free tier appends `:free` etc.; we want the leaf.
        assert_eq!(
            model_slug("provider/family/qwen-2.5-coder:free"),
            "qwen-2.5-coder:free"
        );
    }

    #[test]
    fn ctx_pct_basic_and_rounding() {
        assert_eq!(ctx_pct(64_000, 128_000), Some(50));
        // Rounds to nearest.
        assert_eq!(ctx_pct(1, 3), Some(33));
        assert_eq!(ctx_pct(2, 3), Some(67));
    }

    #[test]
    fn ctx_pct_unknown_budget_is_hidden() {
        // Zero budget -> None so the segment is omitted, never a divide-by-zero.
        assert_eq!(ctx_pct(1000, 0), None);
        assert_eq!(ctx_pct(0, 0), None);
    }

    #[test]
    fn ctx_pct_reports_over_budget_unclamped() {
        // A turn packed past a later-lowered budget shows >100, not a clamp.
        assert_eq!(ctx_pct(200_000, 128_000), Some(156));
    }

    #[test]
    fn format_price_renders_dash_for_missing() {
        assert_eq!(format_price(None), "—");
    }

    #[test]
    fn format_price_uses_two_decimals_for_cents() {
        // Sub-dollar prices keep two decimals so $0.04 ≠ $0.15.
        assert_eq!(format_price(Some(0.04)), "$0.04");
        assert_eq!(format_price(Some(0.15)), "$0.15");
        assert_eq!(format_price(Some(0.80)), "$0.80");
    }

    #[test]
    fn format_price_uses_one_decimal_for_dollars() {
        assert_eq!(format_price(Some(3.0)), "$3.0");
        assert_eq!(format_price(Some(15.0)), "$15.0");
    }
}
