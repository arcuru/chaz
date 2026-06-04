//! Process-level directory of started MCP servers, keyed by the
//! configured server name (`McpServerConfig.name`, *not* the
//! `mcp-<name>` extension name).
//!
//! Populated by [`crate::extensions::mcp::McpExtension`] during
//! instantiation — successful starts register `Running { server }` so
//! callers can reach the live [`McpServer`] handle, failed starts
//! register `Failed { error }` so the operator can see the failure
//! rather than a silently-missing row.
//!
//! Read off the hot path (TUI Peer→MCP settings page snapshots it once
//! per frame); the inner [`RwLock`] is uncontended in practice.

use super::server::McpServer;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Per-process MCP server directory.
#[derive(Default)]
pub struct McpRegistry {
    entries: RwLock<HashMap<String, McpRegistryEntry>>,
}

/// One registered server's name + status.
#[derive(Clone)]
pub struct McpRegistryEntry {
    pub name: String,
    pub status: McpServerStatus,
}

/// Runtime state of a configured MCP server.
#[derive(Clone)]
pub enum McpServerStatus {
    /// Server started successfully; capability + tool metadata live on
    /// `server`. Cloning the `Arc` is cheap and lets snapshot consumers
    /// inspect live state (e.g. `server.capabilities()`,
    /// `server.tool_count()`) without holding the registry lock.
    Running { server: Arc<McpServer> },
    /// Server failed to start. The error string is the message from
    /// [`McpServer::start`].
    Failed { error: String },
}

impl McpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_running(&self, name: String, server: Arc<McpServer>) {
        let mut entries = self.entries.write().expect("McpRegistry lock poisoned");
        entries.insert(
            name.clone(),
            McpRegistryEntry {
                name,
                status: McpServerStatus::Running { server },
            },
        );
    }

    pub fn insert_failed(&self, name: String, error: String) {
        let mut entries = self.entries.write().expect("McpRegistry lock poisoned");
        entries.insert(
            name.clone(),
            McpRegistryEntry {
                name,
                status: McpServerStatus::Failed { error },
            },
        );
    }

    /// All registered entries, sorted by name. Cheap snapshot — callers
    /// don't hold the lock past the call.
    pub fn snapshot(&self) -> Vec<McpRegistryEntry> {
        let entries = self.entries.read().expect("McpRegistry lock poisoned");
        let mut out: Vec<_> = entries.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}
