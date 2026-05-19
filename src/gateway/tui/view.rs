//! Ratatui rendering for the two TUI modes (chat + session picker).
//! Pure view functions — no mutation, no async.

use crate::session::EntryType;
use crate::util::truncate_chars;

use chrono::{DateTime, Utc};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::App;
use super::ClickRegion;
use super::ClickTarget;
use super::Overlay;
use super::TuiMode;
use super::short_session_id;

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

pub(super) fn ui(f: &mut ratatui::Frame, app: &mut App) {
    // Click regions are rebuilt from scratch each frame so coordinates match
    // what the user is currently seeing.
    app.click_regions.clear();

    match app.mode {
        TuiMode::Chat => ui_chat(f, app),
        TuiMode::SessionPicker => ui_picker(f, app),
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
        .title(" Help — Esc to close · ↑↓/PgUp/PgDn/wheel scroll · click a row to insert ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
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
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
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
                Span::styled(format!("  {cmd}"), Style::default().fg(Color::Green)),
                Span::raw(" "),
                Span::styled(*desc, Style::default().fg(Color::Gray)),
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

    let block = Block::bordered().title(format!(" {title} ")).title_style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    let input_widget = Paragraph::new(input.as_str());
    f.render_widget(input_widget, chunks[0]);

    let help = Paragraph::new(" [Enter] save · empty = clear · [Esc] cancel")
        .style(Style::default().fg(Color::DarkGray));
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

    let mut lines: Vec<Line> = Vec::new();

    let tab = app.active();
    for entry in &tab.entries {
        let debug_prefix = if app.debug_mode {
            let ts = entry.timestamp.format("%H:%M:%S");
            let typ = format!("{:?}", entry.entry_type);
            format!("[{ts} {typ:<10}] ")
        } else {
            String::new()
        };
        let dim = Style::default().fg(Color::DarkGray);

        match entry.entry_type {
            EntryType::Message => {
                let is_agent = app.agent_names.contains(&entry.sender);
                let is_system = entry.sender == "system";
                let sender_style = if is_system {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if is_agent {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                };

                let label = format!("{}{}:", debug_prefix, entry.sender);

                lines.push(Line::from(vec![Span::styled(label, sender_style)]));

                for content_line in entry.content.lines() {
                    lines.push(Line::from(format!("  {content_line}")));
                }
                lines.push(Line::from(""));
            }
            // Directives are inputs to the agent (heartbeat, spawn_agent,
            // spawn_task, explicit user directive) — render with ToolResult
            // formatting so they group with tool output for future collapse UX.
            EntryType::Directive => {
                let max_chars = if app.debug_mode { 500 } else { 120 };
                let head = format!("{} (directive): ", entry.sender);
                for (i, l) in display_lines(&entry.content, Some(max_chars))
                    .into_iter()
                    .enumerate()
                {
                    let text = if i == 0 {
                        format!("{debug_prefix}  < {head}{l}")
                    } else {
                        format!("    {l}")
                    };
                    lines.push(Line::from(vec![Span::styled(text, dim)]));
                }
            }
            EntryType::Ack => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}{} thinking...", entry.sender),
                    dim,
                )]));
            }
            EntryType::ToolCall => {
                for (i, l) in display_lines(&entry.content, None).into_iter().enumerate() {
                    let prefix = if i == 0 { "  > " } else { "    " };
                    lines.push(Line::from(vec![Span::styled(
                        format!("{debug_prefix}{prefix}{l}"),
                        dim,
                    )]));
                }
            }
            EntryType::ToolResult => {
                let max_chars = if app.debug_mode { 500 } else { 120 };
                for (i, l) in display_lines(&entry.content, Some(max_chars))
                    .into_iter()
                    .enumerate()
                {
                    let prefix = if i == 0 { "  < " } else { "    " };
                    lines.push(Line::from(vec![Span::styled(
                        format!("{debug_prefix}{prefix}{l}"),
                        dim,
                    )]));
                }
            }
            EntryType::Error => {
                let red = Style::default().fg(Color::Red);
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
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )]));
                for content_line in entry.content.lines() {
                    lines.push(Line::from(vec![Span::styled(
                        format!("  {content_line}"),
                        Style::default().fg(Color::Magenta),
                    )]));
                }
                lines.push(Line::from(""));
            }
        }
    }

    if tab.waiting {
        lines.push(Line::from(vec![Span::styled(
            "  thinking...",
            Style::default().fg(Color::DarkGray),
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
    let msg_count = tab
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Message)
        .count();
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
    let approval_info = tab.pending_approval.as_ref().map(|ex| {
        (
            ex.info.name.clone(),
            ex.info.risk_level.to_string(),
            ex.info.arguments_display.clone(),
        )
    });
    let _ = tab;

    let messages_area = chunks[1];
    let inner_width = messages_area.width.saturating_sub(2);
    let messages_height = messages_area.height.saturating_sub(2);

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let content_height = paragraph.line_count(inner_width).min(u16::MAX as usize) as u16;
    let scroll = if content_height > messages_height {
        content_height
            .saturating_sub(messages_height)
            .saturating_sub(scroll_offset)
    } else {
        0
    };

    let messages = paragraph
        .scroll((scroll, 0))
        .block(Block::bordered().title(" Chaz "));
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
    let status_text = format!(
        " {} | agent: {} | messages: {}{}{} | /help",
        session_label, current_agent, msg_count, usage_segment, debug_indicator
    );
    let status =
        Paragraph::new(status_text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(status, chunks[3]);

    let input = Paragraph::new(app.input.as_str()).block(Block::bordered().title(" > "));
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
        .title(" commands — ↑↓ select · Tab insert · Esc dismiss ")
        .title_style(Style::default().fg(Color::Yellow));
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
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };
        let desc_style = if is_sel {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::Gray)
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
    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;
    let row_y = area.y;
    for (i, tab) in app.tabs.iter().enumerate() {
        let is_active = i == app.active_tab;
        let title = tab.title();
        // Visual: " <title> " active inverted, others dim, + optional " × ".
        let title_label = format!(" {title} ");
        let close_label = if show_close { " × " } else { "" };
        let title_style = if is_active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray).bg(Color::DarkGray)
        };
        let close_style = if is_active {
            Style::default().fg(Color::Red).bg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray).bg(Color::DarkGray)
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
        // Small spacer gap between tabs.
        spans.push(Span::raw(" "));
        x = x.saturating_add(1);
    }
    // Hint text at the right side if space allows.
    let hint = " Ctrl+PgUp/PgDn · Ctrl+W";
    let used = x.saturating_sub(area.x);
    let remaining = area.width.saturating_sub(used);
    if remaining as usize >= hint.len() {
        let pad = remaining as usize - hint.len();
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(
            hint.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let line = Line::from(spans);
    let paragraph =
        Paragraph::new(vec![line]).style(Style::default().bg(Color::DarkGray).fg(Color::White));
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
        .title(format!(" Tool approval — {tool_name} ({risk}) "))
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Two rows: args preview, then button row.
    let args_preview = truncate_chars(args, inner.width as usize * 2);
    let args_line = Line::from(vec![
        Span::styled("args: ", Style::default().fg(Color::DarkGray)),
        Span::raw(args_preview.replace('\n', " ")),
    ]);
    let buttons = Line::from(vec![
        Span::styled(
            " [y] approve ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            " [n] deny ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            " [a] approve all ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
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
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
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
            Style::default().fg(Color::DarkGray),
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
                crate::session::SessionStatus::Closed => " (closed)",
                crate::session::SessionStatus::Active => "",
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

            let is_closed = matches!(info.status, crate::session::SessionStatus::Closed);
            let style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if is_closed {
                Style::default().fg(Color::DarkGray)
            } else if is_current {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
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
                    Style::default().fg(Color::DarkGray),
                )]));
            }

            lines.push(Line::from(""));
            y_off = y_off.saturating_add(row_h);
        }
    }

    let list = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title(" Sessions "));
    f.render_widget(list, chunks[0]);

    let help = Paragraph::new(
        " [Up/Down] navigate | [Enter] open/new | [n] new | [r] rename | [Esc/Ctrl+P] cancel",
    )
    .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(help, chunks[1]);
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
}
