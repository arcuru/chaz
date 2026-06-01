//! chaz-core — the library half of chaz.
//!
//! Holds the runtime, tool system, extensions, session model, security
//! surface, MCP bridge, backends, and command dispatcher. The binary
//! crate (`chaz`) brings the gateway implementations (Matrix, TUI,
//! CLI) and the entrypoint.

pub mod agent;
pub mod agent_db;
pub mod backends;
pub mod bubblewrap_host;
pub mod commands;
pub mod config;
pub mod context;
pub mod db_kind;
pub mod defaults;
pub mod embedding;
pub mod error;
pub mod extension;
pub mod extensions;
pub mod gateway;
pub mod grants;
pub mod hosted_index;
pub mod mcp;
pub mod memory_bank_db;
pub mod model_catalog_cache;
pub mod openai;
pub mod routine;
pub mod runtime;
pub mod security;
pub mod server;
pub mod session;
pub mod skill_bank_db;
pub mod tool;
pub mod tool_host;
pub mod tools;
pub mod types;
pub mod util;
pub mod wasm_host;

#[cfg(test)]
pub mod test_support;
