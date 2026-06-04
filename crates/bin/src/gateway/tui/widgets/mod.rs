//! Shared visual primitives for chaz's TUI.
//!
//! Compose pages from these instead of inlining `Style::default().fg(...)`
//! calls and ad-hoc layout splits. All widgets here are pure render
//! functions — the page owns its state, the widget takes what it needs by
//! reference. Stateful widgets graduate to `impl StatefulWidget` when a
//! case genuinely needs internal state (cursor blink, owned scroll, etc.).

// `header`, `sidebar*`, and `inline_edit_prompt` are staged for the
// settings pages that land in the next stages; suppress dead-code noise
// until those consumers exist. Snapshot tests already exercise them.
#![allow(dead_code, unused_imports)]

mod header;
mod inline_edit;
mod picker;
mod scope_strip;
mod sidebar;
mod status_strip;

#[cfg(test)]
mod snapshot_tests;

pub(super) use header::header;
pub(super) use inline_edit::inline_edit_prompt;
pub(super) use picker::picker;
pub(super) use scope_strip::scope_strip;
pub(super) use sidebar::{sidebar, sidebar_detail_layout};
pub(super) use status_strip::status_strip;
