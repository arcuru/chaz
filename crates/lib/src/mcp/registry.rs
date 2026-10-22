//! Peer-local directory and cleanup owner for MCP startup and running servers.
//! A failed or cancelled close never drains away the only retryable handle.

use super::server::McpServer;
use super::transport::CloseBudget;
use crate::tool::{Tool, ToolRegistry};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::timeout_at;

#[derive(Default)]
pub struct McpRegistry {
    state: RwLock<RegistryState>,
    settled: Notify,
}

#[derive(Default)]
struct RegistryState {
    closing: bool,
    entries: HashMap<String, OwnedEntry>,
}

struct OwnedEntry {
    entry: McpRegistryEntry,
    /// Present before initialization, and retained even if startup/close fails.
    server: Option<Arc<McpServer>>,
    /// Awaited by reference; cancellation cannot detach the join handle.
    startup: Arc<Mutex<Option<JoinHandle<()>>>>,
}
#[derive(Clone)]
pub struct McpRegistryEntry {
    pub name: String,
    pub status: McpServerStatus,
}

/// Runtime state of a configured MCP server.
#[derive(Clone)]
pub enum McpServerStatus {
    /// Configured, and its startup task is in flight. Present from the
    /// moment the extension is instantiated until the server settles, so a
    /// configured server is a visible row from the first frame instead of
    /// appearing out of nowhere once it connects.
    ///
    /// Not a failure. Handshake/discovery use the configured startup timeout;
    /// failure cleanup then has its separate bounded child-cleanup allowance.
    Starting,
    /// Server started and its tools are in the tool registry. Capability +
    /// tool metadata live on `server`; cloning the `Arc` is cheap and lets
    /// snapshot consumers inspect live state (e.g. `server.capabilities()`,
    /// `server.tool_count()`) without holding the registry lock.
    ///
    /// `discovery_error` is `Some` when the server itself came up but
    /// `tools/list` failed — a partial success where the resource and
    /// prompt wrappers work and the tools do not. Distinguishing it from a
    /// clean start matters because both otherwise present as "running with
    /// zero tools", which is also what a legitimately tool-less server
    /// looks like.
    Running {
        server: Arc<McpServer>,
        discovery_error: Option<String>,
    },
    /// Server could not be started, or did not settle within its startup
    /// timeout. The error describes spawn, initialization, timeout, or
    /// incomplete cleanup.
    Failed { error: String },
}

impl McpServerStatus {
    /// Whether this status is terminal — the server will not change state
    /// again without operator action.
    pub fn is_settled(&self) -> bool {
        !matches!(self, McpServerStatus::Starting)
    }
}

