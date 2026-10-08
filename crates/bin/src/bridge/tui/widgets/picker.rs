//! Filter-as-you-type picker — bottom-anchored multi-line list editor.
//!
//! Renders three regions stacked top-to-bottom inside `area`:
//!
//! 1. **Filter input** (1 line) — same look as `inline_edit_prompt`:
//!    ` label: filter▎`.
//! 2. **Match rows** (variable, fills the middle) — one per pre-filtered
//!    candidate, with `> ` marking the highlighted row. `(no matches)`
//!    in dim text if the slice is empty.
//! 3. **Footer hint** (1 line) — `  ↑↓ select · enter add · esc cancel`.
//!
//! Like `inline_edit_prompt`, this is a pure render function. State
//! (filter buffer, cursor, selection index) lives on `App`; the caller
//! pre-filters the candidate list and passes the full filtered slice in.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use super::super::theme;

/// Render a filter-as-you-type picker into `area`.
///
/// - `label` — short field name (e.g. `"add agent"`), rendered dim on the
///   filter line.
/// - `filter` — current filter-buffer contents.
/// - `cursor` — byte offset into `filter` where the block cursor sits.
/// - `items` — already-filtered candidate names, in display order.
/// - `selected` — index into `items` of the highlighted row. Clamped to
///   candidate range; ignored when `items` is empty.
///
/// Returns the match-row viewport for input ownership.
pub(in super::super) fn picker(
    f: &mut Frame,
    area: Rect,
    label: &str,
    filter: &str,
    cursor: usize,
    items: &[&str],
    selected: usize,
) -> Rect {
    if area.height == 0 {
        return Rect::default();
    }

    // Paint the whole region with the bar background so any padding rows
    // pick up the same look as the rest of the bottom strip.
    f.render_widget(Block::default().style(theme::bar()), area);

    let has_footer = area.height >= 3;
    let footer_h: u16 = if has_footer { 1 } else { 0 };
    let chunks = Layout::vertical([
        Constraint::Length(1),        // filter input
        Constraint::Min(0),           // match rows (fills middle)
        Constraint::Length(footer_h), // footer hint (0 when no room)
    ])
    .split(area);

    // 1. Filter input
    let cursor = cursor.min(filter.len());
    let (pre, post) = filter.split_at(cursor);
    let filter_line = Line::from(vec![
        Span::styled(format!(" {label}: "), theme::dim_on_bar()),
        Span::styled(pre.to_string(), theme::text_on_bar()),
        Span::styled("▎", theme::accent()),
        Span::styled(post.to_string(), theme::text_on_bar()),
    ]);
    f.render_widget(Paragraph::new(filter_line).style(theme::bar()), chunks[0]);

    // 2. Match rows
    if items.is_empty() {
        f.render_widget(
            Paragraph::new("  (no matches)").style(theme::dim_on_bar()),
            chunks[1],
        );
    } else {
        let selected = selected.min(items.len() - 1);
        let rows = items.iter().enumerate().map(|(i, name)| {
            let marker = if i == selected { "> " } else { "  " };
            let style = if i == selected {
                theme::selected()
            } else {
                theme::text_on_bar()
            };
            ListItem::new(Line::from(Span::styled(format!("  {marker}{name}"), style)))
        });
        let mut state = ListState::default().with_selected(Some(selected));
        f.render_stateful_widget(List::new(rows).style(theme::bar()), chunks[1], &mut state);
    }

    // 3. Footer hint
    if has_footer {
        let hint = Line::from(vec![Span::styled(
            "  ↑↓ select · enter add · esc cancel",
            theme::dim_on_bar(),
        )]);
        f.render_widget(Paragraph::new(hint).style(theme::bar()), chunks[2]);
    }
    chunks[1]
}
