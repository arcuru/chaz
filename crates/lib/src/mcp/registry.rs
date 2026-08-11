//! Process-level directory of configured MCP servers, keyed by the
//! configured server name (`McpServerConfig.name`, *not* the
//! `mcp-<name>` extension name).
//!
//! Populated by [`crate::extensions::mcp::McpExtension`]. Every configured
//! server appears here as `Starting` from the moment its extension is
//! instantiated, and reaches exactly one terminal state: `Running` once its
//! tools have been registered, or `Failed` if it could not be started in
//! time. Nothing is ever silently absent, which is what lets a caller tell
//! "still coming" apart from "not configured".
//!
//! Because server startup runs off the boot path, the directory also owns
//! the bookkeeping that makes that safe: it counts how many servers are
//! still `Starting` ([`McpRegistry::wait_ready`]) and retains the join
//! handles of the background tasks so they remain cancellable
//! ([`McpRegistry::abort_pending_tasks`]) rather than detached.
//!
//! Read off the hot path (TUI Peer→MCP settings page snapshots it once
//! per frame); the inner [`RwLock`] is uncontended in practice.

use super::server::McpServer;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Per-process MCP server directory.
#[derive(Default)]
pub struct McpRegistry {
    entries: RwLock<HashMap<String, McpRegistryEntry>>,
    /// Wakes [`McpRegistry::wait_ready`] waiters whenever a server reaches
    /// a terminal state. The entry map is the source of truth; this only
    /// avoids a poll loop.
    settled: Notify,
    /// Background startup tasks, retained so shutdown can abort them
    /// rather than leaving a detached `tokio::spawn` to outlive the peer.
    tasks: Mutex<Vec<JoinHandle<()>>>,
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
    /// Configured, and its startup task is in flight. Present from the
    /// moment the extension is instantiated until the server settles, so a
    /// configured server is a visible row from the first frame instead of
    /// appearing out of nowhere once it connects.
    ///
    /// Not a failure. A server can sit here for as long as its configured
    /// startup timeout allows.
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
    /// timeout. The error string is the message from [`McpServer::start`]
    /// or a timeout description.
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

    /// Announce a configured server before its startup task begins.
    ///
    /// Must be called synchronously, before the spawn, by every server that
    /// will later settle. [`Self::wait_ready`] reads "nothing is
    /// `Starting`" as "everything has settled", so a server announced after
    /// a waiter has already sampled the map would not be waited for.
    pub fn insert_starting(&self, name: String) {
        let mut entries = self.entries.write().expect("McpRegistry lock poisoned");
        entries.insert(
            name.clone(),
            McpRegistryEntry {
                name,
                status: McpServerStatus::Starting,
            },
        );
    }

    /// Record a server as up, with its tools already registered.
    /// `discovery_error` carries a `tools/list` failure on an otherwise
    /// healthy server.
    pub fn insert_running(
        &self,
        name: String,
        server: Arc<McpServer>,
        discovery_error: Option<String>,
    ) {
        self.settle(
            name,
            McpServerStatus::Running {
                server,
                discovery_error,
            },
        );
    }

    pub fn insert_failed(&self, name: String, error: String) {
        self.settle(name, McpServerStatus::Failed { error });
    }

    /// Write a terminal status and wake anyone waiting on quiescence.
    fn settle(&self, name: String, status: McpServerStatus) {
        {
            let mut entries = self.entries.write().expect("McpRegistry lock poisoned");
            entries.insert(name.clone(), McpRegistryEntry { name, status });
        }
        self.settled.notify_waiters();
    }

    /// All registered entries, sorted by name. Cheap snapshot — callers
    /// don't hold the lock past the call.
    pub fn snapshot(&self) -> Vec<McpRegistryEntry> {
        let entries = self.entries.read().expect("McpRegistry lock poisoned");
        let mut out: Vec<_> = entries.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Number of servers that have not settled yet.
    pub fn pending_count(&self) -> usize {
        self.entries
            .read()
            .expect("McpRegistry lock poisoned")
            .values()
            .filter(|e| !e.status.is_settled())
            .count()
    }

    /// Park until every announced server has settled.
    ///
    /// Returns immediately when nothing is pending — the common case, and
    /// the whole case when no MCP servers are configured. Uses the same
    /// lost-wakeup-safe `Notify` pattern as the server's startup gate:
    /// register interest, re-check the condition, then await.
    ///
    /// Each server's own startup timeout bounds how long this can take, so
    /// there is no separate deadline here; a hung server settles as
    /// `Failed` and releases the wait.
    pub async fn wait_ready(&self) {
        if self.pending_count() == 0 {
            return;
        }
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            // Enable the waiter before the re-check so a `notify_waiters`
            // landing between the two is not lost.
            notified.as_mut().enable();
            if self.pending_count() == 0 {
                return;
            }
            notified.await;
            if self.pending_count() == 0 {
                return;
            }
        }
    }

    /// Take ownership of a background startup task, so the spawn is
    /// retained rather than detached. Finished handles are reaped on each
    /// call, so a long-lived peer does not accumulate them.
    pub fn track_task(&self, handle: JoinHandle<()>) {
        let mut tasks = self.tasks.lock().expect("McpRegistry tasks lock poisoned");
        tasks.retain(|h| !h.is_finished());
        tasks.push(handle);
    }

    /// Abort every in-flight startup task. Idempotent.
    ///
    /// The peer has no teardown hook today — every mode ends by exiting the
    /// process, which reclaims the tasks with the runtime — so nothing
    /// calls this yet. It exists so that when one is added, cancelling MCP
    /// startup is a call rather than a redesign: the alternative, a
    /// detached `tokio::spawn`, cannot be cancelled at all.
    pub fn abort_pending_tasks(&self) {
        let Ok(mut tasks) = self.tasks.lock() else {
            // Poisoned only if a startup task panicked mid-push. Nothing
            // useful to abort, and a shutdown path is the worst place to
            // raise a second panic.
            return;
        };
        for handle in tasks.drain(..) {
            handle.abort();
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
