//! Concrete bridge implementations for the chaz binary.
//!
//! The [`chaz_core::bridge::Bridge`] trait + approval types live in
//! the library; this module only carries the bin-side impls (Matrix,
//! TUI, CLI).

pub mod cli;
pub mod matrix;
pub mod tui;
