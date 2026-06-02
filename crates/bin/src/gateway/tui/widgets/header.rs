//! Page header — one-line bar with title (+ optional subtitle) on the left
//! and an optional right hint (e.g. `[Esc back]`).
//!
//! Caller passes a 1-row `Rect`. No top/bottom border is drawn; the page
//! is free to draw its own separator below if it wants one.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

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

    let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let right_text = right_hint.map(|s| format!("{s} ")).unwrap_or_default();
    let right_w = right_text.chars().count();
    let total_w = area.width as usize;
    let pad = total_w.saturating_sub(left_w + right_w);

    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    if !right_text.is_empty() {
        spans.push(Span::styled(right_text, theme::dim()));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
