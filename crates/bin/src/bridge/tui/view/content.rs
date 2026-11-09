//! Display-only projections of message Markdown and tool output.
//! Stored entries and runtime/wire serialization remain untouched.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use tui_markdown::{Options, StyleSheet};
use unicode_normalization::UnicodeNormalization;

use super::theme;

/// Reuse the peer-text quarantine, but keep transcript newlines and indentation.
/// Sanitizing after decoding also neutralizes JSON-escaped terminal controls.
pub(super) fn safe_line(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    chaz_core::security::approval_display::quarantine_capped(
        &text.replace('\t', "    "),
        usize::MAX,
    )
    .text
}

/// Preserve source spelling for normalization/tab-only display changes. Other
/// quarantine changes must also reach the clipboard, not reveal hidden controls.
pub(super) fn copy_line(text: &str, safe: &str) -> String {
    if text.replace('\t', "    ").nfc().collect::<String>() == safe {
        text.to_owned()
    } else {
        safe.to_owned()
    }
}

/// A complete JSON value is a serialization boundary. Decode string values
/// once; never interpret escapes in plain text or try to repair clipped JSON.
pub(super) fn tool_output(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.to_owned();
    };
    let mut output = String::new();
    write_value(&value, 0, &mut output);
    output
}

/// An indented outline, not a JSON reserialization: string values are text.
fn write_value(value: &Value, depth: usize, output: &mut String) {
    let indent = "  ".repeat(depth);
    match value {
        Value::String(text) => {
            if text.is_empty() {
                output.push_str("\"\"");
            } else {
                for (i, line) in text.split('\n').enumerate() {
                    if i > 0 {
                        output.push('\n');
                        output.push_str(&indent);
                    }
                    output.push_str(line);
                }
            }
        }
        Value::Object(fields) if !fields.is_empty() => {
            output.push_str("{\n");
            for (key, value) in fields {
                output.push_str(&format!("{indent}  {}: ", Value::String(key.clone())));
                write_value(value, depth + 1, output);
                output.push('\n');
            }
            output.push_str(&format!("{indent}}}"));
        }
        Value::Array(items) if !items.is_empty() => {
            output.push_str("[\n");
            for value in items {
                output.push_str(&format!("{indent}  "));
                write_value(value, depth + 1, output);
                output.push('\n');
            }
            output.push_str(&format!("{indent}]"));
        }
        _ => output.push_str(&value.to_string()),
    }
}

/// Only Chaz presentation choices live here; Markdown interpretation and table
/// layout belong to tui-markdown. In particular, highlighting is not enabled.
#[derive(Clone)]
struct ChazStyles;

impl StyleSheet for ChazStyles {
    fn heading(&self, _level: u8) -> Style {
        theme::accent_bold()
    }
    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }
    fn code(&self) -> Style {
        theme::accent().bg(theme::BAR_BG)
    }
    fn code_block_fence(&self) -> &str {
        ""
    }
    fn link(&self) -> Style {
        theme::accent().add_modifier(Modifier::UNDERLINED)
    }
    fn blockquote(&self) -> Style {
        Style::default()
    }
    fn list_marker(&self) -> Style {
        theme::dim()
    }
    fn table_header(&self) -> Style {
        theme::accent_bold()
    }
    fn table_border(&self) -> Style {
        theme::dim()
    }
    fn html(&self) -> Style {
        Style::default()
    }
}

/// Library output is still untrusted display text: quarantine decoded entities,
/// link targets and code, then retain safe original spelling for clipboard use.
/// The caller's width-keyed transcript cache owns resize invalidation.
pub(super) fn markdown(content: &str, width: u16) -> Vec<(Line<'static>, String)> {
    let options = Options::new(ChazStyles).table_width(width.saturating_sub(2));
    let text = tui_markdown::from_str_with_options(content, &options);
    text.lines
        .into_iter()
        .map(|line| {
            let mut copy = String::new();
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            if !line.spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            for span in line.spans {
                let safe = safe_line(&span.content);
                copy.push_str(&copy_line(&span.content, &safe));
                spans.push(Span::styled(safe, span.style));
            }
            (Line::from(spans).style(line.style), copy)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        text::Text,
        widgets::{Paragraph, Widget, Wrap},
    };
    use unicode_width::UnicodeWidthStr;

    fn screen(input: &str, width: u16) -> String {
        let text = Text::from(
            markdown(input, width)
                .into_iter()
                .map(|(line, _)| line)
                .collect::<Vec<_>>(),
        );
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let height = paragraph.line_count(width).max(1) as u16;
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        paragraph.render(area, &mut buffer);
        (0..height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < width {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x += symbol.width().max(1) as u16;
                }
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn library_output_is_quarantined_for_display_and_copy() {
        for cell in [
            "&#27;[2J",
            "&#10;next",
            "&#9;tab",
            "[link](a&#27;b)",
            "[link](a&#x202e;b)",
            "`a\tb`",
            "a\u{1b}[31m",
        ] {
            let input = format!("| Head | Other |\n|---|---|\n| {cell} | END |");
            for width in [12, 24, 80] {
                let display = screen(&input, width);
                assert!(!display.chars().any(|c| c.is_control() && c != '\n'));
                assert!(!display.contains('\u{202e}'));
                let copy = markdown(&input, width)
                    .into_iter()
                    .map(|(_, copy)| copy)
                    .collect::<Vec<_>>()
                    .join("\n");
                if width == 80 {
                    assert!(copy.contains("END"), "{cell:?}: {copy:?}");
                }
                assert!(
                    !copy
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')
                );
                assert!(!copy.contains('\u{202e}'));
            }
        }
        // Outside tables the final projection still quarantines decoded text,
        // code and link destinations, not just the raw Markdown source.
        let display = screen("safe &#27;[2J [link](a&#x202e;b) `a\u{1b}b`", 80);
        assert!(
            display.contains("�[2J") && display.contains("a�b"),
            "{display}"
        );
    }
}
