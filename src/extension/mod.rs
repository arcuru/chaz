//! Extension/hook framework for chaz.
//!
//! Extensions are compile-time Rust types registered with an [`ExtensionHub`]
//! during startup. They can:
//! - Contribute tools to the [`ToolRegistry`] before it is shared.
//! - Install hook handlers that fire at well-known points in the agent loop
//!   (`before_agent_start`, `tool_call`, `tool_result`, `agent_end`,
//!   `session_start`, `session_shutdown`).
//! - Register slash commands.
//!
//! Modeled after pi's `ExtensionAPI` (TypeScript). Hot reload / WASM-loaded
//! extensions are deliberately out of scope for v1 — see `extension_api_version`.
//!
//! Panic safety: hook impls must not panic. A panic in a hook will propagate
//! through the agent turn. (TODO: add `catch_unwind` isolation once the
//! `futures` crate is in the tree.)

pub mod hooks;

use crate::runtime::RuntimeMessage;
use crate::session::Session;
use crate::tool::ToolRegistry;
use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::Table;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

/// Eidetica store name where per-session extension activation/deactivation
/// events are recorded. Lives on the session DB (not the peer DB) so the
/// provenance travels with the session via sync.
pub const EXTENSIONS_STORE: &str = "extensions";

pub use hooks::{
    HookAgentEnd, HookBeforeAgentStart, HookSessionShutdown, HookSessionStart, HookToolCall,
    HookToolResult,
};

/// Persistent identifier for an extension instance.
///
/// Designed to be written into a session's eidetica DB so the active
/// extension set can be reconstructed when the session is re-opened on
/// another peer or replayed later. Each variant chooses the addressing
/// scheme that fits where the extension's code actually lives:
///
/// - `Builtin` — compiled into the chaz binary; `chaz_version` carries the
///   `CARGO_PKG_VERSION` of the binary that registered it.
/// - `Eidetica` — loaded out of an eidetica DB (think Memory-Bank for
///   extensions). `db_id` is the root entry id; `version` is a content
///   hash or eidetica-supplied identifier.
/// - `Ipld` — content-addressed via IPLD/IPFS. The CID *is* the version.
/// - `Git` — pinned to a git commit on a remote source repo. The SHA *is*
///   the version. Useful for out-of-tree extensions.
///
/// Only `Builtin` is produced today; the other variants are placeholders
/// for the loader paths that will land alongside dynamic extension support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionRef {
    Builtin {
        name: String,
        chaz_version: String,
    },
    Eidetica {
        name: String,
        db_id: String,
        version: String,
    },
    Ipld {
        name: String,
        cid: String,
    },
    Git {
        name: String,
        repo: String,
        sha: String,
    },
}

impl ExtensionRef {
    /// Construct a `Builtin` ref tagged with the current chaz binary
    /// version. This is the default for every extension compiled into the
    /// chaz binary.
    pub fn builtin(name: &str) -> Self {
        ExtensionRef::Builtin {
            name: name.to_string(),
            chaz_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Extension name, regardless of variant.
    pub fn name(&self) -> &str {
        match self {
            ExtensionRef::Builtin { name, .. }
            | ExtensionRef::Eidetica { name, .. }
            | ExtensionRef::Ipld { name, .. }
            | ExtensionRef::Git { name, .. } => name,
        }
    }

    /// Content-addressing token (binary version / DB content hash / CID /
    /// git SHA). Combined with `name`, uniquely identifies the code the
    /// session was running.
    pub fn version(&self) -> &str {
        match self {
            ExtensionRef::Builtin { chaz_version, .. } => chaz_version,
            ExtensionRef::Eidetica { version, .. } => version,
            ExtensionRef::Ipld { cid, .. } => cid,
            ExtensionRef::Git { sha, .. } => sha,
        }
    }
}

/// One activation or deactivation of an extension on a session.
///
/// Stored as rows in the session's `extensions` table (see [`EXTENSIONS_STORE`]).
/// Each row is a discrete event keyed only implicitly — eidetica's CRDT
/// merges rows from different peers without coordination. Current state is
/// derived by folding events: per `name`, the latest event by `timestamp`
/// wins (Activated → in the active set; Deactivated → not).
///
/// Provenance (which peer wrote this event) is carried by eidetica's entry
/// signing metadata, not duplicated in the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionEvent {
    Activated {
        name: String,
        extension_ref: ExtensionRef,
        timestamp: DateTime<Utc>,
    },
    Deactivated {
        name: String,
        timestamp: DateTime<Utc>,
    },
}

impl ExtensionEvent {
    pub fn name(&self) -> &str {
        match self {
            ExtensionEvent::Activated { name, .. } | ExtensionEvent::Deactivated { name, .. } => {
                name
            }
        }
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            ExtensionEvent::Activated { timestamp, .. }
            | ExtensionEvent::Deactivated { timestamp, .. } => *timestamp,
        }
    }
}

