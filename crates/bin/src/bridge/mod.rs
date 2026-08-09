//! Concrete bridge implementations for the chaz binary.
//!
//! The [`chaz_core::bridge::Bridge`] trait + approval types live in
//! the library; this module only carries the bin-side impls (TUI, CLI,
//! one-shot command). The Matrix bridge lives in its own
//! `chaz-matrix-bridge` crate.

pub mod cli;
pub mod cmd;
pub mod tui;
