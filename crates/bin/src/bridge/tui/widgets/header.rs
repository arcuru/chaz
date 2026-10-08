//! Page header — one-line bar with title (+ optional subtitle) on the left
//! and an optional right hint (e.g. `[Esc back]`).
//!
//! Caller passes a 1-row `Rect`. No top/bottom border is drawn; the page
//! is free to draw its own separator below if it wants one.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::theme;

/// Render a header bar into `area` (expects height 1).
///
/// - `title` — left-aligned, bold accent.
/// - `subtitle` — optional, follows title with a ` — ` separator, dim.
/// - `right_hint` — optional right-aligned dim text (e.g. `[Esc back]`).
pub(in super::super) fn header(
    f: &mut Frame,
    area: Rect,
    title: &str,
    subtitle: Option<&str>,
    right_hint: Option<&str>,
) {
    let mut left: Vec<Span> = Vec::new();
    left.push(Span::styled(format!(" {title}"), theme::accent_bold()));
    if let Some(sub) = subtitle {
        left.push(Span::styled(format!("  —  {sub}"), theme::dim()));
    }

    let right = Line::from(Span::styled(
        right_hint.map(|s| format!("{s} ")).unwrap_or_default(),
        theme::dim(),
    ));
    // Reserve the active-key hint before clipping a long title/subtitle.
    // Ratatui measures and clips terminal cells, including wide graphemes.
    let chunks = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right.width().min(area.width as usize) as u16),
    ])
    .split(area);
    f.render_widget(Paragraph::new(Line::from(left)), chunks[0]);
    f.render_widget(Paragraph::new(right), chunks[1]);
}