/// Read every event in the session's extension log. Order is *not*
/// guaranteed by storage — callers that care about ordering must sort by
/// [`ExtensionEvent::timestamp`].
pub async fn list_events(session_db: &Database) -> anyhow::Result<Vec<ExtensionEvent>> {
    let txn = session_db.new_transaction().await?;
    let store = txn
        .get_store::<Table<ExtensionEvent>>(EXTENSIONS_STORE)
        .await?;
    let rows = store.search(|_| true).await?;
    Ok(rows.into_iter().map(|(_, e)| e).collect())
}

/// Fold the event log into the current active-extension set.
///
/// Per extension `name`, the latest event by timestamp determines membership:
/// `Activated` keeps it in; `Deactivated` drops it. The result is sorted by
/// name for stable callers.
///
/// Public for the upcoming `/extensions` reader and replay path; tests
/// exercise it directly.
#[allow(dead_code)]
pub async fn read_active(session_db: &Database) -> anyhow::Result<Vec<ExtensionRef>> {
    let mut events = list_events(session_db).await?;
    events.sort_by_key(|e| e.timestamp());
    let mut latest: HashMap<String, ExtensionEvent> = HashMap::new();
    for e in events {
        latest.insert(e.name().to_string(), e);
    }
    let mut active: Vec<ExtensionRef> = latest
        .into_values()
        .filter_map(|e| match e {
            ExtensionEvent::Activated { extension_ref, .. } => Some(extension_ref),
            ExtensionEvent::Deactivated { .. } => None,
        })
        .collect();
    active.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(active)
}

/// Append a single event to the session's extension log.
///
/// Public for the upcoming runtime remove API (writes a `Deactivated`)
/// and for tests; the activation path goes through
/// [`ExtensionHub::record_active`] which batches writes.
#[allow(dead_code)]
pub async fn append_event(session_db: &Database, event: ExtensionEvent) -> anyhow::Result<()> {
    let txn = session_db.new_transaction().await?;
    let store = txn
        .get_store::<Table<ExtensionEvent>>(EXTENSIONS_STORE)
        .await?;
    store.insert(event).await?;
    txn.commit().await?;
    Ok(())
}

/// Decision returned from a `tool_call` hook.
#[derive(Debug)]
pub enum ToolCallDecision {
    /// Continue execution. Args may have been mutated in place.
    Continue,
    /// Skip the tool call; synthesize a result with this reason.
    Block { reason: String },
}

/// Lightweight context handed to hook implementations.
///
/// Deliberately narrower than `ToolContext` — extensions should not be
/// mutating tool scopes, grants, or hosts. Session access is exposed so
/// extensions can read history or append entries via the existing
/// `Session` API.
pub struct HookContext {
    pub agent_name: String,
    pub model: Option<String>,
    pub call_depth: usize,
    pub session: Arc<Mutex<Session>>,
}

/// Outcome of an extension-registered slash command.
///
/// Mirrors `commands::CommandOutcome::Text`/`Error` — extensions can't
/// produce session switches or session lists, which are gateway-coupled.
pub enum ExtensionCommandOutcome {
    Text(String),
    Error(String),
}

/// Handler for a slash command registered by an extension.
pub trait ExtensionCommand: Send + Sync {
    fn description(&self) -> &'static str;

    /// Invoke the command. `args` is everything after the command name,
    /// trimmed. `ctx` carries the same session/agent info as a hook.
    fn invoke<'a>(
        &'a self,
        args: &'a str,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>>;
}

/// An extension is a compile-time Rust type that wires hooks, tools, and
/// commands into the agent runtime. Implementations are registered in
/// `src/extensions/mod.rs::register_builtins`.
pub trait Extension: Send + Sync {
    fn name(&self) -> &'static str;

