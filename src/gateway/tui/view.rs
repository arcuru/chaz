//! Ratatui rendering for the two TUI modes (chat + session picker).
//! Pure view functions — no mutation, no async.

use crate::session::EntryType;

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::App;
use super::TuiMode;

pub(super) fn ui(f: &mut ratatui::Frame, app: &App) {
    match app.mode {
        TuiMode::Chat => ui_chat(f, app),
        TuiMode::SessionPicker => ui_picker(f, app),
    }
}

fn ui_chat(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .split(f.area());

    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.entries {
        let debug_prefix = if app.debug_mode {
            let ts = entry.timestamp.format("%H:%M:%S");
            let typ = format!("{:?}", entry.entry_type);
            format!("[{ts} {typ:<10}] ")
        } else {
            String::new()
        };
        let dim = Style::default().fg(Color::DarkGray);

        match entry.entry_type {
            EntryType::Message | EntryType::Directive => {
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

                let type_label = if entry.entry_type == EntryType::Directive {
                    " (directive)"
                } else {
                    ""
                };
                let label = format!("{}{}{}:", debug_prefix, entry.sender, type_label);

                lines.push(Line::from(vec![Span::styled(label, sender_style)]));

                for content_line in entry.content.lines() {
                    lines.push(Line::from(format!("  {content_line}")));
                }
                lines.push(Line::from(""));
            }
            EntryType::Ack => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}{} thinking...", entry.sender),
                    dim,
                )]));
            }
            EntryType::ToolCall => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}  > {}", entry.content),
                    dim,
                )]));
            }
            EntryType::ToolResult => {
                let max_len = if app.debug_mode { 500 } else { 120 };
                let display = if entry.content.len() > max_len {
                    format!("{}...", &entry.content[..max_len])
                } else {
                    entry.content.clone()
                };
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}  < {display}"),
                    dim,
                )]));
            }
            EntryType::Error => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}  ERROR {}: {}", entry.sender, entry.content),
                    Style::default().fg(Color::Red),
                )]));
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

    if let Some(ref exchange) = app.pending_approval {
        let info = &exchange.info;
        lines.push(Line::from(vec![Span::styled(
            "--- Tool Approval Required ---",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(format!("  Tool: {}", info.name)));
        lines.push(Line::from(format!("  Risk: {}", info.risk_level)));
        lines.push(Line::from(format!("  Args: {}", info.arguments_display)));
        lines.push(Line::from(vec![
            Span::styled("  [y]", Style::default().fg(Color::Green)),
            Span::raw("es  "),
            Span::styled("[n]", Style::default().fg(Color::Red)),
            Span::raw("o  "),
            Span::styled("[a]", Style::default().fg(Color::Yellow)),
            Span::raw("ll"),
        ]));
        lines.push(Line::from(""));
    }

    if app.waiting {
        lines.push(Line::from(vec![Span::styled(
            "  thinking...",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    let messages_height = chunks[0].height.saturating_sub(2);
    let content_height = lines.len() as u16;
    let scroll = if content_height > messages_height {
        content_height
            .saturating_sub(messages_height)
            .saturating_sub(app.scroll_offset)
    } else {
        0
    };

    let messages = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .block(Block::bordered().title(" Chaz "));
    f.render_widget(messages, chunks[0]);

    let msg_count = app
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Message)
        .count();
    let debug_indicator = if app.debug_mode { " | DEBUG" } else { "" };
    let session_label = match &app.session_name {
        Some(name) => format!("{} ({})", name, app.session_db_id),
        None => app.session_db_id.clone(),
    };
    let status_text = format!(
        " {} | agent: {} | messages: {}{} | /help",
        session_label, app.current_agent, msg_count, debug_indicator
    );
    let status =
        Paragraph::new(status_text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(status, chunks[1]);

    let input = Paragraph::new(app.input.as_str()).block(Block::bordered().title(" > "));
    f.render_widget(input, chunks[2]);

    let cursor_x = chunks[2].x + app.cursor as u16 + 1;
    let cursor_y = chunks[2].y + 1;
    f.set_cursor_position((cursor_x, cursor_y));
}

fn ui_picker(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    if app.session_list.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  No sessions found. Press 'n' to create one.",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        for (i, info) in app.session_list.iter().enumerate() {
            let is_selected = i == app.picker_index;
            let is_current = info.session_db_id == app.session_db_id;

            let marker = if is_selected { "> " } else { "  " };
            let current_marker = if is_current { " *" } else { "" };

            let agent_str = info.agent_name.as_deref().unwrap_or("default");
            let name_str = info
                .name
                .as_ref()
                .map(|n| format!(" \"{n}\""))
                .unwrap_or_default();

            let header = format!(
                "{}{}{}{} ({}, {} entries)",
                marker, info.session_db_id, name_str, current_marker, agent_str, info.entry_count
            );

            let style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if is_current {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            };

            lines.push(Line::from(vec![Span::styled(header, style)]));

            if let Some(ref preview) = info.last_message {
                lines.push(Line::from(vec![Span::styled(
                    format!("    {preview}"),
                    Style::default().fg(Color::DarkGray),
                )]));
            }

            lines.push(Line::from(""));
        }
    }

    let list = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title(" Sessions "));
    f.render_widget(list, chunks[0]);

    let help =
        Paragraph::new(" [Up/Down] navigate | [Enter] select | [n] new session | [Esc] cancel")
            .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(help, chunks[1]);
}
