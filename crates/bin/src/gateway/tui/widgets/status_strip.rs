//! Status / help strip — one-line bar at the bottom of a page with
//! context-aware key hints.
//!
//! Caller passes a 1-row `Rect` and a pre-formatted hint string. Page is
//! responsible for the hint content (e.g. `" ↑↓ select · Enter open · Esc back "`).

use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::super::theme;

/// Render a status / help strip into `area` (expects height 1).
pub(in super::super) fn status_strip(f: &mut Frame, area: Rect, hints: &str) {
    let para = Paragraph::new(hints).style(theme::text_on_bar());
    f.render_widget(para, area);
}
