//! Ratatui rendering for the two TUI modes (chat + session picker).
//! Pure view functions — no mutation, no async.

use chaz_core::backends::BackendManager;
use chaz_core::config::Config;
use chaz_core::mcp::{McpRegistryEntry, McpServerStatus};
use chaz_core::server::Server;
use chaz_core::session::EntryType;
use chaz_core::util::truncate_chars;

use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use chrono::{DateTime, Utc};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::AgentDiffMode;
use super::AgentDiffView;
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

mod composer;
#[cfg(test)]
mod hub_tests;
#[cfg(test)]
mod list_tests;
mod settings;
#[cfg(test)]
mod settings_tests;

/// Last `/`-separated segment of a model id (`anthropic/claude-opus-4-7` →
/// `claude-opus-4-7`). Bare ids without `/` are returned as-is. Used for the
/// status bar so the slug stays readable without the provider prefix.
fn model_slug(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Last reported input size is an estimate of current occupancy, not a fresh
/// tokenization of the transcript. Zero can also mean missing provider usage.
fn context_segment(tab: &super::Tab) -> String {
    let last = tab
        .entries
        .iter()
        .rev()
        .find(|e| e.sender == tab.current_agent && e.entry_type == EntryType::Message);
    let used = last
        .and_then(|e| e.metadata.as_ref())
        .filter(|m| m.model == tab.effective_model)
        .and_then(|m| m.context_tokens)
        .filter(|&n| n > 0);
    let percentage = match used {
        Some(n) if tab.context_budget > 0 => {
            format!(
                " ({:.1}%)",
                f64::from(n) / tab.context_budget as f64 * 100.0
            )
        }
        _ => String::new(),
    };
    let used = used.map_or_else(
        || "unknown".to_string(),
        |n| format!("~{}", human_tokens(u64::from(n))),
    );
    let max = if tab.context_budget == 0 {
        "unknown".to_string()
    } else {
        human_tokens(tab.context_budget as u64)
    };
    format!(" | ctx {used}/{max}{percentage} tok")
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

/// Visible slice of a cursor-driven list.
///
/// These lists render as one `Paragraph` of lines, so keeping the selection on
/// screen means choosing which rows to build at all. Building every row and
/// letting the pane clip them is what let a cursor walk off the bottom edge
/// while the list itself never moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ListWindow {
    /// First row to build.
    pub first: usize,
    /// One past the last row to build.
    pub end: usize,
    /// Offset to persist in `App::scroll` for this list.
    pub offset: usize,
}

impl ListWindow {
    /// `heights[i]` is row `i`'s line count and `budget` the lines available
    /// for rows, both excluding the list's fixed header and footer.
    pub(super) fn new(
        heights: &[usize],
        cursor: Option<usize>,
        budget: usize,
        offset: usize,
    ) -> Self {
        Self::fit(heights.len(), |i| heights[i], cursor, budget, offset)
    }

    /// [`ListWindow::new`] for the common case where every row is `height`
    /// lines tall. The session picker holds the whole session catalog, so it
    /// takes this path rather than materializing a `heights` vector per frame.
    pub(super) fn uniform(
        len: usize,
        height: usize,
        cursor: Option<usize>,
        budget: usize,
        offset: usize,
    ) -> Self {
        Self::fit(len, |_| height, cursor, budget, offset)
    }

    /// The window follows the cursor rather than recentering, so navigating
    /// inside it doesn't jitter the list. It always contains the cursor's row,
    /// even when that row alone is taller than the budget — that row then
    /// renders clipped rather than vanishing.
    ///
    /// `cursor` is `None` when this list's cursor currently sits outside it.
    /// The session picker's pinned "New session" row is the only such case: a
    /// `Some` there would be a lie, and reading it as row 0 would snap the
    /// window back to the top whenever the user selected that row.
    fn fit(
        len: usize,
        height: impl Fn(usize) -> usize,
        cursor: Option<usize>,
        budget: usize,
        offset: usize,
    ) -> Self {
        if len == 0 {
            return Self {
                first: 0,
                end: 0,
                offset: 0,
            };
        }
        // Everything fits: park at the top rather than honoring an offset left
        // over from when the list was longer.
        if (0..len).map(&height).sum::<usize>() <= budget {
            return Self {
                first: 0,
                end: len,
                offset: 0,
            };
        }
        let cursor = cursor.map(|c| c.min(len - 1));
        // Never leave the window past the last row — a list that shrank
        // self-corrects here, with no explicit offset reset — and never below
        // a cursor that has walked back above it.
        let mut first = offset.min(len - 1);
        if let Some(c) = cursor {
            first = first.min(c);
        }
        // Scroll down while the rows from `first` through the cursor overflow.
        if let Some(c) = cursor {
            let mut through_cursor: usize = (first..=c).map(&height).sum();
            while first < c && through_cursor > budget {
                through_cursor -= height(first);
                first += 1;
            }
        }
        // Fill whatever budget is left with the rows below.
        let mut end = first;
        let mut used = 0usize;
        while end < len && used + height(end) <= budget {
            used += height(end);
            end += 1;
        }
        // Always build at least one row, and always the cursor's — the second
        // is what keeps a row taller than the budget on screen, clipped.
        let floor = first + 1;
        let floor = cursor.map_or(floor, |c| floor.max(c + 1));
        Self {
            first,
            end: end.max(floor).min(len),
            offset: first,
        }
    }

    /// Row indices to build, in order.
    pub(super) fn rows(&self) -> std::ops::Range<usize> {
        self.first..self.end
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
    app.settings_list_area = None;
    app.settings_reader.viewport = None;
    if let Some(picker) = app.settings_picker.as_mut() {
        picker.viewport = Rect::default();
    }

    // Mirror the fast-start gate so ui_chat (which has no `server` handle)
    // can show the "reconciling agents…" indicator until the deferred
    // at-startup work releases it.
    app.startup_ready = server.is_startup_ready();
    // Catalog discovery and agent-default edits need not write the session DB.
    // Resolve pins against live defaults, windows and caps on every draw.
    if let Some(tab) = app.current_mut() {
        let agent = server.agents().get(&tab.current_agent);
        let requested_model = tab
            .model_pin
            .as_deref()
            .or_else(|| agent.as_ref().and_then(|a| a.default_model.as_deref()));
        tab.effective_model = backend.resolve_model_name(requested_model);
        let context_model = requested_model
            .map(str::to_string)
            .or_else(|| backend.default_model())
            .unwrap_or_default();
        let cap = agent.and_then(|a| a.max_context_tokens);
        tab.context_budget = server.effective_context_budget(&context_model, cap);
        for row in &mut tab.roster {
            if row.name == tab.current_agent {
                row.model.clone_from(&tab.effective_model);
            }
        }
    }

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

    // Extension status segments rendered on a dedicated second status
    // line. `app.status_segments` is refreshed by the run loop from the
    // session's `extension_outputs` store (the daemon writes it at the turn
    // boundary). Cloned so `ui_chat` can still take `&mut App`.
    let ext_segments = app.status_segments.clone();

    match app.mode {
        TuiMode::Chat => ui_chat(f, app, &ext_segments),
        TuiMode::SessionPicker => ui_picker(f, app),
        TuiMode::ModelPicker => ui_model_picker(f, app),
        TuiMode::Settings(scope) => settings::ui_settings(f, app, scope, server, backend, config),
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

fn ui_chat(f: &mut ratatui::Frame, app: &mut App, ext_segments: &[String]) {
    // 4-line approval panel when a tool is waiting on the user; 0 otherwise.
    let has_approval = app.active().pending_approval.is_some();
    let approval_h: u16 = if has_approval { 4 } else { 0 };
    // Dedicated 1-line strip for extension-contributed status segments,
    // sitting just above the core status bar so it never fights the
    // `agent | model | messages` line. Hidden (height 0) when no
    // extension has contributed a segment.
    let ext_status_h: u16 = if ext_segments.is_empty() { 0 } else { 1 };
    // 1-line tab bar at the top. Always present even with one tab so the user
    // has a consistent affordance.
    let composer = composer::layout(&app.input, app.cursor, f.area().width.saturating_sub(2));
    // Reserve a transcript row even when the draft is taller than the screen.
    let composer_h = (composer.lines.len().min(u16::MAX as usize) as u16)
        .saturating_add(2)
        .min(
            f.area()
                .height
                .saturating_sub(3 + approval_h + ext_status_h)
                .max(3),
        );
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(approval_h),
        Constraint::Length(ext_status_h),
        Constraint::Length(1),
        Constraint::Length(composer_h),
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

        match &entry.entry_type {
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

                let source = entry
                    .routing
                    .as_ref()
                    .and_then(|r| r.source.as_ref())
                    .map(|r| format!(" [via {} {}]", r.transport, r.channel))
                    .unwrap_or_else(|| {
                        if is_agent {
                            " [local]".into()
                        } else {
                            String::new()
                        }
                    });
                let label = format!("{}{}{}:", debug_prefix, entry.sender, source);

                lines.push(Line::from(vec![Span::styled(label, sender_style)]));

                for content_line in entry.content.lines() {
                    lines.push(Line::from(format!("  {content_line}")));
                }
                lines.push(Line::from(""));
            }
            EntryType::MatrixObserved | EntryType::MatrixSend | EntryType::BridgeEvent => {
                use chaz_core::session::BridgeEventRole;
                let Some(role) = entry.bridge_role() else {
                    continue;
                }; // future role is inert
                if role == BridgeEventRole::Receipt {
                    continue;
                }
                let (label, address) = if role == BridgeEventRole::Observation {
                    (
                        "observed, no wake",
                        entry.routing.as_ref().and_then(|r| r.source.as_ref()),
                    )
                } else {
                    let dest = entry.routing.as_ref().and_then(|r| r.destinations.first());
                    let id = dest.and_then(|d| d.message_id.as_deref());
                    let delivered = tab.entries.iter().any(|e| {
                        e.bridge_role() == Some(BridgeEventRole::Receipt)
                            && e.routing.as_ref().and_then(|r| r.reply_to.as_deref()) == id
                    });
                    (if delivered { "sent" } else { "pending" }, dest)
                };
                let transport = address.map(|a| a.transport.as_str()).unwrap_or("external");
                let channel = address
                    .map(|a| a.channel.as_str())
                    .unwrap_or("unknown channel");
                lines.push(Line::from(vec![Span::styled(
                    format!(
                        "{debug_prefix}{} [{transport} {label} in {channel}]:",
                        entry.sender
                    ),
                    dim,
                )]));
                if let Some(body) = entry.bridge_body() {
                    for content_line in body.lines() {
                        lines.push(Line::from(format!("  {content_line}")));
                    }
                }
                lines.push(Line::from(""));
            }
            EntryType::MatrixSent | EntryType::Unknown { .. } => {} // Inert audit records.
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
                    format!("{debug_prefix}{} acknowledged turn", entry.sender),
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
            EntryType::ApprovalRequest => {
                let label = chaz_core::bridge::parse_approval_request(entry)
                    .map(|p| format!("🔒 approval requested: {}", p.tool_name))
                    .unwrap_or_else(|| "🔒 approval requested".to_string());
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}  {label}"),
                    dim,
                )]));
            }
            EntryType::ApprovalDecision => {
                let label = chaz_core::bridge::parse_approval_decision(entry)
                    .map(|p| format!("🔓 approval {}", p.decision))
                    .unwrap_or_else(|| "🔓 approval decision".to_string());
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}  {label}"),
                    dim,
                )]));
            }
        }
    }

    if let Some(line) = turn_activity_line(tab.active_turns) {
        lines.push(line);
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
    // Only the host's latest message belongs with its effective input budget;
    // aggregate prompt usage sums ReAct calls and is not context occupancy.
    let ctx_segment = context_segment(tab);
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
    // Fast-start: the TUI draws before the deferred at-startup work (agent
    // reconcile) finishes. Surface that so a held first turn doesn't look
    // like a hang. Clears itself once the gate opens.
    let startup_indicator = if app.startup_ready {
        ""
    } else {
        " | ⟳ reconciling agents…"
    };

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

    // A normal-width toolbar must keep the numeric pair visible even when a
    // session has a long name. Clip only the label, using terminal-cell widths.
    let label_width = (chunks[4].width as usize).saturating_sub(ctx_segment.width() + 1);
    let session_label = if session_label.width() > label_width {
        let mut cells = 0;
        let mut label: String = session_label
            .graphemes(true)
            .take_while(|g| {
                cells += g.width();
                cells <= label_width.saturating_sub(1)
            })
            .collect();
        if label_width > 0 {
            label.push('…');
        }
        label
    } else {
        session_label
    };
    let make_status = |agent_segment: &str| {
        format!(
            " {session_label}{ctx_segment}{agent_segment}{usage_segment}{debug_indicator}{expand_indicator}{startup_indicator}"
        )
    };
    let mut status_text = make_status(&agent_segment);

    // If the full roster would overflow the status line, collapse it to a
    // count plus the host, leaving the per-agent detail to the Settings page.
    if multi_agent && status_text.chars().count() > chunks[4].width as usize {
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
    // Extension status strip — only drawn when an extension contributed a
    // segment (otherwise `ext_status_h` is 0 and `chunks[3]` is empty).
    // Segments are already ordered alphabetically by key (BTreeMap on the
    // hub side); join with a separator, width-truncate, and render dim on
    // the same dark background as the core status bar below it.
    if !ext_segments.is_empty() {
        let joined = format!(" {}", ext_segments.join(" │ "));
        let ext_text = truncate_chars(&joined, chunks[3].width as usize);
        let ext_status = Paragraph::new(ext_text).style(
            Style::default()
                .bg(Color::Rgb(0x1a, 0x1d, 0x26))
                .fg(COLOR_DIM),
        );
        f.render_widget(ext_status, chunks[3]);
    }

    let status = Paragraph::new(status_text).style(
        Style::default()
            .bg(Color::Rgb(0x1a, 0x1d, 0x26))
            .fg(Color::White),
    );
    f.render_widget(status, chunks[4]);

    let visible_h = chunks[5].height.saturating_sub(2) as usize;
    // Keep the cursor visible while the draft exceeds its available height.
    let start = composer
        .cursor_row
        .saturating_sub(visible_h.saturating_sub(1));
    let input = Paragraph::new(composer.lines[start..].join("\n")).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_DIM))
            .title(Span::styled(" > ", Style::default().fg(COLOR_ACCENT))),
    );
    f.render_widget(input, chunks[5]);

    // Completion popup floats just above the input box, over the bottom of
    // the transcript. Drawn after the transcript so it sits on top.
    render_completion_popup(f, app, chunks[1], chunks[5]);

    if visible_h > 0 {
        f.set_cursor_position((
            chunks[5].x + composer.cursor_col + 1,
            chunks[5].y + (composer.cursor_row - start) as u16 + 1,
        ));
    }
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
pub(super) fn turn_activity_line(active_turns: usize) -> Option<Line<'static>> {
    (active_turns > 0).then(|| {
        Line::from(Span::styled(
            "  thinking...",
            Style::default().fg(COLOR_DIM),
        ))
    })
}