    /// Persistent identifier for this extension instance, serialized into
    /// the session DB so the active-extension set can be reconstructed
    /// later. Defaults to a `Builtin` ref carrying the chaz binary version
    /// — override for extensions loaded from non-binary sources (eidetica
    /// DB, IPLD, git repo).
    fn extension_ref(&self) -> ExtensionRef {
        ExtensionRef::builtin(self.name())
    }

    /// Wire hooks and commands. Called once at startup.
    fn register(self: Arc<Self>, hub: &mut ExtensionHub);

    /// Contribute tools to the registry. Called before the registry is
    /// wrapped in `Arc`, so tools land in `ScopedTools`/`ToolProfile`
    /// filtering automatically.
    fn contribute_tools(&self, _registry: &mut ToolRegistry) {}

    /// Hook ABI version. Bumped when the hook interface changes shape in
    /// a backwards-incompatible way. Orthogonal to [`extension_ref`] —
    /// `extension_ref` identifies *which* extension is loaded;
    /// `extension_api_version` identifies *which hook contract* it expects.
    fn extension_api_version(&self) -> u32 {
        1
    }
}

/// Central registry for hook handlers, extension commands, and the
/// extensions themselves. Held on `Server` as `Arc<ExtensionHub>`.
pub struct ExtensionHub {
    extensions: Vec<Arc<dyn Extension>>,
    before_agent_start: Vec<Box<dyn HookBeforeAgentStart>>,
    tool_call: Vec<Box<dyn HookToolCall>>,
    tool_result: Vec<Box<dyn HookToolResult>>,
    agent_end: Vec<Box<dyn HookAgentEnd>>,
    session_start: Vec<Box<dyn HookSessionStart>>,
    session_shutdown: Vec<Box<dyn HookSessionShutdown>>,
    commands: HashMap<String, Box<dyn ExtensionCommand>>,
    /// Names reserved by built-in slash commands; extensions cannot register
    /// these. Populated by [`ExtensionHub::reserve_builtin_commands`] during
    /// hub construction.
    reserved_command_names: std::collections::HashSet<String>,
}

impl Default for ExtensionHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionHub {
    pub fn new() -> Self {
        Self {
            extensions: Vec::new(),
            before_agent_start: Vec::new(),
            tool_call: Vec::new(),
            tool_result: Vec::new(),
            agent_end: Vec::new(),
            session_start: Vec::new(),
            session_shutdown: Vec::new(),
            commands: HashMap::new(),
            reserved_command_names: std::collections::HashSet::new(),
        }
    }

