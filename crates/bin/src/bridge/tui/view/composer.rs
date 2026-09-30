//! Visual layout of the chat composer. Cursor offsets in `App` remain UTF-8
//! byte boundaries; the terminal coordinates are measured in display cells.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) struct Composer {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: u16,
}

pub(super) fn layout(input: &str, cursor: usize, width: u16) -> Composer {
    let width = width.max(1) as usize;
    let mut lines = vec![String::new()];
    let (mut row, mut col) = (0, 0);
    let mut position = None;
    for (offset, grapheme) in input.grapheme_indices(true) {
        if grapheme == "\n" {
            if offset == cursor {
                position = Some((row, col));
            }
            lines.push(String::new());
            row += 1;
            col = 0;
            continue;
        }
        let cells = UnicodeWidthStr::width(grapheme);
        if col > 0 && col + cells > width {
            lines.push(String::new());
            row += 1;
            col = 0;
        }
        if offset == cursor {
            position = Some((row, col));
        }
        lines[row].push_str(grapheme);
        col += cells;
    }
    if cursor == input.len() {
        if col >= width {
            lines.push(String::new());
            row += 1;
            col = 0;
        }
        position = Some((row, col));
    }
    let (cursor_row, cursor_col) = position.unwrap_or((row, col));
    Composer {
        lines,
        cursor_row,
        cursor_col: cursor_col.min(width - 1) as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_line_and_resize() {
        let input = "abcdefghij";
        let at_ten = layout(input, 7, 4);
        assert_eq!(at_ten.lines, ["abcd", "efgh", "ij"]);
        assert_eq!((at_ten.cursor_row, at_ten.cursor_col), (1, 3));
        let resized = layout(input, input.len(), 5);
        assert_eq!(resized.lines, ["abcde", "fghij", ""]);
        assert_eq!((resized.cursor_row, resized.cursor_col), (2, 0));
    }

    #[test]
    fn multiline_and_edit_boundary() {
        let input = "abcd\néf";
        let before_newline = layout(input, 4, 4);
        assert_eq!(before_newline.lines, ["abcd", "éf"]);
        assert_eq!(
            (before_newline.cursor_row, before_newline.cursor_col),
            (0, 3)
        );
        let after_newline = layout(input, 5, 4);
        assert_eq!((after_newline.cursor_row, after_newline.cursor_col), (1, 0));
        assert_eq!(layout("abc\néf", 3, 4).lines, ["abc", "éf"]);
    }

    #[test]
    fn unicode_cells_and_grapheme_cursor() {
        let input = "a界e\u{301}👍🏽z";
        let before_emoji = input.find('👍').unwrap();
        let at_emoji = layout(input, before_emoji, 4);
        assert_eq!(at_emoji.lines, ["a界e\u{301}", "👍🏽z"]);
        assert_eq!((at_emoji.cursor_row, at_emoji.cursor_col), (1, 0));
        let after_emoji = before_emoji + "👍🏽".len();
        let at_z = layout(input, after_emoji, 4);
        assert_eq!((at_z.cursor_row, at_z.cursor_col), (1, 2));
    }
}
