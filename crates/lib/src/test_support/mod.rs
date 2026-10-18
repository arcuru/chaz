//! Test scaffolding for integration-flavored tests that drive the chaz
//! runtime end-to-end with a scripted LLM backend.
//!
//! This module is compiled only for tests. It lives in the crate rather than
//! `tests/` so tests can reach `pub(crate)` items.

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