fn render_tab_bar(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let n = app.tabs.len();
    // Every view is closable: closing the last one returns to the hub.
    let show_close = true;
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
    // The widest key hint that fits at the right side.
    let used = x.saturating_sub(area.x);
    let remaining = area.width.saturating_sub(used) as usize;
    if let Some(hint) = [
        " Ctrl+P sessions · Ctrl+, settings · Ctrl+PgUp/PgDn · Ctrl+W close",
        " Ctrl+P sessions · Ctrl+, settings · Ctrl+W close",
        " Ctrl+P sessions",
    ]
    .into_iter()
    .find(|hint| hint.chars().count() <= remaining)
    {
        let pad = remaining - hint.chars().count();
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
    let (value, suffix) = if n < 1000 {
        return n.to_string();
    } else if n < 1_000_000 {
        (n as f64 / 1000.0, "k")
    } else {
        (n as f64 / 1_000_000.0, "M")
    };
    let value = format!("{value:.1}");
    format!("{}{suffix}", value.trim_end_matches(".0"))
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

pub(super) fn ui_picker(f: &mut ratatui::Frame, app: &mut App) {
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

    // Session rows below the pinned row are windowed, so a cursor moving past
    // the fold scrolls the list instead of walking off the bottom edge — which
    // is exactly what happened when every row was built and the pane clipped
    // them. `y_off` is now the first line below the pinned row, which is
    // where the first windowed row draws.
    let rows_y0 = y_off;
    let row_h: u16 = 2;
    let window = ListWindow::uniform(
        app.session_list.len(),
        row_h as usize,
        app.picker_index.checked_sub(1),
        inner_h.saturating_sub(rows_y0) as usize,
        app.scroll.picker,
    );
    app.scroll.picker = window.offset;
    // Include a partially visible row, but never the window's forced row when
    // no session line fits below the pinned New session row.
    let visible_count = if inner_w == 0 {
        0
    } else {
        inner_h.saturating_sub(rows_y0).div_ceil(row_h) as usize
    };
    app.picker_visible_rows = window.first..window.end.min(window.first + visible_count);

    if app.session_list.is_empty() {
        // Empty, loading and failed are distinct: a failure never reads as
        // "no sessions". The list is this peer's local catalog.
        let message = if app.session_catalog_loading {
            "  Loading sessions…".to_string()
        } else if let Some(error) = &app.session_picker_error {
            format!("  Failed to load sessions: {error} — Ctrl+P retries.")
        } else {
            "  No saved sessions yet — select \"New session\" above.".to_string()
        };
        lines.push(Line::from(vec![Span::styled(
            message,
            Style::default().fg(COLOR_DIM),
        )]));
    } else {
        let current_session_db_id = app.current().map(|tab| tab.session_db_id.clone());
        let now = Utc::now();
        for i in window.rows() {
            let info = &app.session_list[i];
            let is_selected = i + 1 == app.picker_index;
            let is_current = current_session_db_id.as_deref() == Some(info.session_db_id.as_str());

            let marker = if is_selected { "> " } else { "  " };
            let current_marker = if is_current { " *" } else { "" };

            let agent_str = info.agent_name.as_deref().unwrap_or("default");
            let title = match &info.name {
                Some(n) => format!("\"{n}\""),
                None => short_session_id(&info.session_db_id),
            };
            let bridge = info.bridge.as_str();
            let age = humanize_age(info.created_at, now);
            let closed_suffix = match info.status {
                chaz_core::session::SessionStatus::Closed => " (closed)",
                chaz_core::session::SessionStatus::Active => "",
            };

            // Name is available from the peer-local index. Agent metadata is
            // loaded only for rows visible in the picker viewport.
            let header = if info.loaded {
                format!(
                    "{marker}{title}{current_marker} [{bridge}] {agent_str} • {age}{closed_suffix}"
                )
            } else {
                format!("{marker}{title}{current_marker} [{bridge}] … • {age}{closed_suffix}")
            };

            let is_closed = matches!(info.status, chaz_core::session::SessionStatus::Closed);
            let style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if !info.loaded {
                // Dim until loaded so the eye skips over rows still filling in.
                Style::default().fg(COLOR_DIM)
            } else if is_closed {
                Style::default().fg(COLOR_DIM)
            } else if is_current {
                Style::default().fg(COLOR_ACCENT)
            } else {
                Style::default().fg(COLOR_USER)
            };

            // Screen row of this row inside the pane, rebased on the window
            // so a scrolled list still hit-tests where it actually drew.
            let row_y = rows_y0 + (i - window.first) as u16 * row_h;
            if row_y < inner_h {
                let clipped_h = row_h.min(inner_h - row_y);
                if clipped_h > 0 {
                    app.click_regions.push(ClickRegion {
                        x: inner_x,
                        y: inner_y + row_y,
                        w: inner_w,
                        h: clipped_h,
                        target: ClickTarget::PickerSelect(i),
                    });
                }
            }

            lines.push(Line::from(vec![Span::styled(header, style)]));

            lines.push(Line::from(""));
        }
    }

    let list = Paragraph::new(lines).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_DIM))
            .title(Span::styled(
                format!(
                    " Sessions{} ",
                    scroll_indicator(window.first, window.end, app.session_list.len())
                ),
                Style::default().fg(COLOR_ACCENT),
            )),
    );
    f.render_widget(list, chunks[0]);

    // Footer: a one-shot notice (failed open/create), else the keys the hub
    // actually owns. Esc/Ctrl+P lead back only when a conversation is open.
    let bar = Style::default()
        .bg(Color::Rgb(0x1a, 0x1d, 0x26))
        .fg(Color::White);
    let footer = if let Some(notice) = &app.hub_notice {
        Paragraph::new(format!(" {notice}")).style(bar.fg(COLOR_ERROR))
    } else {
        let back = if app.tabs.is_empty() {
            ""
        } else {
            " · Esc back"
        };
        let width = chunks[1].width as usize;
        let hint = [
            format!(
                " ↑↓ select · Enter open · n new · r rename · s peer settings{back} · Ctrl+C quit"
            ),
            format!(" ↑↓ · Enter open · n new · r rename · s settings{back}"),
            format!(" Enter open · n new · s settings{back}"),
        ]
        .into_iter()
        .find(|hint| hint.chars().count() <= width)
        .unwrap_or_else(|| format!(" n new · s settings{back}"));
        Paragraph::new(hint).style(bar)
    };
    f.render_widget(footer, chunks[1]);
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
        .or_else(|| app.current().map(|tab| tab.effective_model.clone()))
        .unwrap_or_default();
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
mod chat_frame_tests {
    //! Frame-level checks on the composer: the growth cap, the scroll offset
    //! that keeps the cursor visible, and the cursor's terminal coordinates.
    //! `composer::layout` is tested in isolation next door; these cover the
    //! arithmetic in `ui_chat` that turns that layout into a drawn frame.

