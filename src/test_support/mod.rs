//! Test scaffolding for integration-flavored tests that drive the chaz
//! runtime end-to-end with a scripted LLM backend.
//!
//! This module is `pub(crate)` and only used by `#[cfg(test)]` integration
//! modules. The infrastructure intentionally lives in the crate (rather than
//! `tests/`) so it can reach `pub(crate)` items without forcing a
//! binary→library refactor. Helpers are intentionally over-provided — items
//! used by only one current test are still part of the harness API for
//! future tests, so dead_code is silenced module-wide.

#![allow(dead_code)]

pub(crate) mod harness;
pub(crate) mod mock_backend;

pub(crate) use harness::*;
pub(crate) use mock_backend::*;

#[cfg(test)]
mod runtime_integration;
