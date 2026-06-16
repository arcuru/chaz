//! Inline edit prompt — bottom-strip editor for fields that don't fit
//! cleanly inline (lists, multiline, free-form strings).
//!
//! Renders as one line at the bottom of a page: ` label: value▎`.
//! Caller passes cursor byte offset; the cursor is drawn as a block
//! character. Blinking will come with a stateful variant later.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::theme;

/// Render an inline edit prompt into `area` (expects height 1).
///
/// - `label` — short field name, rendered dim with a trailing `: `.
/// - `value` — current edit-buffer contents.
/// - `cursor` — byte offset into `value` where the cursor sits. Clamped
///   to `value.len()`. Splits the value into a pre / post section with a
///   block-cursor character between them.
pub(in super::super) fn inline_edit_prompt(
    f: &mut Frame,
    area: Rect,
    label: &str,
    value: &str,
    cursor: usize,
) {
    let cursor = cursor.min(value.len());
    let (pre, post) = value.split_at(cursor);

    let spans = vec![
        Span::styled(format!(" {label}: "), theme::dim_on_bar()),
        Span::styled(pre.to_string(), theme::text_on_bar()),
        Span::styled("▎", theme::accent()),
        Span::styled(post.to_string(), theme::text_on_bar()),
    ];

    let para = Paragraph::new(Line::from(spans)).style(theme::bar());
    f.render_widget(para, area);
}