    /// Reserve built-in slash command names so extensions can't shadow them.
    pub fn reserve_builtin_commands<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.reserved_command_names
            .extend(names.into_iter().map(Into::into));
    }

    pub fn register_extension(&mut self, ext: Arc<dyn Extension>) {
        ext.clone().register(self);
        self.extensions.push(ext);
    }

    pub fn extension_names(&self) -> Vec<&'static str> {
        self.extensions.iter().map(|e| e.name()).collect()
    }

    /// Snapshot the persistent identifier of every registered extension.
    /// Intended for writing into a session's DB at `session_start` so the
    /// active-extension set can be reproduced later.
    pub fn extension_refs(&self) -> Vec<ExtensionRef> {
        self.extensions.iter().map(|e| e.extension_ref()).collect()
    }

    /// Write `Activated` events for the current extension set into the
    /// session DB's `extensions` table, skipping any whose latest event
    /// already records the same [`ExtensionRef`] as Activated. Idempotent
    /// across session re-starts when the extension set is unchanged;
    /// captures genuine adds (no prior event, or previously Deactivated)
    /// and version bumps (different `version()` on the same name).
    ///
    /// Deactivations are not synthesized here — those come from a future
    /// runtime remove API, which writes a `Deactivated` event directly.
    pub async fn record_active(&self, session_db: &Database) -> anyhow::Result<()> {
        let current = self.extension_refs();
        let existing = list_events(session_db).await?;

        let mut latest_by_name: HashMap<String, ExtensionEvent> = HashMap::new();
        let mut sorted = existing;
        sorted.sort_by_key(|e| e.timestamp());
        for e in sorted {
            latest_by_name.insert(e.name().to_string(), e);
        }

        // Force monotonicity over the observed log so a newly written event
        // strictly post-dates anything already in the table. Without this,
        // an event synced in from a peer with a skewed clock (or a test
        // that wrote a future-dated event) could end up "older" than what
        // we just appended, and the fold would silently discard our write.
        let max_seen = latest_by_name
            .values()
            .map(|e| e.timestamp())
            .max()
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let timestamp = std::cmp::max(Utc::now(), max_seen + chrono::Duration::milliseconds(1));

        let to_write: Vec<ExtensionEvent> = current
            .into_iter()
            .filter_map(|r| {
                let name = r.name().to_string();
                let needs_write = match latest_by_name.get(&name) {
                    Some(ExtensionEvent::Activated { extension_ref, .. }) => extension_ref != &r,
                    Some(ExtensionEvent::Deactivated { .. }) => true,
                    None => true,
                };
                if needs_write {
                    Some(ExtensionEvent::Activated {
                        name,
                        extension_ref: r,
                        timestamp,
                    })
                } else {
                    None
                }
            })
            .collect();

        if to_write.is_empty() {
            return Ok(());
        }
        let txn = session_db.new_transaction().await?;
        let store = txn
            .get_store::<Table<ExtensionEvent>>(EXTENSIONS_STORE)
            .await?;
        for event in to_write {
            store.insert(event).await?;
        }
        txn.commit().await?;
        Ok(())
    }

    // --- registration API used inside Extension::register ---

    pub fn on_before_agent_start(&mut self, hook: Box<dyn HookBeforeAgentStart>) {
        self.before_agent_start.push(hook);
    }

    pub fn on_tool_call(&mut self, hook: Box<dyn HookToolCall>) {
        self.tool_call.push(hook);
    }

    pub fn on_tool_result(&mut self, hook: Box<dyn HookToolResult>) {
        self.tool_result.push(hook);
    }

    pub fn on_agent_end(&mut self, hook: Box<dyn HookAgentEnd>) {
        self.agent_end.push(hook);
    }

    pub fn on_session_start(&mut self, hook: Box<dyn HookSessionStart>) {
        self.session_start.push(hook);
    }

    pub fn on_session_shutdown(&mut self, hook: Box<dyn HookSessionShutdown>) {
        self.session_shutdown.push(hook);
    }

    /// Register an extension slash command.
    ///
    /// Names colliding with a built-in or an already-registered extension
    /// command are rejected with a warning. First registration wins on
    /// cross-extension collision.
    pub fn register_command<S: Into<String>>(
        &mut self,
        name: S,
        handler: Box<dyn ExtensionCommand>,
    ) {
        let name = name.into();
        if self.reserved_command_names.contains(&name) {
            warn!(
                command = %name,
                "Extension command shadows a built-in; ignoring registration"
            );
            return;
        }
        if self.commands.contains_key(&name) {
            warn!(
                command = %name,
                "Duplicate extension command registration; keeping first registration"
            );
            return;
        }
        self.commands.insert(name, handler);
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    pub fn list_commands(&self) -> Vec<(&str, &'static str)> {
        self.commands
            .iter()
            .map(|(name, handler)| (name.as_str(), handler.description()))
            .collect()
    }

    // --- hook dispatch ---

    /// Fire `before_agent_start` for every registered handler. Each
    /// handler may append messages, which are flattened into a single
    /// vector preserving registration order.
    pub async fn fire_before_agent_start(&self, ctx: &HookContext) -> Vec<RuntimeMessage> {
        let mut out = Vec::new();
        for hook in &self.before_agent_start {
            out.extend(hook.on_before_agent_start(ctx).await);
        }
        out
    }

    /// Fire `tool_call` for every registered handler. Args are mutated in
    /// place. First `Block` short-circuits the rest.
    pub async fn fire_tool_call(
        &self,
        ctx: &HookContext,
        tool_name: &str,
        args: &mut serde_json::Value,
    ) -> ToolCallDecision {
        for hook in &self.tool_call {
            match hook.on_tool_call(ctx, tool_name, args).await {
                ToolCallDecision::Continue => {}
                ToolCallDecision::Block { reason } => return ToolCallDecision::Block { reason },
            }
        }
        ToolCallDecision::Continue
    }

    /// Fire `tool_result`. Handlers are run in registration order; each
    /// receives the (possibly transformed) result from the previous.
    pub async fn fire_tool_result(
        &self,
        ctx: &HookContext,
        tool_name: &str,
        result: String,
    ) -> String {
        let mut acc = result;
        for hook in &self.tool_result {
            acc = hook.on_tool_result(ctx, tool_name, acc).await;
        }
        acc
    }

    pub async fn fire_agent_end(&self, ctx: &HookContext) {
        for hook in &self.agent_end {
            hook.on_agent_end(ctx).await;
        }
    }

    pub async fn fire_session_start(&self, ctx: &HookContext) {
        for hook in &self.session_start {
            hook.on_session_start(ctx).await;
        }
    }

    pub async fn fire_session_shutdown(&self, ctx: &HookContext) {
        for hook in &self.session_shutdown {
            hook.on_session_shutdown(ctx).await;
        }
    }

    /// Look up and invoke an extension command by name. Returns `None`
    /// if no extension registered this name.
    pub async fn try_dispatch_command(
        &self,
        name: &str,
        args: &str,
        ctx: &HookContext,
    ) -> Option<ExtensionCommandOutcome> {
        let handler = self.commands.get(name)?;
        Some(handler.invoke(args, ctx).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeMessage;
    use crate::types::ConversationId;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;

    #[test]
    fn builtin_ref_carries_binary_version() {
        let r = ExtensionRef::builtin("heartbeat");
        match &r {
            ExtensionRef::Builtin { name, chaz_version } => {
                assert_eq!(name, "heartbeat");
                assert_eq!(chaz_version, env!("CARGO_PKG_VERSION"));
            }
            other => panic!("expected Builtin, got {other:?}"),
        }
        assert_eq!(r.name(), "heartbeat");
        assert_eq!(r.version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn name_and_version_accessors_cover_every_variant() {
        let cases = [
            (
                ExtensionRef::Builtin {
                    name: "a".into(),
                    chaz_version: "0.1.0".into(),
                },
                "a",
                "0.1.0",
            ),
            (
                ExtensionRef::Eidetica {
                    name: "b".into(),
                    db_id: "db".into(),
                    version: "v1".into(),
                },
                "b",
                "v1",
            ),
            (
                ExtensionRef::Ipld {
                    name: "c".into(),
                    cid: "bafy...".into(),
                },
                "c",
                "bafy...",
            ),
            (
                ExtensionRef::Git {
                    name: "d".into(),
                    repo: "https://example.com/r".into(),
                    sha: "deadbeef".into(),
                },
                "d",
                "deadbeef",
            ),
        ];
        for (r, expected_name, expected_version) in &cases {
            assert_eq!(r.name(), *expected_name);
            assert_eq!(r.version(), *expected_version);
        }
    }

    #[test]
    fn extension_ref_serde_round_trips_with_tag() {
        let original = ExtensionRef::Git {
            name: "loop_detector".into(),
            repo: "https://github.com/x/y".into(),
            sha: "abc123".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        // `#[serde(tag = "kind")]` produces a flat, discoverable shape.
        assert!(json.contains("\"kind\":\"git\""), "got: {json}");
        let parsed: ExtensionRef = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    struct NamedExt(&'static str);
    impl Extension for NamedExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn register(self: Arc<Self>, _hub: &mut ExtensionHub) {}
    }

    async fn make_session_db() -> (Instance, Database) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = Doc::new();
        s.set("name", "session");
        let db = user.create_database(s, &key).await.unwrap();
        (instance, db)
    }

    #[tokio::test]
    async fn read_active_on_empty_db_returns_empty() {
        let (_inst, db) = make_session_db().await;
        let active = read_active(&db).await.unwrap();
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn record_active_writes_events_for_each_extension() {
        let (_inst, db) = make_session_db().await;
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(NamedExt("alpha")));
        hub.register_extension(Arc::new(NamedExt("beta")));

        hub.record_active(&db).await.unwrap();

        let events = list_events(&db).await.unwrap();
        assert_eq!(events.len(), 2);
        let names: std::collections::HashSet<_> = events.iter().map(|e| e.name()).collect();
        assert!(names.contains("alpha"));
        assert!(names.contains("beta"));
        for e in &events {
            assert!(matches!(e, ExtensionEvent::Activated { .. }));
        }
    }

    #[tokio::test]
    async fn record_active_is_idempotent_when_set_unchanged() {
        let (_inst, db) = make_session_db().await;
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(NamedExt("alpha")));

        hub.record_active(&db).await.unwrap();
        let after_first = list_events(&db).await.unwrap().len();
        assert_eq!(after_first, 1);

        // Second call with no changes must not append a duplicate.
        hub.record_active(&db).await.unwrap();
        let after_second = list_events(&db).await.unwrap().len();
        assert_eq!(after_second, 1);
    }

    #[tokio::test]
    async fn record_active_after_deactivation_reactivates() {
        let (_inst, db) = make_session_db().await;
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(NamedExt("alpha")));

        // Initial activation.
        hub.record_active(&db).await.unwrap();
        // Directly write a Deactivated to simulate a future remove API call.
        append_event(
            &db,
            ExtensionEvent::Deactivated {
                name: "alpha".into(),
                timestamp: Utc::now() + chrono::Duration::seconds(1),
            },
        )
        .await
        .unwrap();
        assert!(read_active(&db).await.unwrap().is_empty());

        // Recording while alpha is in the hub but Deactivated in the log
        // re-activates it.
        hub.record_active(&db).await.unwrap();
        let active = read_active(&db).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name(), "alpha");
    }

    #[tokio::test]
    async fn read_active_folds_to_latest_event_per_name() {
        let (_inst, db) = make_session_db().await;
        let t0 = Utc::now();
        // alpha: Activated at t0, then Deactivated at t1 — should not be active.
        append_event(
            &db,
            ExtensionEvent::Activated {
                name: "alpha".into(),
                extension_ref: ExtensionRef::builtin("alpha"),
                timestamp: t0,
            },
        )
        .await
        .unwrap();
        append_event(
            &db,
            ExtensionEvent::Deactivated {
                name: "alpha".into(),
                timestamp: t0 + chrono::Duration::seconds(10),
            },
        )
        .await
        .unwrap();
        // beta: Activated at t0 only — should be active.
        append_event(
            &db,
            ExtensionEvent::Activated {
                name: "beta".into(),
                extension_ref: ExtensionRef::builtin("beta"),
                timestamp: t0,
            },
        )
        .await
        .unwrap();

        let active = read_active(&db).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name(), "beta");
    }

    struct VersionedExt(&'static str, &'static str);
    impl Extension for VersionedExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn extension_ref(&self) -> ExtensionRef {
            ExtensionRef::Git {
                name: self.0.to_string(),
                repo: "repo".to_string(),
                sha: self.1.to_string(),
            }
        }
        fn register(self: Arc<Self>, _hub: &mut ExtensionHub) {}
    }

    #[tokio::test]
    async fn record_active_writes_new_event_when_version_bumps() {
        let (_inst, db) = make_session_db().await;

        let mut hub_v1 = ExtensionHub::new();
        hub_v1.register_extension(Arc::new(VersionedExt("loop", "sha1")));
        hub_v1.record_active(&db).await.unwrap();
        assert_eq!(list_events(&db).await.unwrap().len(), 1);

        // Fresh hub with the same name but different SHA: must write a new
        // event so the upgrade is captured in the log.
        let mut hub_v2 = ExtensionHub::new();
        hub_v2.register_extension(Arc::new(VersionedExt("loop", "sha2")));
        hub_v2.record_active(&db).await.unwrap();
        let events = list_events(&db).await.unwrap();
        assert_eq!(events.len(), 2);
        let active = read_active(&db).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].version(), "sha2");
    }

    #[test]
    fn hub_extension_refs_returns_one_per_extension_in_order() {
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(NamedExt("alpha")));
        hub.register_extension(Arc::new(NamedExt("beta")));
        let refs = hub.extension_refs();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name(), "alpha");
        assert_eq!(refs[1].name(), "beta");
        for r in &refs {
            assert!(matches!(r, ExtensionRef::Builtin { .. }));
        }
    }

    async fn fixture_ctx() -> HookContext {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = Doc::new();
        s.set("name", "session");
        let db = user.create_database(s, &key).await.unwrap();
        let session = Session::new(ConversationId("conv".into()), db).await;
        HookContext {
            agent_name: "test_agent".into(),
            model: None,
            call_depth: 0,
            session: Arc::new(Mutex::new(session)),
        }
    }

    struct CountingHook {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl HookBeforeAgentStart for CountingHook {
        fn on_before_agent_start<'a>(
            &'a self,
            _ctx: &'a HookContext,
        ) -> Pin<Box<dyn Future<Output = Vec<RuntimeMessage>> + Send + 'a>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![RuntimeMessage::System("injected".into())]
            })
        }
    }

    #[tokio::test]
    async fn before_agent_start_runs_in_registration_order() {
        let mut hub = ExtensionHub::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        hub.on_before_agent_start(Box::new(CountingHook {
            calls: calls.clone(),
        }));
        hub.on_before_agent_start(Box::new(CountingHook {
            calls: calls.clone(),
        }));
        let ctx = fixture_ctx().await;
        let injected = hub.fire_before_agent_start(&ctx).await;
        assert_eq!(injected.len(), 2);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    struct BlockingHook;
    impl HookToolCall for BlockingHook {
        fn on_tool_call<'a>(
            &'a self,
            _ctx: &'a HookContext,
            name: &'a str,
            _args: &'a mut serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>> {
            Box::pin(async move {
                if name == "shell" {
                    ToolCallDecision::Block {
                        reason: "blocked by test".into(),
                    }
                } else {
                    ToolCallDecision::Continue
                }
            })
        }
    }

    struct MutatingHook;
    impl HookToolCall for MutatingHook {
        fn on_tool_call<'a>(
            &'a self,
            _ctx: &'a HookContext,
            _name: &'a str,
            args: &'a mut serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>> {
            Box::pin(async move {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("touched".into(), serde_json::Value::Bool(true));
                }
                ToolCallDecision::Continue
            })
        }
    }

    #[tokio::test]
    async fn tool_call_block_short_circuits_and_mutation_propagates() {
        let mut hub = ExtensionHub::new();
        hub.on_tool_call(Box::new(MutatingHook));
        hub.on_tool_call(Box::new(BlockingHook));
        let ctx = fixture_ctx().await;

        let mut args = serde_json::json!({});
        let decision = hub.fire_tool_call(&ctx, "read_file", &mut args).await;
        assert!(matches!(decision, ToolCallDecision::Continue));
        assert_eq!(args.get("touched").and_then(|v| v.as_bool()), Some(true));

        let mut args2 = serde_json::json!({});
        let decision2 = hub.fire_tool_call(&ctx, "shell", &mut args2).await;
        assert!(matches!(
            decision2,
            ToolCallDecision::Block { ref reason } if reason == "blocked by test"
        ));
    }

    struct DummyCmd;
    impl ExtensionCommand for DummyCmd {
        fn description(&self) -> &'static str {
            "test command"
        }
        fn invoke<'a>(
            &'a self,
            args: &'a str,
            _ctx: &'a HookContext,
        ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
            Box::pin(async move { ExtensionCommandOutcome::Text(format!("got: {args}")) })
        }
    }

    #[tokio::test]
    async fn command_collision_with_builtin_is_rejected() {
        let mut hub = ExtensionHub::new();
        hub.reserve_builtin_commands(["info"]);
        hub.register_command("info", Box::new(DummyCmd));
        assert!(!hub.has_command("info"));
    }

    #[tokio::test]
    async fn duplicate_extension_command_keeps_first() {
        let mut hub = ExtensionHub::new();
        hub.register_command("greet", Box::new(DummyCmd));
        struct OtherCmd;
        impl ExtensionCommand for OtherCmd {
            fn description(&self) -> &'static str {
                "other"
            }
            fn invoke<'a>(
                &'a self,
                _args: &'a str,
                _ctx: &'a HookContext,
            ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
                Box::pin(async move { ExtensionCommandOutcome::Text("other".into()) })
            }
        }
        hub.register_command("greet", Box::new(OtherCmd));
        let ctx = fixture_ctx().await;
        let out = hub
            .try_dispatch_command("greet", "x", &ctx)
            .await
            .expect("command registered");
        match out {
            ExtensionCommandOutcome::Text(s) => assert_eq!(s, "got: x"),
            _ => panic!("expected text outcome"),
        }
    }
}
