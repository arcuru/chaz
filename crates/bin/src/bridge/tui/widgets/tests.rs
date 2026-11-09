//! Behavioral checks for picker visibility and essential navigation hints.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use super::{header, picker};

fn render<F>(width: u16, height: u16, draw: F) -> String
where
    F: FnOnce(&mut ratatui::Frame, Rect),
{
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| draw(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn picker_last_candidate_is_visible() {
    let names: Vec<_> = (0..20).map(|i| format!("agent-{i:02}")).collect();
    let items: Vec<_> = names.iter().map(String::as_str).collect();
    for (width, height) in [(80, 8), (24, 4), (24, 2)] {
        let screen = render(width, height, |f, area| {
            picker(f, area, "add agent", "", 0, &items, 19);
        });
        assert!(screen.contains("> agent-19"), "{width}x{height}: {screen}");
    }
}

#[test]
fn header_long_wide_subtitle_keeps_escape_hint() {
    for width in [80, 40] {
        let screen = render(width, 1, |f, area| {
            header(
                f,
                area,
                "Session Settings",
                Some("研究室研究室研究室研究室研究室研究室研究室研究室"),
                Some("[Esc back]"),
            );
        });
        assert!(screen.contains("[Esc back]"), "{screen}");
    }
}

#[test]
fn picker_scrolls_both_directions_and_clamps_wide_candidates() {
    let names: Vec<_> = (0..20).map(|i| format!("研究-{i:02}")).collect();
    let items: Vec<_> = names.iter().map(String::as_str).collect();
    for selected in (0..20).chain((0..20).rev()).chain([99]) {
        for height in [2, 3, 4, 8, 30] {
            let screen = render(24, height, |f, area| {
                picker(f, area, "add agent", "", 0, &items, selected);
            });
            let expected = format!("> 研 究 -{:02}", selected.min(19));
            assert!(
                screen.contains(&expected),
                "selected={selected}, height={height}: {screen}"
            );
            assert_eq!(screen.lines().filter(|r| r.contains("> ")).count(), 1);
        }
    }
}
