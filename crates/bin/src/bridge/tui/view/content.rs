//! Display-only projections of message Markdown and tool output.
//! Stored entries and runtime/wire serialization remain untouched.

use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

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

#[derive(Default)]
struct MarkdownLines {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    indent: String,
}

impl MarkdownLines {
    fn text(&mut self, text: &str, style: Style) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            if !part.is_empty() {
                if self.spans.is_empty() {
                    self.spans.push(Span::raw(self.indent.clone()));
                }
                self.spans.push(Span::styled(safe_line(part), style));
            }
        }
    }

    fn newline(&mut self) {
        self.lines.push(Line::from(std::mem::take(&mut self.spans)));
    }

    fn finish_line(&mut self) {
        if !self.spans.is_empty() {
            self.newline();
        }
    }

    fn block_break(&mut self) {
        self.finish_line();
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }
}

/// CommonMark events become existing theme styles and Ratatui spans. Wrapping,
/// viewport caching and scrolling remain the transcript renderer's job.
pub(super) fn markdown(content: &str) -> Vec<Line<'static>> {
    let mut out = MarkdownLines::default();
    let mut styles = vec![Style::default()];
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut links = Vec::new();
    let mut indents = Vec::new();
    // Let CommonMark handle tabs and line endings; quarantine emitted text,
    // including decoded entities and link targets, before it reaches the screen.
    for event in Parser::new(content) {
        let style = *styles.last().unwrap();
        match event {
            Event::Start(tag) => {
                let next_style = match tag {
                    Tag::Paragraph => {
                        if lists.is_empty() {
                            out.block_break();
                        }
                        style
                    }
                    Tag::Heading { .. } => {
                        out.block_break();
                        style.patch(theme::accent_bold())
                    }
                    Tag::CodeBlock(_) => {
                        out.finish_line();
                        style.patch(theme::accent().bg(theme::BAR_BG))
                    }
                    Tag::List(start) => {
                        if lists.is_empty() {
                            out.block_break();
                        } else {
                            out.finish_line();
                        }
                        lists.push(start);
                        style
                    }
                    Tag::Item => {
                        out.finish_line();
                        indents.push(out.indent.clone());
                        let marker = match lists.last_mut() {
                            Some(Some(n)) => {
                                let marker = format!("{n}. ");
                                *n = n.saturating_add(1);
                                marker
                            }
                            _ => "• ".into(),
                        };
                        out.text(&marker, theme::dim());
                        out.indent.push_str(&" ".repeat(marker.chars().count()));
                        style
                    }
                    Tag::BlockQuote(_) => {
                        out.block_break();
                        indents.push(out.indent.clone());
                        out.indent.push_str("│ ");
                        style
                    }
                    Tag::Emphasis => style.add_modifier(Modifier::ITALIC),
                    Tag::Strong => style.add_modifier(Modifier::BOLD),
                    Tag::Link {
                        link_type,
                        dest_url,
                        ..
                    }
                    | Tag::Image {
                        link_type,
                        dest_url,
                        ..
                    } => {
                        links.push(
                            if matches!(link_type, LinkType::Autolink | LinkType::Email) {
                                String::new()
                            } else {
                                dest_url.to_string()
                            },
                        );
                        style
                            .patch(theme::accent())
                            .add_modifier(Modifier::UNDERLINED)
                    }
                    _ => style,
                };
                styles.push(next_style);
            }
            Event::End(tag) => {
                match tag {
                    TagEnd::Paragraph
                    | TagEnd::Heading(_)
                    | TagEnd::CodeBlock
                    | TagEnd::HtmlBlock => out.finish_line(),
                    TagEnd::Item | TagEnd::BlockQuote(_) => {
                        out.finish_line();
                        out.indent = indents.pop().unwrap();
                    }
                    TagEnd::List(_) => {
                        lists.pop();
                        if lists.is_empty() {
                            out.block_break();
                        }
                    }
                    TagEnd::Link | TagEnd::Image => {
                        if let Some(url) = links.pop().filter(|url| !url.is_empty()) {
                            out.text(&format!(" ({url})"), style);
                        }
                    }
                    _ => {}
                }
                styles.pop();
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                out.text(&text, style)
            }
            Event::Code(text) => out.text(&text, style.patch(theme::accent().bg(theme::BAR_BG))),
            Event::SoftBreak | Event::HardBreak => out.newline(),
            Event::Rule => {
                out.block_break();
                out.text("───", theme::dim());
                out.block_break();
            }
            _ => {}
        }
    }
    out.finish_line();
    while out.lines.last().is_some_and(|line| line.spans.is_empty()) {
        out.lines.pop();
    }
    for line in &mut out.lines {
        if !line.spans.is_empty() {
            line.spans.insert(0, Span::raw("  "));
        }
    }
    out.lines
}