impl McpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_starting(&self, name: String) {
        let mut state = self.state.write().expect("McpRegistry lock poisoned");
        if !state.closing {
            state
                .entries
                .entry(name.clone())
                .or_insert_with(|| OwnedEntry {
                    entry: McpRegistryEntry {
                        name,
                        status: McpServerStatus::Starting,
                    },
                    server: None,
                    startup: Arc::new(Mutex::new(None)),
                });
        }
    }

    /// Spawn and retain the transport/task in the same synchronous admission
    /// section. Close cannot race between spawn and ownership registration.
    pub(crate) fn launch(
        &self,
        name: String,
        create: impl FnOnce() -> Result<(Arc<McpServer>, JoinHandle<()>), String>,
    ) -> bool {
        let mut state = self.state.write().expect("McpRegistry lock poisoned");
        if state.closing || state.entries.contains_key(&name) {
            return false;
        }
        let (status, server, startup) = match create() {
            Ok((server, handle)) => (McpServerStatus::Starting, Some(server), Some(handle)),
            Err(error) => {
                tracing::warn!(server = %name, %error, "MCP server failed to spawn — skipping its tools");
                (McpServerStatus::Failed { error }, None, None)
            }
        };
        let launched = startup.is_some();
        state.entries.insert(
            name.clone(),
            OwnedEntry {
                entry: McpRegistryEntry { name, status },
                server,
                startup: Arc::new(Mutex::new(startup)),
            },
        );
        self.settled.notify_waiters();
        launched
    }

    /// Publication is synchronous under the close-admission gate: either all
    /// tools and Running appear before close, or none appear after it.
    pub(crate) fn publish(
        &self,
        name: &str,
        server: Arc<McpServer>,
        discovery_error: Option<String>,
        tools: Vec<Arc<dyn Tool>>,
        tool_registry: &ToolRegistry,
        owner: &'static str,
    ) -> bool {
        let mut state = self.state.write().expect("McpRegistry lock poisoned");
        if state.closing {
            return false;
        }
        let Some(entry) = state.entries.get_mut(name) else {
            return false;
        };
        for tool in tools {
            tool_registry.register_arc_owned(tool, Some(owner));
        }
        entry.entry.status = McpServerStatus::Running {
            server,
            discovery_error,
        };
        self.settled.notify_waiters();
        true
    }

    pub fn insert_running(
        &self,
        name: String,
        server: Arc<McpServer>,
        discovery_error: Option<String>,
    ) {
        let mut state = self.state.write().expect("McpRegistry lock poisoned");
        if state.closing {
            server.begin_close();
            return;
        }
        state.entries.insert(
            name.clone(),
            OwnedEntry {
                entry: McpRegistryEntry {
                    name,
                    status: McpServerStatus::Running {
                        server: server.clone(),
                        discovery_error,
                    },
                },
                server: Some(server),
                startup: Arc::new(Mutex::new(None)),
            },
        );
        self.settled.notify_waiters();
    }

    pub fn insert_failed(&self, name: String, error: String) {
        let mut state = self.state.write().expect("McpRegistry lock poisoned");
        if state.closing {
            return;
        }
        match state.entries.get_mut(&name) {
            Some(entry) => entry.entry.status = McpServerStatus::Failed { error },
            None => {
                state.entries.insert(
                    name.clone(),
                    OwnedEntry {
                        entry: McpRegistryEntry {
                            name,
                            status: McpServerStatus::Failed { error },
                        },
                        server: None,
                        startup: Arc::new(Mutex::new(None)),
                    },
                );
            }
        }
        self.settled.notify_waiters();
    }

    pub fn snapshot(&self) -> Vec<McpRegistryEntry> {
        let state = self.state.read().expect("McpRegistry lock poisoned");
        let mut out: Vec<_> = state.entries.values().map(|e| e.entry.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn pending_count(&self) -> usize {
        let state = self.state.read().expect("McpRegistry lock poisoned");
        if state.closing {
            return 0;
        }
        state
            .entries
            .values()
            .filter(|e| !e.entry.status.is_settled())
            .count()
    }

    pub async fn wait_ready(&self) {
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.pending_count() == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Stop startup/publication and interrupt I/O before peer teardown awaits.
    pub(crate) fn begin_close(&self) -> Result<(), String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "incomplete MCP cleanup: registry poisoned")?;
        state.closing = true;
        for entry in state.entries.values() {
            if let Some(server) = &entry.server {
                server.begin_close();
            }
        }
        self.settled.notify_waiters();
        Ok(())
    }

    /// Close sequentially with one budget per child, not a constant peer-wide
    /// deadline. Ownership is removed only after BOTH explicit reap and join.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.begin_close()?;
        let names = {
            let state = self
                .state
                .read()
                .map_err(|_| "incomplete MCP cleanup: registry poisoned")?;
            let mut names: Vec<_> = state.entries.keys().cloned().collect();
            names.sort();
            names
        };
        let mut failures = Vec::new();
        for name in names {
            let budget = CloseBudget::new();
            // Retain the entry itself across awaits, without holding the map's
            // synchronous lock. Concurrent/double close shares its join slot.
            let entry = {
                let state = self
                    .state
                    .read()
                    .map_err(|_| "incomplete MCP cleanup: registry poisoned")?;
                state
                    .entries
                    .get(&name)
                    .map(|e| (e.server.clone(), e.startup.clone()))
            };
            let Some((server, startup)) = entry else {
                continue;
            };
            let result = async {
                let mut startup = timeout_at(budget.end, startup.lock())
                    .await
                    .map_err(|_| "incomplete MCP cleanup: startup lock timed out".to_string())?;
                if let Some(handle) = startup.as_mut() {
                    handle.abort();
                    let result = timeout_at(budget.end, handle).await.map_err(|_| {
                        "incomplete MCP cleanup: startup join timed out".to_string()
                    })?;
                    startup.take();
                    result
                        .or_else(|e| if e.is_cancelled() { Ok(()) } else { Err(e) })
                        .map_err(|e| format!("incomplete MCP cleanup: startup join failed: {e}"))?;
                }
                if let Some(server) = &server {
                    server.shutdown_with_budget(budget).await?;
                }
                Ok::<(), String>(())
            }
            .await;
            match result {
                Ok(()) => {
                    self.state
                        .write()
                        .map_err(|_| "incomplete MCP cleanup: registry poisoned")?
                        .entries
                        .remove(&name);
                }
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn wait_ready_returns_immediately_when_nothing_announced() {
        let registry = McpRegistry::new();
        assert_eq!(registry.pending_count(), 0);
        tokio::time::timeout(Duration::from_secs(1), registry.wait_ready())
            .await
            .expect("wait_ready must not block on an empty registry");
    }

    #[tokio::test]
    async fn shutdown_waits_for_running_stdio_child_exit() {
        let script = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method", "")
    if method == "initialize":
        result = {"serverInfo":{"name":"sentinel"},"protocolVersion":"2025-11-25","capabilities":{}}
        print(json.dumps({"jsonrpc":"2.0","id":msg["id"],"result":result}), flush=True)
"#;
        let config = crate::config::McpServerConfig {
            name: "sentinel".into(),
            command: "python3".into(),
            args: Some(vec!["-u".into(), "-c".into(), script.into()]),
            env: None,
            url: None,
            default_policy: None,
            startup_timeout_secs: crate::config::default_mcp_startup_timeout_secs(),
        };
        let registry = McpRegistry::new();
        let server = Arc::new(McpServer::start(&config).unwrap());
        server.initialize().await.unwrap();
        let pid = server.process_id().await.unwrap();
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        registry.insert_running("sentinel".into(), server, None);
        registry.shutdown().await.unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn wait_ready_blocks_until_every_server_settles() {
        let registry = Arc::new(McpRegistry::new());
        registry.insert_starting("alpha".to_string());
        registry.insert_starting("beta".to_string());
        assert_eq!(registry.pending_count(), 2);

        // Prove the wait actually blocks while one server is outstanding:
        // settle only `alpha`, then assert the wait times out.
        registry.insert_failed("alpha".to_string(), "boom".to_string());
        assert_eq!(registry.pending_count(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), registry.wait_ready())
                .await
                .is_err(),
            "wait_ready must block while a server is still Starting"
        );

        // Settling the last one releases the wait.
        let r = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            r.insert_failed("beta".to_string(), "boom".to_string());
        });
        tokio::time::timeout(Duration::from_secs(5), registry.wait_ready())
            .await
            .expect("wait_ready must release once the last server settles");
        assert_eq!(registry.pending_count(), 0);
    }

    #[tokio::test]
    async fn starting_servers_are_visible_in_snapshot_before_they_settle() {
        let registry = McpRegistry::new();
        registry.insert_starting("filesystem".to_string());

        // The whole point of `Starting`: a configured server is a row from
        // the moment it is announced, not a blank until it connects.
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].name, "filesystem");
        assert!(matches!(snapshot[0].status, McpServerStatus::Starting));
        assert!(!snapshot[0].status.is_settled());

        registry.insert_failed("filesystem".to_string(), "no such command".to_string());
        let snapshot = registry.snapshot();
        assert_eq!(
            snapshot.len(),
            1,
            "settling replaces, it does not duplicate"
        );
        assert!(snapshot[0].status.is_settled());
    }
}

#[cfg(test)]
mod join_tests {
    use super::*;
    use crate::mcp::test_support::Fixture;

    #[tokio::test]
    async fn panicked_startup_reports_incomplete_without_losing_retryable_transport() {
        let registry = McpRegistry::new();
        let f = Fixture::new("eof");
        let server = Arc::new(McpServer::start(&f.config).unwrap());
        registry.launch("fixture".into(), || {
            Ok((
                server.clone(),
                tokio::spawn(async { panic!("fixture startup panic") }),
            ))
        });
        f.wait("ready").await;
        assert!(
            registry
                .shutdown()
                .await
                .unwrap_err()
                .contains("startup join failed")
        );
        assert_eq!(registry.snapshot().len(), 1);
        assert!(server.process_id().await.is_some());
        registry.shutdown().await.unwrap();
        assert!(server.process_id().await.is_none());
        assert!(registry.snapshot().is_empty());
    }
}