    use super::super::{App, Tab};
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::HashSet;

    async fn test_app(input: &str, cursor: usize) -> App {
        let (_instance, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewUser::passwordless("composer-frame"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let tab = Tab {
            session_db_id: db.root_id().to_string(),
            session_db: db,
            entries: Vec::new(),
            scroll_offset: 0,
            pending_approval: None,
            active_turns: 0,
            current_agent: "chaz".into(),
            session_name: None,
            effective_model: String::new(),
            roster: Vec::new(),
            context_budget: 0,
            model_pin: None,
            expanded_entries: HashSet::new(),
        };
        let mut app = App::new(HashSet::new(), tab);
        app.input = input.to_string();
        app.cursor = cursor;
        app
    }

    /// Draw one chat frame and return (rows, cursor position).
    fn draw(app: &mut App, width: u16, height: u16) -> (Vec<String>, (u16, u16)) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| super::ui_chat(f, app, &[]))
            .expect("draw");
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        (rows, (cursor.x, cursor.y))
    }

    #[tokio::test]
    async fn toolbar_context_uses_last_host_call_and_current_model_budget() {
        use chaz_core::runtime::{ResponseMetadata, TokenUsage};
        use chaz_core::session::{EntryType, SessionEntry};
        let mut app = test_app("", 0).await;
        let tab = app.active_mut();
        tab.effective_model = "large-model".into();
        tab.context_budget = 1_050_000;
        let entry = SessionEntry {
            sender: "chaz".into(),
            content: "fixture reply".into(),
            timestamp: chrono::Utc::now(),
            entry_type: EntryType::Message,
            metadata: Some(ResponseMetadata {
                model: "large-model".into(),
                provider: None,
                response_id: None,
                usage: TokenUsage {
                    prompt_tokens: 9_999_999,
                    ..Default::default()
                },
                context_tokens: Some(12_345),
                extra: Default::default(),
            }),
            routing: None,
        };
        tab.entries.push(entry.clone());
        let mut other = entry.clone();
        other.sender = "guest".into();
        other.metadata.as_mut().unwrap().context_tokens = Some(900_000);
        tab.entries.push(other);
        let screen = draw(&mut app, 100, 16).0.join("\n");
        assert!(screen.contains("ctx ~12.3k/1.1M (1.2%) tok"), "{screen}");

        app.active_mut().entries[0]
            .metadata
            .as_mut()
            .unwrap()
            .context_tokens = Some(57_000);
        app.active_mut().context_budget = 1_000_000;
        let screen = draw(&mut app, 100, 16).0.join("\n");
        assert!(screen.contains("ctx ~57k/1M (5.7%) tok"), "{screen}");

        // New model, same session: don't pair its budget with an old count.
        app.active_mut().effective_model = "small-model".into();
        app.active_mut().context_budget = 32_000;
        let screen = draw(&mut app, 80, 16).0.join("\n");
        assert!(screen.contains("ctx unknown/32k tok"), "{screen}");
        // A new response restores usage; a cap changes only the denominator.
        let mut latest = entry;
        latest.metadata.as_mut().unwrap().model = "small-model".into();
        latest.metadata.as_mut().unwrap().context_tokens = Some(20_001);
        app.active_mut().entries.push(latest);
        app.active_mut().context_budget = 16_000;
        let screen = draw(&mut app, 80, 16).0.join("\n");
        assert!(screen.contains("ctx ~20k/16k (125.0%) tok"), "{screen}");
        // A known count without a budget cannot produce a percentage.
        app.active_mut().context_budget = 0;
        let screen = draw(&mut app, 80, 16).0.join("\n");
        assert!(screen.contains("ctx ~20k/unknown tok"), "{screen}");
        app.active_mut().context_budget = 16_000;
        // Missing/zero usage on the latest message must not resurrect older data.
        app.active_mut()
            .entries
            .last_mut()
            .unwrap()
            .metadata
            .as_mut()
            .unwrap()
            .context_tokens = Some(0);
        let screen = draw(&mut app, 80, 16).0.join("\n");
        assert!(screen.contains("ctx unknown/16k tok"), "{screen}");
        app.active_mut().entries.last_mut().unwrap().metadata = None;
        app.active_mut().context_budget = 0;
        let screen = draw(&mut app, 80, 16).0.join("\n");
        assert!(screen.contains("ctx unknown/unknown tok"), "{screen}");
        app.active_mut().entries.clear();
        app.active_mut().context_budget = 1_050_000;
        let screen = draw(&mut app, 80, 16).0.join("\n");
        assert!(screen.contains("ctx unknown/1.1M tok"), "{screen}");
    }

