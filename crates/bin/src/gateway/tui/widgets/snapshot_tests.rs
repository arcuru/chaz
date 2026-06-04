//! Snapshot tests for the shared widget primitives. Catches "I changed
//! the scope strip and broke the layout" regressions before review.
//!
//! Tests render to a `TestBackend` and compare the buffer content (not
//! styles) against an `insta` snapshot. Styles aren't snapshotted in v1
//! because RGB color values are noisy and content captures layout
//! regressions; if a style regression slips through, we add it then.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use super::{header, inline_edit_prompt, picker, scope_strip, sidebar, status_strip};

/// Render a single-widget frame and return the buffer as a multi-line
/// string (one row per line, leading/trailing spaces preserved).
fn snapshot<F>(width: u16, height: u16, draw: F) -> String
where
    F: FnOnce(&mut ratatui::Frame, Rect),
{
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test backend init");
    terminal
        .draw(|f| draw(f, f.area()))
        .expect("test render failed");
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn scope_strip_three_tabs_middle_selected() {
    let s = snapshot(60, 1, |f, area| {
        scope_strip(f, area, " scope: ", &["Session", "ava", "chaz"], 1);
    });
    insta::assert_snapshot!(s);
}

#[test]
fn scope_strip_no_prefix() {
    let s = snapshot(40, 1, |f, area| {
        scope_strip(f, area, "", &["A", "B"], 0);
    });
    insta::assert_snapshot!(s);
}

#[test]
fn status_strip_simple_hint() {
    let s = snapshot(60, 1, |f, area| {
        status_strip(f, area, " ↑↓ select · Enter open · Esc back ");
    });
    insta::assert_snapshot!(s);
}

#[test]
fn header_title_subtitle_and_hint() {
    let s = snapshot(60, 1, |f, area| {
        header(
            f,
            area,
            "Peer Settings",
            Some("carbon.peer"),
            Some("[Esc back]"),
        );
    });
    insta::assert_snapshot!(s);
}

#[test]
fn header_title_only() {
    let s = snapshot(40, 1, |f, area| {
        header(f, area, "Settings", None, None);
    });
    insta::assert_snapshot!(s);
}

#[test]
fn sidebar_five_items_third_selected_unfocused() {
    let s = snapshot(16, 6, |f, area| {
        sidebar(
            f,
            area,
            &["Agents", "Backends", "Defaults", "Bridges", "About"],
            2,
            false,
        );
    });
    insta::assert_snapshot!(s);
}

#[test]
fn sidebar_five_items_third_selected_focused() {
    let s = snapshot(16, 6, |f, area| {
        sidebar(
            f,
            area,
            &["Agents", "Backends", "Defaults", "Bridges", "About"],
            2,
            true,
        );
    });
    insta::assert_snapshot!(s);
}

#[test]
fn inline_edit_prompt_cursor_mid_string() {
    let s = snapshot(40, 1, |f, area| {
        inline_edit_prompt(f, area, "name", "ava", 2);
    });
    insta::assert_snapshot!(s);
}

#[test]
fn inline_edit_prompt_cursor_at_end() {
    let s = snapshot(40, 1, |f, area| {
        inline_edit_prompt(f, area, "model", "deepseek/v4", 11);
    });
    insta::assert_snapshot!(s);
}

#[test]
fn picker_populated_list_second_selected() {
    let s = snapshot(40, 6, |f, area| {
        picker(
            f,
            area,
            "add agent",
            "",
            0,
            &["chaz", "ava", "researcher"],
            1,
        );
    });
    insta::assert_snapshot!(s);
}

#[test]
fn picker_empty_list_no_matches() {
    let s = snapshot(40, 4, |f, area| {
        picker(f, area, "add agent", "xyz", 3, &[], 0);
    });
    insta::assert_snapshot!(s);
}

#[test]
fn picker_filter_typed_one_match() {
    let s = snapshot(40, 5, |f, area| {
        picker(f, area, "add agent", "av", 2, &["ava"], 0);
    });
    insta::assert_snapshot!(s);
}
