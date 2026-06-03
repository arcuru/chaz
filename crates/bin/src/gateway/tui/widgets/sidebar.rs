//! Sidebar + detail layout for settings pages.
//!
//! `sidebar_detail_layout` splits an area into a fixed-width left rail
//! and a remaining right pane. The caller decides what to render in each.
//!
//! `sidebar` renders a list of category labels into the left rail with
//! the active one highlighted. Pages that want a fancier sidebar (icons,
//! groups, dividers) are free to render their own from the rect.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::super::theme;

/// Split `area` into `(sidebar, detail)` rects.
///
/// `sidebar_w` is the sidebar width in columns. The caller is responsible
/// for choosing it (typically max-label-width + a couple of cells of
/// padding). The detail rect gets everything to the right.
pub(in super::super) fn sidebar_detail_layout(area: Rect, sidebar_w: u16) -> (Rect, Rect) {
    let chunks = Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(0)]).split(area);
    (chunks[0], chunks[1])
}

/// Render a sidebar of category labels.
///
/// Selection marker `>` is rendered on the active row; inactive rows are
/// dim. When `focused` is true the active row uses the inverted "selected"
/// style (black-on-accent) so it's obvious which pane owns arrow keys;
/// when false the active row stays accent-bold (still visible, no bg) so
/// the user can tell which category is current without it stealing focus.
/// Each row occupies one terminal line; if the sidebar is shorter than
/// `items.len()`, the tail is clipped (no scroll in v1 — categories are
/// flat and few).
pub(in super::super) fn sidebar(
    f: &mut Frame,
    area: Rect,
    items: &[&str],
    selected: usize,
    focused: bool,
) {
    let mut lines: Vec<Line> = Vec::with_capacity(items.len());
    for (i, label) in items.iter().enumerate() {
        let is_active = i == selected;
        let marker = if is_active { "> " } else { "  " };
        let style = if is_active && focused {
            theme::selected()
        } else if is_active {
            theme::accent_bold()
        } else {
            theme::dim()
        };
        lines.push(Line::from(Span::styled(format!("{marker}{label}"), style)));
    }
    f.render_widget(Paragraph::new(lines), area);
}