    #[tokio::test]
    async fn toolbar_context_remains_visible_with_long_session_names() {
        let mut app = test_app("", 0).await;
        app.active_mut().context_budget = 1_050_000;
        for name in ["long session name ".repeat(8), "界e\u{301}".repeat(40)] {
            app.active_mut().session_name = Some(name);
            let screen = draw(&mut app, 80, 16).0.join("\n");
            assert!(screen.contains("ctx unknown/1.1M tok"), "{screen}");
        }
    }

    #[tokio::test]
    async fn picker_scroll_keeps_selected_row_visible_and_rebases_click_targets() {
        use super::super::{ClickTarget, TuiMode};
        use chaz_core::commands::SessionInfo;
        use chaz_core::session::{BridgeKind, SessionIndex, SessionStatus};
        let mut app = test_app("", 0).await;
        app.mode = TuiMode::SessionPicker;
        app.session_list = (0..30)
            .map(|i| {
                SessionInfo::placeholder(&SessionIndex {
                    session_db_id: format!("session-{i}"),
                    source: None,
                    bridge: BridgeKind::Tui,
                    created_at: None,
                    status: SessionStatus::Active,
                    name: Some(format!("item-{i}-a-very-long-session-name")),
                })
            })
            .collect();
        app.picker_index = 25;
        let mut terminal = Terminal::new(TestBackend::new(32, 12)).unwrap();
        terminal.draw(|f| super::ui_picker(f, &mut app)).unwrap();
        let screen = (0..12)
            .map(|y| {
                (0..32)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(screen.iter().any(|r| r.contains("item-24")), "{screen:?}");
        assert!(screen.iter().any(|r| r.contains("New session")));
        let selected = app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::PickerSelect(24)))
            .unwrap();
        assert!(screen[selected.y as usize].contains("item-24"));
        assert!(app.click_regions.iter().all(|r| r.y + r.h <= 11));
        for hit in &app.click_regions {
            if let ClickTarget::PickerSelect(i) = hit.target {
                assert!(
                    screen[hit.y as usize].contains(&format!("item-{i}")),
                    "{screen:?}"
                );
            }
        }
        let hit = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::PickerSelect(i) if i != 24))
            .unwrap();
        let ClickTarget::PickerSelect(clicked) = hit.target else {
            unreachable!()
        };
        super::super::input::handle_mouse(
            &mut app,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: hit.x,
                row: hit.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(app.picker_index, clicked + 1);
        let new = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::PickerNew))
            .unwrap();
        super::super::input::handle_mouse(
            &mut app,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: new.x,
                row: new.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(app.picker_index, 0);
        terminal
            .draw(|f| {
                app.click_regions.clear();
                super::ui_picker(f, &mut app)
            })
            .unwrap();
        assert!(
            app.scroll.picker > 0,
            "pinned row should not reset session viewport"
        );
        app.picker_index = 30;
        let mut narrow = Terminal::new(TestBackend::new(12, 8)).unwrap();
        app.click_regions.clear();
        narrow.draw(|f| super::ui_picker(f, &mut app)).unwrap();
        assert!(
            app.click_regions
                .iter()
                .all(|r| r.y + r.h <= 7 && r.x + r.w <= 12)
        );
        assert!(
            app.click_regions
                .iter()
                .any(|r| matches!(r.target, ClickTarget::PickerSelect(29)))
        );
    }

    #[tokio::test]
    async fn picker_wheel_moves_selection_without_scrolling_chat() {
        use super::super::{TuiMode, input};
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let mut app = test_app("", 0).await;
        app.mode = TuiMode::SessionPicker;
        app.session_list = (0..10)
            .map(|i| {
                chaz_core::commands::SessionInfo::placeholder(&chaz_core::session::SessionIndex {
                    session_db_id: format!("s-{i}"),
                    source: None,
                    bridge: chaz_core::session::BridgeKind::Tui,
                    created_at: None,
                    status: chaz_core::session::SessionStatus::Active,
                    name: None,
                })
            })
            .collect();
        input::handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 2,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.picker_index, 3);
        assert_eq!(app.active().scroll_offset, 0);
    }

    #[tokio::test]
    async fn settings_models_frame_windows_rows_and_clicks_at_narrow_size() {
        use super::super::{ClickTarget, SessionMetaSnapshot};
        use chaz_core::backends::BackendManager;
        use chaz_core::security::SecretStore;
        use chaz_core::session::AgentRef;
        use std::collections::HashMap;
        let mut app = test_app("", 0).await;
        let secrets = SecretStore::new(app.active().session_db.clone()).await;
        let backend = BackendManager::new(&None, secrets);
        app.session_settings_snapshot = Some(SessionMetaSnapshot {
            session_db_id: "test".into(),
            model_pin: None,
            agent_models: HashMap::new(),
            agents: (0..25)
                .map(|i| AgentRef {
                    db_id: format!("db-{i}"),
                    display_name: format!("agent-{i}"),
                    home_pubkey: None,
                })
                .collect(),
            host_agent_db_id: None,
            created_at: None,
            entry_count: 0,
        });
        app.session_models_cursor = 24;
        let mut terminal = Terminal::new(TestBackend::new(28, 10)).unwrap();
        terminal
            .draw(|f| super::settings::render_session_models(f, f.area(), &mut app, &backend))
            .unwrap();
        let rows = (0..10)
            .map(|y| {
                (0..28)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let selected = app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::SettingsDetailRow(24)))
            .unwrap();
        assert!(rows[selected.y as usize].contains("agent-23"), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("agent-0")));
        assert!(app.click_regions.iter().all(|r| r.y + r.h <= 10));
    }

    #[test]
    fn variable_height_window_keeps_cursor_visible() {
        let heights = [1, 6, 1, 1, 1, 1];
        let window = super::ListWindow::new(&heights, Some(5), 3, 0);
        assert!(window.rows().contains(&5));
        assert!(window.first > 1);
    }

    /// Composer rows for a frame: the bordered box occupies the bottom rows, so
    /// read them back off the tail of the buffer.
    fn composer_rows(rows: &[String], box_height: usize) -> Vec<String> {
        rows[rows.len() - box_height..].to_vec()
    }

    #[tokio::test]
    async fn long_draft_wraps_and_grows_instead_of_running_off_the_edge() {
        // 18 cells of draft in a 12-wide terminal: 10 usable columns inside the
        // border, so two full rows plus a partial third.
        let draft = "a".repeat(18);
        let mut app = test_app(&draft, draft.len()).await;
        let (rows, cursor) = draw(&mut app, 12, 14);
        // Two wrapped rows plus the top and bottom border: the box grew from 3.
        let box_rows = composer_rows(&rows, 4);
        assert_eq!(box_rows[1], "│aaaaaaaaaa│");
        assert_eq!(box_rows[2], "│aaaaaaaa  │");
        // Cursor sits just past the last grapheme on the second wrapped row,
        // inside the border — not off the right edge.
        assert_eq!(cursor, (9, 12));
    }

    #[tokio::test]
    async fn wide_graphemes_place_the_cursor_by_display_cell() {
        // Byte-indexed cursor math would put this at x=7 (6 bytes + border);
        // three double-width cells are 6 display cells, so x=7 by cells too —
        // use a draft where the two disagree: 3 chars, 6 bytes, 6 cells.
        let draft = "界界界";
        let mut app = test_app(draft, draft.len()).await;
        let (_rows, cursor) = draw(&mut app, 20, 14);
        assert_eq!(draft.len(), 9, "3-byte chars: byte math would give x=10");
        assert_eq!(cursor, (7, 12));
    }

    #[tokio::test]
    async fn tall_draft_is_capped_and_scrolls_to_keep_the_cursor_visible() {
        // 40 rows of wrapped draft in a 10-row terminal cannot all be drawn.
        let draft = "b".repeat(200);
        let mut app = test_app(&draft, draft.len()).await;
        let (rows, cursor) = draw(&mut app, 12, 10);
        // The composer is capped rather than swallowing the frame: the tab bar,
        // the transcript region and the status line all still get a row, and the
        // box starts below them.
        assert!(
            rows[3].starts_with("╭ > "),
            "composer top row {:?}",
            rows[3]
        );
        assert!(
            rows[9].starts_with('╰'),
            "composer bottom row {:?}",
            rows[9]
        );
        // The draft is 20 wrapped rows; only the tail around the cursor is
        // drawn, and the cursor lands on the last row inside the box.
        assert_eq!(cursor, (1, 8));
        assert_eq!(rows[7], "│bbbbbbbbbb│");
    }

    #[tokio::test]
    async fn empty_draft_keeps_the_three_row_box_and_home_cursor() {
        let mut app = test_app("", 0).await;
        let (rows, cursor) = draw(&mut app, 20, 14);
        let box_rows = composer_rows(&rows, 3);
        assert!(box_rows[0].starts_with('╭'), "top border {:?}", box_rows[0]);
        assert!(
            box_rows[2].starts_with('╰'),
            "bottom border {:?}",
            box_rows[2]
        );
        assert_eq!(cursor, (1, 12));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn thinking_indicator_is_drawn_only_for_a_live_claim() {
        use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
        let mut terminal = Terminal::new(TestBackend::new(24, 1)).unwrap();
        assert!(super::turn_activity_line(0).is_none());
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(super::turn_activity_line(1).unwrap()),
                    frame.area(),
                );
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("thinking..."),
            "actual TUI buffer: {rendered}"
        );
    }
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
        assert_eq!(human_tokens(1_000), "1k");
        assert_eq!(human_tokens(12_345), "12.3k");
        assert_eq!(human_tokens(57_000), "57k");
        assert_eq!(human_tokens(1_000_000), "1M");
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
