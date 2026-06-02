//! Scope tab strip — one-line bar with a label prefix and a sequence of
//! tabs separated by ` · `. The active tab is highlighted; inactive tabs
//! are dim.
//!
//! Used by the model picker (`scope: Session · ava · chaz`) and the
//! settings pages (`category: Agents · Backends · Defaults · …`).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::super::theme;

/// Render a tab strip into `area` (expects height 1).
///
/// - `prefix` — text that introduces the strip (`" scope: "`, `" view: "`).
///   Rendered dim on the bar background. Pass an empty string to omit.
/// - `labels` — tab labels in display order.
/// - `selected` — index of the active tab (clamped to `labels.len()-1`).
///   If `labels` is empty, this renders just the prefix.
pub(in super::super) fn scope_strip(
    f: &mut Frame,
    area: Rect,
    prefix: &str,
    labels: &[&str],
    selected: usize,
) {
    let mut spans: Vec<Span> = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix.to_string(), theme::dim_on_bar()));
    }
    for (i, label) in labels.iter().enumerate() {
        let style = if i == selected {
            theme::selected()
        } else {
            theme::dim_on_bar()
        };
        spans.push(Span::styled(format!(" {label} "), style));
        if i + 1 < labels.len() {
            spans.push(Span::styled(" · ", theme::dim_on_bar()));
        }
    }
    let para = Paragraph::new(Line::from(spans)).style(theme::bar());
    f.render_widget(para, area);
}
