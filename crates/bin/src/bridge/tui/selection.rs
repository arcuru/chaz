//! Conversation selection uses source byte ranges, not decorated screen text.
//! Ratatui supplies the wrapping; soft wraps must not become clipboard newlines.
use std::collections::BTreeMap;
use std::io::{self, Write};

use base64::{Engine, engine::general_purpose::STANDARD};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Color,
    widgets::{Paragraph, Widget, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    line: usize,
    byte: usize,
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    start: Point,
    end: Point,
}

#[derive(Default, Debug)]
pub(super) struct CopyText {
    lines: BTreeMap<usize, String>,
    rows: BTreeMap<usize, Vec<Cell>>,
}

impl CopyText {
    pub(super) fn add(&mut self, line: usize, visual_row: usize, text: &str, width: u16) {
        self.lines.insert(line, text.to_owned());
        if width == 0 {
            return;
        }
        let decorated = format!("  {text}");
        let paragraph = Paragraph::new(decorated.as_str()).wrap(Wrap { trim: false });
        let height = paragraph.line_count(width).min(u16::MAX as usize) as u16;
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        paragraph.render(area, &mut buffer);
        let mut consumed = 0;
        for y in 0..height {
            // Wide glyph continuation cells are blank in Buffer, not source
            // spaces. Read only the glyph's leading cell.
            let mut rendered = String::new();
            let mut x = 0;
            while x < width {
                let symbol = buffer[(x, y)].symbol();
                rendered.push_str(symbol);
                x += UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            let rendered = rendered.trim_end_matches(' ');
            let mut cells = Vec::new();
            let mut byte = consumed;
            for glyph in rendered.graphemes(true) {
                // Wrapping omits break whitespace and zero-width controls
                // (such as tabs). Walk forward through the original source.
                let Some(skipped) = decorated[byte..].find(glyph) else {
                    continue;
                };
                byte += skipped;
                let next = byte + glyph.len();
                let cell = Cell {
                    start: Point {
                        line,
                        byte: byte.saturating_sub(2).min(text.len()),
                    },
                    end: Point {
                        line,
                        byte: next.saturating_sub(2).min(text.len()),
                    },
                };
                cells.extend(std::iter::repeat_n(cell, glyph.width().max(1)));
                byte = next;
            }
            consumed = byte;
            // Padding selects through the end of this source line, including
            // meaningful trailing spaces, but never copies viewport padding.
            let end = Point {
                line,
                byte: byte.saturating_sub(2).min(text.len()),
            };
            let end = if y + 1 == height {
                Point {
                    line,
                    byte: text.len(),
                }
            } else {
                end
            };
            cells.resize(width as usize, Cell { start: end, end });
            self.rows.insert(visual_row + y as usize, cells);
        }
    }

    fn text(&self, start: Point, end: Point) -> String {
        self.lines
            .range(start.line..=end.line)
            .map(|(&line, text)| {
                let from = if line == start.line { start.byte } else { 0 };
                let to = if line == end.line {
                    end.byte
                } else {
                    text.len()
                };
                &text[from..to]
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Default)]
pub(super) struct Selection {
    identity: Option<(String, u64, u16)>,
    area: Rect,
    scroll: usize,
    anchor: Option<Cell>,
    focus: Option<Cell>,
    dragging: bool,
}

impl Selection {
    pub(super) fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.dragging = false;
    }

    pub(super) fn viewport(&mut self, id: &str, rev: u64, area: Rect, scroll: usize) {
        let identity = (id.to_owned(), rev, area.width);
        if self.identity.as_ref() != Some(&identity) {
            self.clear();
            self.identity = Some(identity);
        }
        self.area = area;
        self.scroll = scroll;
    }

    fn cell(&self, text: &CopyText, x: u16, y: u16, clamp: bool) -> Option<Cell> {
        if self.area.is_empty() || (!clamp && !self.area.contains((x, y).into())) {
            return None;
        }
        let x = x.clamp(self.area.x, self.area.right() - 1) - self.area.x;
        let y = y.clamp(self.area.y, self.area.bottom() - 1) - self.area.y;
        let row = self.scroll + y as usize;
        let cells = if clamp {
            text.rows
                .range(self.scroll..self.scroll + self.area.height as usize)
                .min_by_key(|(index, _)| index.abs_diff(row))
                .map(|(_, cells)| cells)?
        } else {
            text.rows.get(&row)?
        };
        cells.get(x as usize).copied()
    }

    pub(super) fn begin(&mut self, text: &CopyText, x: u16, y: u16) -> bool {
        self.clear();
        let Some(cell) = self.cell(text, x, y, false) else {
            return false;
        };
        self.anchor = Some(cell);
        self.focus = Some(cell);
        self.dragging = true;
        true
    }

    pub(super) fn drag(&mut self, text: &CopyText, x: u16, y: u16) {
        if self.dragging {
            self.focus = self.cell(text, x, y, true).or(self.focus);
        }
    }

    pub(super) fn finish(&mut self, text: &CopyText, x: u16, y: u16) -> Option<String> {
        if !self.dragging {
            return None;
        }
        self.drag(text, x, y);
        self.dragging = false;
        let (a, b) = (self.anchor?, self.focus?);
        // A click alone is not a selection or a clipboard write.
        if a.start == b.start {
            self.clear();
            return None;
        }
        let (start, end) = self.bounds()?;
        let selected = text.text(start, end);
        (!selected.is_empty()).then_some(selected)
    }

    fn bounds(&self) -> Option<(Point, Point)> {
        let (a, b) = (self.anchor?, self.focus?);
        Some(if a.start <= b.start {
            (a.start, b.end)
        } else {
            (b.start, a.end)
        })
    }

    pub(super) fn highlight(&self, text: &CopyText, buffer: &mut Buffer) {
        let Some((start, end)) = self.bounds() else {
            return;
        };
        for (&row, cells) in text
            .rows
            .range(self.scroll..self.scroll + self.area.height as usize)
        {
            for (x, cell) in cells.iter().enumerate() {
                if cell.start < end && cell.end > start {
                    buffer[(
                        self.area.x + x as u16,
                        self.area.y + (row - self.scroll) as u16,
                    )]
                        .set_bg(Color::Blue)
                        .set_fg(Color::White);
                }
            }
        }
    }
}

/// OSC 52 writes the clipboard through the terminal, including over SSH.
/// Success means the request was written; terminals may refuse clipboard writes.
pub(super) fn copy_to(writer: &mut impl Write, text: &str) -> io::Result<()> {
    write!(writer, "\x1b]52;c;{}\x07", STANDARD.encode(text))?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(
        text: &CopyText,
        width: u16,
        scroll: usize,
        from: (u16, u16),
        to: (u16, u16),
    ) -> String {
        let mut selection = Selection::default();
        selection.viewport("session", 1, Rect::new(0, 0, width, 5), scroll);
        assert!(selection.begin(text, from.0, from.1));
        selection.drag(text, to.0, to.1);
        selection.finish(text, to.0, to.1).unwrap()
    }

    #[test]
    fn multiline_code_keeps_indentation_without_decorations() {
        let mut text = CopyText::default();
        text.add(1, 1, "fn main() {", 40);
        text.add(2, 2, "    println!(\"café 界 e\u{301}\");", 40);
        text.add(3, 3, "}", 40);
        let expected = "fn main() {\n    println!(\"café 界 e\u{301}\");\n}";
        assert_eq!(select(&text, 40, 0, (2, 1), (2, 3)), expected);
        assert_eq!(select(&text, 40, 0, (2, 3), (2, 1)), expected);
    }

    #[test]
    fn soft_wraps_are_not_newlines_and_scrollback_uses_source_rows() {
        let mut text = CopyText::default();
        text.add(4, 20, "one two three four five", 10);
        text.add(5, 23, "next", 10);
        assert_eq!(
            select(&text, 10, 20, (2, 0), (9, 2)),
            "one two three four five"
        );
        assert_eq!(
            select(&text, 10, 20, (2, 0), (5, 3)),
            "one two three four five\nnext"
        );
    }

    #[test]
    fn tabs_blank_lines_and_trailing_spaces_come_from_source() {
        let mut text = CopyText::default();
        text.add(1, 0, "a\tb  ", 20);
        text.add(2, 1, "", 20);
        text.add(3, 2, "end", 20);
        assert_eq!(select(&text, 20, 0, (2, 0), (19, 2)), "a\tb  \n\nend");
    }

    #[test]
    fn wide_and_combining_glyphs_are_selected_as_whole_graphemes() {
        let mut text = CopyText::default();
        text.add(1, 0, "界e\u{301} café", 20);
        assert_eq!(select(&text, 20, 0, (3, 0), (4, 0)), "界e\u{301}");
    }

    #[test]
    fn wheel_scroll_preserves_selection_but_rewrap_or_refresh_clears_it() {
        let mut text = CopyText::default();
        text.add(1, 10, "first", 20);
        text.add(2, 13, "last", 20);
        let mut selection = Selection::default();
        selection.viewport("session", 1, Rect::new(0, 0, 20, 3), 10);
        assert!(selection.begin(&text, 2, 0));
        selection.viewport("session", 1, Rect::new(0, 0, 20, 3), 11);
        assert_eq!(selection.finish(&text, 19, 2).unwrap(), "first\nlast");
        for (id, rev, width) in [("other", 1, 20), ("session", 2, 20), ("session", 1, 30)] {
            selection.viewport("session", 1, Rect::new(0, 0, 20, 3), 10);
            selection.begin(&text, 2, 0);
            selection.viewport(id, rev, Rect::new(0, 0, width, 3), 10);
            assert!(selection.finish(&text, 6, 0).is_none());
        }
    }

    #[test]
    fn click_or_decoration_does_not_write_clipboard() {
        let mut text = CopyText::default();
        text.add(1, 1, "hello", 20);
        let mut selection = Selection::default();
        selection.viewport("session", 1, Rect::new(0, 0, 20, 3), 0);
        assert!(!selection.begin(&text, 2, 0));
        assert!(selection.finish(&text, 5, 1).is_none());
        assert!(selection.begin(&text, 2, 1));
        assert!(selection.finish(&text, 2, 1).is_none());
    }

    #[test]
    fn clipboard_request_encodes_the_selected_utf8() {
        let mut output = Vec::new();
        copy_to(&mut output, "café\n    code").unwrap();
        assert_eq!(output, b"\x1b]52;c;Y2Fmw6kKICAgIGNvZGU=\x07");
    }
}
