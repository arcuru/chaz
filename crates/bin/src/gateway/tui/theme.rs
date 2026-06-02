//! Visual theme — named palette + style constants for the TUI.
//!
//! All rendering code imports from here rather than constructing styles
//! inline. Adding a new color or style means adding it here first; if the
//! same combination shows up twice in render code, promote it.

// Several helpers are staged for upcoming settings-page work and have no
// caller yet; suppress the noise until those pages land.
#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};

// ── Palette ───────────────────────────────────────────────────────────

pub(super) const USER: Color = Color::Rgb(0x5c, 0xf0, 0xff); // bright cyan
pub(super) const ASSISTANT: Color = Color::Rgb(0xff, 0x7a, 0xd6); // magenta
pub(super) const SYSTEM: Color = Color::Rgb(0xe6, 0xd9, 0x7a); // muted yellow
pub(super) const TOOL: Color = Color::Rgb(0x4d, 0xd0, 0xff); // tool cyan
pub(super) const ERROR: Color = Color::Rgb(0xff, 0x5a, 0x6e); // red
pub(super) const ACCENT: Color = Color::Rgb(0x6a, 0xff, 0xa3); // electric green
pub(super) const DIM: Color = Color::Rgb(0x70, 0x74, 0x82); // gray

/// Dark background used for status strips, scope strips, help bars.
pub(super) const BAR_BG: Color = Color::Rgb(0x1a, 0x1d, 0x26);

// ── Style helpers ─────────────────────────────────────────────────────
//
// These return fresh `Style` values; the cost is a struct copy, not an
// allocation. Returning `Style` (vs storing as `const`) keeps the API
// composable with `.add_modifier(...)` at call sites that need an extra
// tweak.

/// Dim foreground — labels, helper text, low-priority info.
pub(super) fn dim() -> Style {
    Style::default().fg(DIM)
}

/// Accent foreground (electric green) — emphasis, current values, links.
pub(super) fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// Accent foreground + bold — titles, strong emphasis.
pub(super) fn accent_bold() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Error foreground.
pub(super) fn error() -> Style {
    Style::default().fg(ERROR)
}

/// Selected / active row — black text on accent background, bold.
pub(super) fn selected() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

/// Bar background only (no foreground set). Use on padding spans.
pub(super) fn bar() -> Style {
    Style::default().bg(BAR_BG)
}

/// Dim text on bar background — inactive tab labels, idle hints.
pub(super) fn dim_on_bar() -> Style {
    Style::default().fg(DIM).bg(BAR_BG)
}

/// White-on-bar — the default text color for status / help strips.
pub(super) fn text_on_bar() -> Style {
    Style::default().fg(Color::White).bg(BAR_BG)
}
