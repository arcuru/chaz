//! Test scaffolding for integration-flavored tests that drive the chaz
//! runtime end-to-end with a scripted LLM backend.
//!
//! This module is `pub(crate)` and only used by `#[cfg(test)]` integration
//! modules. The infrastructure intentionally lives in the crate (rather than
//! `tests/`) so it can reach `pub(crate)` items without forcing a
//! binary→library refactor. Helpers with no current caller are deleted
//! rather than kept; nothing here is dead-code-silenced module-wide.

pub(crate) mod harness;
pub(crate) mod mock_backend;
pub(crate) mod mock_host;

pub(crate) use harness::*;
pub(crate) use mock_backend::*;
pub(crate) use mock_host::*;

#[cfg(test)]
mod projection_integration;
#[cfg(test)]
mod runtime_integration;
