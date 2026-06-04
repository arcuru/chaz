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
//! Panic safety: hook handlers should not panic, but if one does the
//! `ExtensionHub::fire_*` paths wrap each handler invocation in
//! `FutureExt::catch_unwind`. A panic is logged with the offending
//! extension's name and hook kind, and the firing loop continues with
//! a per-hook default (empty injection list, `Continue`, untransformed
//! tool result, etc.). A panic in handler `A` will not skip handler
//! `B` or tear down the agent turn.

pub mod agent_state;
pub mod caps;
pub mod handler;
pub(crate) mod hook_bridge;
pub mod hooks;
pub mod instance;
pub mod manifest;

#[allow(unused_imports)]
pub use instance::{CapResolver, ExtensionInstance, PeerHandles, Scope, ScopeCtx, TurnCtx};

use crate::hosted_index::HostedIndex;
use crate::routine::RoutineScope;
use crate::runtime::RuntimeMessage;
use crate::session::{Session, SessionRegistry};
use crate::tool::Tool;
use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::{DocStore, Table};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

/// One kind of hook the framework can fire.
///
/// Every hook handler an extension registers is tagged with its kind, and
/// every extension must declare via [`Extension::supported_hooks`] which
/// kinds it intends to handle. The hub validates declarations match
/// registrations at startup — a registration for a kind the extension
/// didn't declare is a programming error.
///
/// Declaration serves three purposes:
/// 1. **Security**: only handlers whose extension declared the kind run.
///    For future WASM/sandboxed extensions this becomes the manifest.
/// 2. **Efficiency**: the hub can skip extensions that don't handle a
///    given kind without invoking them.
/// 3. **Inspection**: `/extensions list -v` and similar surfaces use the
///    declared sets to describe what each extension does.
///
/// Every variant fires through a `fire_<kind>` method on
/// [`ExtensionHub`] (for hook kinds) or surfaces through the
/// tool/command registries (for `Tool` / `Command`). Scheduled work no
/// longer flows through hooks — it goes through the
/// [`crate::routine::RoutineEngine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    BeforeAgentStart,
    ToolCall,
    ToolResult,
    AgentEnd,
    SessionStart,
    SessionShutdown,
    /// Extension provides one or more named tools (via
    /// [`ExtensionHub::register_tool`]).
    Tool,
    /// Extension provides one or more slash commands (via
    /// [`ExtensionHub::register_command`]).
    Command,
}

/// Eidetica store name where per-session extension activation/deactivation
/// events are recorded. Lives on the session DB (not the peer DB) so the
/// provenance travels with the session via sync.
pub const EXTENSIONS_STORE: &str = "extensions";

/// Eidetica `DocStore` name where per-session per-extension settings are
/// stored. Keys are extension names; values are JSON-serialized settings
/// blobs. Lives on the session DB so settings travel with the session.
pub const EXTENSION_SETTINGS_STORE: &str = "extension_settings";

/// Read the settings JSON for one extension on this session's DB.
/// Missing key (or any read error) yields `json!({})` rather than
/// propagating — settings absence is the normal "no overrides" state.
pub async fn read_settings(session_db: &Database, ext_name: &str) -> serde_json::Value {
    let Ok(txn) = session_db.new_transaction().await else {
        return serde_json::json!({});
    };
    let Ok(store) = txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE).await else {
        return serde_json::json!({});
    };
    match store.get_string(ext_name).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    }
}

/// Persist settings JSON for one extension on this session's DB.
/// Overwrites any prior value.
pub async fn write_settings(
    session_db: &Database,
    ext_name: &str,
    value: serde_json::Value,
) -> anyhow::Result<()> {
    let serialized = serde_json::to_string(&value)?;
    let txn = session_db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE).await?;
    store.set_string(ext_name, serialized).await?;
    txn.commit().await?;
    Ok(())
}

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

/// Fold an extension event log into the set of names whose latest
/// event is `Deactivated`.
///
/// Where [`read_active`] expects a *comprehensive* log (the session
/// path writes an `Activated` for every extension at `session_start`),
/// this expects a *sparse* log: the per-agent extension log only
/// records explicit opt-outs/opt-ins, so absence of an event means
/// "no opinion → allowed". An extension is disabled for this scope iff
/// its latest event is `Deactivated`.
///
/// Used to build the per-agent narrowing filter: an agent can only
/// remove extensions from the session's active set, never add — so the
/// dispatch path computes `session_active − read_disabled(agent_db)`.
pub async fn read_disabled(db: &Database) -> anyhow::Result<HashSet<String>> {
    let mut events = list_events(db).await?;
    events.sort_by_key(|e| e.timestamp());
    let mut latest: HashMap<String, ExtensionEvent> = HashMap::new();
    for e in events {
        latest.insert(e.name().to_string(), e);
    }
    Ok(latest
        .into_iter()
        .filter_map(|(name, e)| matches!(e, ExtensionEvent::Deactivated { .. }).then_some(name))
        .collect())
}

/// Append a single event to an extension log (session or agent DB).
///
/// Used by the runtime remove API (writes a `Deactivated`) and tests;
/// the session activation path goes through
/// [`ExtensionHub::record_active`] which batches writes.
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
///
/// `active_extensions` carries the per-session active-extension set. The
/// hub's `fire_<kind>` methods use it to skip handlers whose owner isn't
/// active for this session, so a `/extensions remove memory` immediately
/// stops the memory extension's hooks from firing on subsequent turns.
pub struct HookContext {
    pub agent_name: String,
    pub model: Option<String>,
    pub call_depth: usize,
    pub session: Arc<Mutex<Session>>,
    pub active_extensions: HashSet<String>,
    /// Handle to the running `RoutineEngine`, threaded from `Server` at
    /// context construction. Scheduling extensions resync the live heap
    /// through this after a committed schedule mutation. `None` under
    /// `--print` (no engine running) and in tests.
    pub routine_engine: Option<Arc<crate::routine::RoutineEngine>>,
}

impl HookContext {
    /// Read the settings JSON for the named extension off the current
    /// session's DB. Returns `json!({})` if no override is stored —
    /// callers typically fall back to the extension's own
    /// [`Extension::default_settings`] when keys are missing.
    pub async fn get_settings(&self, ext_name: &str) -> serde_json::Value {
        let session = self.session.lock().await;
        read_settings(session.database(), ext_name).await
    }

    /// Persist a new settings blob for the named extension on this
    /// session's DB. Overwrites any prior value.
    pub async fn set_settings(
        &self,
        ext_name: &str,
        value: serde_json::Value,
    ) -> anyhow::Result<()> {
        let session = self.session.lock().await;
        write_settings(session.database(), ext_name, value).await
    }
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

/// An extension is a compile-time Rust type that hooks into the agent
/// runtime. Its entire API surface is hook registration: tools, slash
/// commands, and lifecycle hooks all flow through [`ExtensionHub`].
/// Implementations are registered in
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

    /// Declare every hook kind this extension intends to handle. Used
    /// at runtime for inspection / future sandboxing surfaces.
    ///
    /// Tools and commands count: an extension that registers any tool
    /// must include [`HookKind::Tool`]; any command requires
    /// [`HookKind::Command`].
    fn supported_hooks(&self) -> &[HookKind];

    /// Hook ABI version. Bumped when the hook interface changes shape in
    /// a backwards-incompatible way. Orthogonal to [`extension_ref`] —
    /// `extension_ref` identifies *which* extension is loaded;
    /// `extension_api_version` identifies *which hook contract* it expects.
    fn extension_api_version(&self) -> u32 {
        1
    }

    /// Schema defaults for this extension's settings. Returned to callers
    /// of [`HookContext::get_settings`] when no per-session override has
    /// been written. Extensions with no configurable settings can leave
    /// the default `json!({})` — the framework will still hand it back
    /// uniformly.
    fn default_settings(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    // ---- Cap-based install path -----------------------------------------
    //
    // These methods drive the cap-based install flow. Extensions that
    // register nothing get sensible defaults (manifest derived from
    // name + ref + supported_hooks, no providers, empty install).

    /// Static contract the extension publishes. The default derives
    /// from [`Self::name`] + [`Self::extension_ref`] + [`Self::supported_hooks`]
    /// and declares no caps in any direction. Extensions migrating to
    /// the cap surface override this to declare their required /
    /// requested / provided capabilities.
    fn manifest(&self) -> manifest::ExtensionManifest {
        manifest::ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: self.extension_ref(),
            supported_hooks: self.supported_hooks().to_vec(),
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    // ---- Lifecycle (per-scope) ------------------------------------------
    //
    // Extensions declare the scopes they live at and the host
    // instantiates them at each scope's lifecycle event (peer start
    // / session open / agent load). Each instance publishes its
    // tools / commands / hook handlers / cap impls through typed
    // endpoints on [`ExtensionInstance`].

    /// Where this extension lives. Default: `&[Scope::Global]` — one
    /// instance per peer for the binary's lifetime.
    ///
    /// Extensions that contribute at multiple lifecycle scopes return
    /// each scope in the slice. The host instantiates them once per
    /// scope; each `instantiate()` call's `ScopeCtx` variant matches
    /// the scope being constructed. `memory` and `skills` use this
    /// to expose Global tools/commands plus per-session caps from one
    /// extension type.
    fn scopes(&self) -> &[instance::Scope] {
        &[instance::Scope::Global]
    }

    /// Construct a runtime instance of this extension at the given
    /// scope. The host calls this at the scope's lifecycle event
    /// (e.g. `session_start` for `Scope::PerSession`) and holds the
    /// returned instance until the matching teardown.
    ///
    /// Default: return a no-op [`instance::LegacyInstance`] — useful
    /// for tests that only need a registered Extension without any
    /// runtime contribution.
    fn instantiate<'a>(
        &'a self,
        _scope_ctx: instance::ScopeCtx<'a>,
    ) -> instance::InstantiateFuture<'a> {
        let manifest = self.manifest();
        Box::pin(async move {
            Ok(Arc::new(instance::LegacyInstance::new(manifest))
                as Arc<dyn instance::ExtensionInstance>)
        })
    }
}

/// Internal wrapper that tags a hook handler with the extension that
/// registered it. Owner attribution is the foundation for per-session
/// enforcement, inspection, and (eventually) sandboxing.
struct RegisteredHook<T: ?Sized> {
    // Read by the per-session filter that lands in the next step; the
    // attribution is laid down here so the wrapper shape is stable.
    #[allow(dead_code)]
    owner: &'static str,
    hook: Box<T>,
}

/// Internal wrapper that tags an extension slash command with its owner.
///
/// `Arc` (not `Box`) so the same handler can flow in from either the
/// legacy install path (which produces `Box<dyn ExtensionCommand>`,
/// converted via `Arc::from`) or an `ExtensionInstance::commands()`
/// drain (which already produces `Arc<dyn ExtensionCommand>`).
struct RegisteredCommand {
    owner: &'static str,
    handler: Arc<dyn ExtensionCommand>,
}

/// Internal wrapper that tags a registered tool with its owner.
struct RegisteredTool {
    owner: &'static str,
    tool: Arc<dyn Tool>,
}

/// Record that an extension hook handler panicked. Called from the
/// `Err` arm of every fire_* `catch_unwind` site. Free function (rather
/// than a closure) so the same shape is reused across all six hook
/// kinds; the payload is dropped (the unwinding allocator is fine to
/// release it here) to keep the fire path `Send`.
fn log_hook_panic(owner: &'static str, hook: &'static str) {
    tracing::error!(
        extension = owner,
        hook = hook,
        "extension hook handler panicked; continuing with default"
    );
}

/// Central registry for hook handlers, extension commands, and the
/// extensions themselves. Held on `Server` as `Arc<ExtensionHub>`.
pub struct ExtensionHub {
    extensions: Vec<Arc<dyn Extension>>,
    before_agent_start: Vec<RegisteredHook<dyn HookBeforeAgentStart>>,
    tool_call: Vec<RegisteredHook<dyn HookToolCall>>,
    tool_result: Vec<RegisteredHook<dyn HookToolResult>>,
    agent_end: Vec<RegisteredHook<dyn HookAgentEnd>>,
    session_start: Vec<RegisteredHook<dyn HookSessionStart>>,
    session_shutdown: Vec<RegisteredHook<dyn HookSessionShutdown>>,
    commands: HashMap<String, RegisteredCommand>,
    /// Tools registered by extensions, indexed by descriptor name.
    tools: HashMap<String, RegisteredTool>,
    /// Names reserved by built-in slash commands; extensions cannot register
    /// these. Populated by [`ExtensionHub::reserve_builtin_commands`] during
    /// hub construction.
    reserved_command_names: HashSet<String>,

    /// Reverse index: which hook kinds did each extension actually register
    /// a handler for? Populated incrementally by every `on_<kind>` call and
    /// validated against `supported_hooks()` when the extension finishes
    /// registering. Subset of declared kinds — an extension can declare a
    /// kind and not register one (legal; the slot is just empty).
    hooks_by_extension: HashMap<&'static str, HashSet<HookKind>>,
    /// Reverse index: which slash commands did each extension register?
    /// Built alongside the per-name command map for inspection.
    commands_by_extension: HashMap<&'static str, HashSet<String>>,
    /// Reverse index: which tools did each extension register?
    tools_by_extension: HashMap<&'static str, HashSet<String>>,

    /// Per-extension routine handlers, keyed by extension name. The
    /// only surviving slot on [`handler::InstalledExtension`] —
    /// populated by `drain_global_instance` from each Global
    /// instance's `routine_handler()` endpoint and consulted by
    /// [`Self::dispatch_routine`].
    installed: HashMap<String, handler::InstalledExtension>,
    /// Interned extension-name strings — the per-kind hook vectors
    /// store `owner: &'static str`, so names flowing in as `String`
    /// (from manifests) get leaked once here. Bounded (<< 100 names).
    name_intern: HashSet<&'static str>,
    /// Session registry handle the hub uses to resolve session-scoped
    /// routine fires into a `(ConversationId, Database)` so it can
    /// build per-session caps (SessionRead/Write/Settings). `None` in
    /// tests that exercise the hub in isolation; production builds set
    /// it via [`Self::set_session_registry`] after both the hub and
    /// the registry are constructed.
    session_registry: Option<Arc<SessionRegistry>>,
    /// Hosted index — the hub uses this (together with
    /// `session_registry`) to build scoped `AgentStateAdmin` handles
    /// for extensions that declare the cap. `None` until
    /// [`Self::set_hosted_index`] is called during bootstrap.
    hosted_index: Option<HostedIndex>,
    /// Per-extension agent allowlist sourced from
    /// `Config::agent_state_allowlist`. Maps extension name → allowed
    /// agent display names. An absent entry means unrestricted.
    agent_state_allowlist: HashMap<String, Vec<String>>,

    // ---- Lifecycle instance bookkeeping -----------------------------------
    //
    // The new extension model (see `instance.rs`) instantiates extensions
    // at their declared scope and holds the resulting `ExtensionInstance`
    // until the corresponding teardown. These maps are populated lazily —
    // on session_start for `PerSession`, on agent load for `PerAgent`,
    // and at the end of `install_all` for `Global`. Empty until at least
    // one extension migrates off the legacy install path.
    /// Global-scope instances: one per extension, keyed by extension
    /// name. Populated at the end of `install_all` for every extension
    /// whose `scope()` returns `Scope::Global` — that's the default,
    /// so this map ends up holding a `LegacyInstance` per extension
    /// during the migration window.
    global_instances: HashMap<String, Arc<dyn instance::ExtensionInstance>>,

    /// Per-agent instances, keyed by `(agent_db_id, extension_name)`.
    /// Empty until per-agent extensions land. RwLock so the lazy
    /// agent-load path can populate from a `&self` dispatch point;
    /// reads dominate.
    agent_instances: tokio::sync::RwLock<
        HashMap<(eidetica::entry::ID, String), Arc<dyn instance::ExtensionInstance>>,
    >,

    /// Per-session instances, keyed by `(session_db_id, extension_name)`.
    /// Populated lazily on first dispatch for a session (the
    /// session_start hook surface is the future eager trigger). RwLock
    /// so context-tail / prompt-augmentation dispatch can populate
    /// from `&self`.
    session_instances:
        tokio::sync::RwLock<HashMap<(String, String), Arc<dyn instance::ExtensionInstance>>>,

    /// Peer-handle bag built from the same deps `install_all` uses,
    /// stored so per-session/per-agent instantiation can hand it to
    /// `ScopeCtx` without rebuilding from scratch on every event.
    /// `None` until the host wires it via [`Self::set_peer_handles`].
    peer_handles: Option<Arc<instance::PeerHandles>>,
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
            tools: HashMap::new(),
            reserved_command_names: HashSet::new(),
            hooks_by_extension: HashMap::new(),
            commands_by_extension: HashMap::new(),
            tools_by_extension: HashMap::new(),
            installed: HashMap::new(),
            name_intern: HashSet::new(),
            session_registry: None,
            hosted_index: None,
            agent_state_allowlist: HashMap::new(),
            global_instances: HashMap::new(),
            agent_instances: tokio::sync::RwLock::new(HashMap::new()),
            session_instances: tokio::sync::RwLock::new(HashMap::new()),
            peer_handles: None,
        }
    }

    /// Install the peer-handle bag used to construct
    /// [`instance::ScopeCtx`] for lifecycle instantiation. Must be set
    /// before any per-session / per-agent extension instantiates.
    /// Idempotent.
    pub fn set_peer_handles(&mut self, handles: Arc<instance::PeerHandles>) {
        self.peer_handles = Some(handles);
    }

    /// Compose the live instance set for one turn:
    /// `global ∪ agent ∪ session`. Per-session overrides per-agent
    /// overrides global when an extension is present at multiple
    /// scopes (today this can't happen, but the precedence is part of
    /// the contract for when it will).
    ///
    /// `agent_db_id` is the agent's database ID (for the per-agent
    /// lookup); `session_db_id` is the session DB's root ID string.
    /// Either may produce no matches — global-only extensions still
    /// fire.
    pub async fn instances_for_turn(
        &self,
        agent_db_id: Option<&eidetica::entry::ID>,
        session_db_id: Option<&str>,
    ) -> Vec<Arc<dyn instance::ExtensionInstance>> {
        // Walk global, then agent, then session, deduping by extension
        // name. Later wins so per-session > per-agent > global.
        let mut by_name: HashMap<String, Arc<dyn instance::ExtensionInstance>> = HashMap::new();
        for (name, inst) in &self.global_instances {
            by_name.insert(name.clone(), inst.clone());
        }
        if let Some(agent_id) = agent_db_id {
            let agents = self.agent_instances.read().await;
            for ((id, name), inst) in agents.iter() {
                if id == agent_id {
                    by_name.insert(name.clone(), inst.clone());
                }
            }
        }
        if let Some(session_id) = session_db_id {
            let sessions = self.session_instances.read().await;
            for ((id, name), inst) in sessions.iter() {
                if id == session_id {
                    by_name.insert(name.clone(), inst.clone());
                }
            }
        }
        by_name.into_values().collect()
    }

    /// Lazily instantiate every `Scope::PerSession` extension for the
    /// given session DB if not already cached. Idempotent — subsequent
    /// calls for the same session are cheap no-ops. Returns the set of
    /// instances for the session (cloned `Arc`s).
    ///
    /// Called by the dispatch path before iterating per-session
    /// endpoints. The session_start hook surface will eventually drive
    /// eager instantiation, but lazy is good enough for the first
    /// migration: the cost is a single read-then-maybe-write the first
    /// time a session is dispatched, and zero on every subsequent
    /// dispatch.
    pub async fn ensure_session_instances(
        &self,
        session_db: &eidetica::Database,
    ) -> Vec<Arc<dyn instance::ExtensionInstance>> {
        let session_id = session_db.root_id().to_string();
        // Fast path: already populated for this session.
        {
            let read = self.session_instances.read().await;
            let existing: Vec<_> = read
                .iter()
                .filter(|((sid, _), _)| sid == &session_id)
                .map(|(_, inst)| inst.clone())
                .collect();
            if !existing.is_empty() {
                return existing;
            }
        }

        // Need to populate. We can only build instances if
        // `peer_handles` has been wired by the host. Without it, fall
        // back to "no per-session instances" — extensions stay on the
        // legacy path.
        let Some(peer) = self.peer_handles.clone() else {
            return Vec::new();
        };

        let mut built: Vec<(String, Arc<dyn instance::ExtensionInstance>)> = Vec::new();
        for ext in &self.extensions {
            if !ext.scopes().contains(&instance::Scope::PerSession) {
                continue;
            }
            let scope_ctx = instance::ScopeCtx::Session {
                peer: &peer,
                session_db_id: &session_id,
                session_db,
            };
            match ext.instantiate(scope_ctx).await {
                Ok(inst) => {
                    let name = inst.manifest().name.clone();
                    built.push((name, inst));
                }
                Err(e) => {
                    warn!(
                        extension = %ext.manifest().name,
                        session = %session_id,
                        error = %e,
                        "Per-session instantiate failed; extension inactive for this session"
                    );
                }
            }
        }

        let mut write = self.session_instances.write().await;
        let mut out = Vec::with_capacity(built.len());
        for (name, inst) in built {
            // Another concurrent caller may have populated since the
            // read above — first-write-wins to keep instance identity
            // stable across a session.
            let entry = write
                .entry((session_id.clone(), name))
                .or_insert_with(|| inst.clone());
            out.push(entry.clone());
        }
        out
    }

    /// True if any registered extension declares [`Scope::PerAgent`].
    /// The per-agent lifecycle is zero-cost until at least one extension
    /// opts in: the dispatch path skips agent-DB resolution entirely
    /// when this returns false.
    fn has_per_agent_extensions(&self) -> bool {
        self.extensions
            .iter()
            .any(|e| e.scopes().contains(&instance::Scope::PerAgent))
    }

    /// Lazily instantiate every [`Scope::PerAgent`] extension for the
    /// given agent DB if not already cached. Idempotent; the mirror of
    /// [`Self::ensure_session_instances`], keyed by the agent DB's root
    /// ID so two sessions driven by the same agent share one instance.
    ///
    /// Like the session path, instantiation needs `peer_handles`; without
    /// it (isolated tests) this is a no-op and the agent contributes no
    /// per-agent instances.
    pub async fn ensure_agent_instances(
        &self,
        agent_name: &str,
        agent_db: &eidetica::Database,
    ) -> Vec<Arc<dyn instance::ExtensionInstance>> {
        let agent_id = agent_db.root_id().clone();
        // Fast path: already populated for this agent.
        {
            let read = self.agent_instances.read().await;
            let existing: Vec<_> = read
                .iter()
                .filter(|((aid, _), _)| aid == &agent_id)
                .map(|(_, inst)| inst.clone())
                .collect();
            if !existing.is_empty() {
                return existing;
            }
        }

        let Some(peer) = self.peer_handles.clone() else {
            return Vec::new();
        };

        let mut built: Vec<(String, Arc<dyn instance::ExtensionInstance>)> = Vec::new();
        for ext in &self.extensions {
            if !ext.scopes().contains(&instance::Scope::PerAgent) {
                continue;
            }
            let scope_ctx = instance::ScopeCtx::Agent {
                peer: &peer,
                agent_name,
                agent_db,
            };
            match ext.instantiate(scope_ctx).await {
                Ok(inst) => {
                    let name = inst.manifest().name.clone();
                    built.push((name, inst));
                }
                Err(e) => {
                    warn!(
                        extension = %ext.manifest().name,
                        agent = %agent_name,
                        error = %e,
                        "Per-agent instantiate failed; extension inactive for this agent"
                    );
                }
            }
        }

        let mut write = self.agent_instances.write().await;
        let mut out = Vec::with_capacity(built.len());
        for (name, inst) in built {
            let entry = write
                .entry((agent_id.clone(), name))
                .or_insert_with(|| inst.clone());
            out.push(entry.clone());
        }
        out
    }

    /// Resolve and ensure per-agent instances by agent display name.
    ///
    /// Opens the agent's Living Agent DB through the running server
    /// (reached via `peer_handles.server_cell`) and delegates to
    /// [`Self::ensure_agent_instances`]. Returns empty when no extension
    /// declares [`Scope::PerAgent`], when the server isn't wired yet
    /// (isolated tests), or when the agent isn't hosted by this peer.
    async fn ensure_agent_instances_for_name(
        &self,
        agent_name: &str,
    ) -> Vec<Arc<dyn instance::ExtensionInstance>> {
        if !self.has_per_agent_extensions() {
            return Vec::new();
        }
        let Some(peer) = self.peer_handles.as_ref() else {
            return Vec::new();
        };
        let Some(server) = peer.server_cell.get() else {
            return Vec::new();
        };
        let Some(adb) = server.open_agent_db_by_name(agent_name).await else {
            return Vec::new();
        };
        self.ensure_agent_instances(agent_name, adb.database())
            .await
    }

    /// Compose the per-turn instance set that contributes to LLM context
    /// (prompt augmentation, context tails): per-session ∪ per-agent,
    /// deduped by extension name with per-session winning. Global
    /// instances are intentionally excluded here — they don't carry the
    /// session/agent context those endpoints need, and never have.
    async fn context_instances(
        &self,
        agent_name: &str,
        session_db: Option<&Database>,
    ) -> Vec<Arc<dyn instance::ExtensionInstance>> {
        let mut by_name: HashMap<String, Arc<dyn instance::ExtensionInstance>> = HashMap::new();
        // Agent first, so a same-named per-session instance overwrites it.
        for inst in self.ensure_agent_instances_for_name(agent_name).await {
            by_name.insert(inst.manifest().name.clone(), inst);
        }
        if let Some(db) = session_db {
            for inst in self.ensure_session_instances(db).await {
                by_name.insert(inst.manifest().name.clone(), inst);
            }
        }
        by_name.into_values().collect()
    }

    /// Install the session registry the hub uses to resolve session-
    /// scoped routine fires into per-session caps. Called once at
    /// startup from chaz's main, after both [`Self::new`] and
    /// [`SessionRegistry::new`]. Idempotent — later calls overwrite.
    pub fn set_session_registry(&mut self, registry: Arc<SessionRegistry>) {
        self.session_registry = Some(registry);
    }

    /// Install the hosted index the hub uses to build scoped
    /// `AgentStateAdmin` handles. Called once at startup from chaz's
    /// main. Must be set before `install_all`.
    pub fn set_hosted_index(&mut self, index: HostedIndex) {
        self.hosted_index = Some(index);
    }

    /// Install the per-extension agent allowlist sourced from
    /// `Config::agent_state_allowlist`. Called once at startup from
    /// chaz's main. Must be set before `install_all`.
    pub fn set_agent_state_allowlist(&mut self, allowlist: HashMap<String, Vec<String>>) {
        self.agent_state_allowlist = allowlist;
    }

    /// Snapshot of `InstalledExtension` for a registered extension.
    /// Returns `None` for extensions with no routine handler.
    pub fn installed_for(&self, name: &str) -> Option<&handler::InstalledExtension> {
        self.installed.get(name)
    }

    /// Dispatch one routine fire to the named extension's routine
    /// handler.
    ///
    /// `scope` is recorded for tracing; the routine handler reads the
    /// session/agent it targets out of its own opaque payload, not from
    /// the dispatch call.
    ///
    /// Returns `Ok(())` if dispatch succeeded (the handler returned
    /// `Ok`); `Err(...)` if the handler errored, the extension isn't
    /// installed, or the installed extension didn't register a routine
    /// handler. The engine's failure-handling pass uses the `Err` path
    /// to drive `consecutive_failures` / auto-disable.
    pub async fn dispatch_routine(
        &self,
        extension: &str,
        scope: &RoutineScope,
        payload: serde_json::Value,
    ) -> anyhow::Result<()> {
        let installed = self.installed.get(extension).ok_or_else(|| {
            anyhow::anyhow!("no installed extension named '{extension}' to dispatch routine to")
        })?;
        let handler = installed.routine_handler.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "extension '{extension}' has no routine_handler — declare it in install()"
            )
        })?;
        tracing::debug!(extension, ?scope, "dispatching routine fire");
        handler.on_fire(payload).await
    }

    /// Build a turn-scoped [`HubCapResolver`] that walks
    /// `instances_for_turn(agent_db_id, session_db_id)`.
    ///
    /// One per turn — cheap to build (snapshots `Arc`s out of the live
    /// instance map) and small enough to live on the stack of the
    /// caller. The resolver is `Send + Sync + 'static` so endpoints
    /// can shove it across `.await` points without lifetime
    /// gymnastics.
    pub async fn cap_resolver_for_turn(
        &self,
        agent_db_id: Option<&eidetica::entry::ID>,
        session_db_id: Option<&str>,
    ) -> HubCapResolver {
        // Lazy session-instance population — same trigger as the
        // augment_system_prompt / context_tails paths.
        if let Some(session_id) = session_db_id
            && let Some(reg) = self.session_registry.as_ref()
            && let Ok((_conv_id, session_db)) = reg.open_session(session_id).await
        {
            let _ = self.ensure_session_instances(&session_db).await;
        }
        let instances = self.instances_for_turn(agent_db_id, session_db_id).await;
        HubCapResolver::snapshot(instances)
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

    pub fn extension_names(&self) -> Vec<&'static str> {
        self.extensions.iter().map(|e| e.name()).collect()
    }

    /// Hook kinds an extension actually registered a handler for. Always
    /// a subset of [`Extension::supported_hooks`]. Empty for unknown
    /// extension names.
    pub fn hooks_for(&self, name: &str) -> HashSet<HookKind> {
        self.hooks_by_extension
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Extensions (by name) that have a handler registered for `kind`.
    /// Order matches registration order; useful for iterating dispatch
    /// candidates with deterministic precedence.
    pub fn extensions_for_kind(&self, kind: HookKind) -> Vec<&'static str> {
        self.extensions
            .iter()
            .map(|e| e.name())
            .filter(|n| {
                self.hooks_by_extension
                    .get(n)
                    .map(|s| s.contains(&kind))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Slash commands registered by each extension. Empty for unknown
    /// extension names.
    pub fn commands_for(&self, name: &str) -> HashSet<String> {
        self.commands_by_extension
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Tools registered by each extension. Empty for unknown names.
    pub fn tools_for(&self, name: &str) -> HashSet<String> {
        self.tools_by_extension
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Iterate every registered tool as `(owner, name, Arc<dyn Tool>)`.
    /// Used by `main.rs` to populate the legacy [`crate::tool::ToolRegistry`]
    /// from the hub's hook-registered tools, attributed for per-session
    /// active-set filtering.
    pub fn tools_for_registry(&self) -> Vec<(&'static str, String, Arc<dyn Tool>)> {
        self.tools
            .iter()
            .map(|(name, reg)| (reg.owner, name.clone(), reg.tool.clone()))
            .collect()
    }

    /// Owner extension of a given tool, or `None` for unknown names.
    pub fn tool_owner(&self, name: &str) -> Option<&'static str> {
        self.tools.get(name).map(|r| r.owner)
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
                    // Respect explicit removal: `/extensions remove X`
                    // wrote a Deactivated and that decision persists across
                    // restarts. Re-activation is a deliberate user action,
                    // not something `record_active` should synthesize.
                    Some(ExtensionEvent::Deactivated { .. }) => false,
                    // No prior event for this name — extension is new (or
                    // this is a brand-new session). Default-include.
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

    /// Record that `owner` has a handler for `kind` in the reverse
    /// index. Called by the install_all drain helpers
    /// ([`Self::register_tool_attributed`] /
    /// [`Self::register_command_attributed`]) and the hook-bridge wiring.
    fn note_hook(&mut self, owner: &'static str, kind: HookKind) {
        self.hooks_by_extension
            .entry(owner)
            .or_default()
            .insert(kind);
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    pub fn list_commands(&self) -> Vec<(&str, &'static str)> {
        self.commands
            .iter()
            .map(|(name, reg)| (name.as_str(), reg.handler.description()))
            .collect()
    }

    /// Owner extension of a given slash command, or `None` for unknown
    /// names. Useful for the upcoming per-session command-dispatch filter.
    pub fn command_owner(&self, name: &str) -> Option<&'static str> {
        self.commands.get(name).map(|r| r.owner)
    }

    // --- hook dispatch ---
    //
    // Every fire_* method filters by `ctx.active_extensions` — a handler
    // whose owner extension isn't in the session's active set is skipped.
    // The active set is computed from the session's `extensions` event log
    // and cached on `Server`; see `Server::active_extensions`.
    //
    // Each handler invocation is wrapped in `FutureExt::catch_unwind`.
    // A panicking handler is logged via `log_hook_panic` and the firing
    // loop continues with a per-hook default.

    /// Fire `before_agent_start` for every active handler. Each handler
    /// may append messages, which are flattened into a single vector
    /// preserving registration order.
    pub async fn fire_before_agent_start(&self, ctx: &HookContext) -> Vec<RuntimeMessage> {
        let mut out = Vec::new();
        for reg in &self.before_agent_start {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            let fut = AssertUnwindSafe(reg.hook.on_before_agent_start(ctx));
            match fut.catch_unwind().await {
                Ok(msgs) => out.extend(msgs),
                Err(_) => log_hook_panic(reg.owner, "before_agent_start"),
            }
        }
        out
    }

    /// Fire `tool_call` for every active handler. Args are mutated in
    /// place. First `Block` short-circuits the rest.
    pub async fn fire_tool_call(
        &self,
        ctx: &HookContext,
        tool_name: &str,
        args: &mut serde_json::Value,
    ) -> ToolCallDecision {
        for reg in &self.tool_call {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            let fut = AssertUnwindSafe(reg.hook.on_tool_call(ctx, tool_name, args));
            match fut.catch_unwind().await {
                Ok(ToolCallDecision::Continue) => {}
                Ok(ToolCallDecision::Block { reason }) => {
                    return ToolCallDecision::Block { reason };
                }
                Err(_) => log_hook_panic(reg.owner, "tool_call"),
            }
        }
        ToolCallDecision::Continue
    }

    /// Fire `tool_result`. Active handlers run in registration order;
    /// each receives the (possibly transformed) result from the previous.
    pub async fn fire_tool_result(
        &self,
        ctx: &HookContext,
        tool_name: &str,
        result: String,
    ) -> String {
        let mut acc = result;
        for reg in &self.tool_result {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            // Clone for the call so a panic leaves the prior `acc`
            // intact (the input was moved into the future).
            let prev = acc.clone();
            let fut = AssertUnwindSafe(reg.hook.on_tool_result(ctx, tool_name, acc));
            acc = match fut.catch_unwind().await {
                Ok(s) => s,
                Err(_) => {
                    log_hook_panic(reg.owner, "tool_result");
                    prev
                }
            };
        }
        acc
    }

    pub async fn fire_agent_end(&self, ctx: &HookContext) {
        for reg in &self.agent_end {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            let fut = AssertUnwindSafe(reg.hook.on_agent_end(ctx));
            if fut.catch_unwind().await.is_err() {
                log_hook_panic(reg.owner, "agent_end");
            }
        }
    }

    pub async fn fire_session_start(&self, ctx: &HookContext) {
        for reg in &self.session_start {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            let fut = AssertUnwindSafe(reg.hook.on_session_start(ctx));
            if fut.catch_unwind().await.is_err() {
                log_hook_panic(reg.owner, "session_start");
            }
        }
    }

    pub async fn fire_session_shutdown(&self, ctx: &HookContext) {
        for reg in &self.session_shutdown {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            let fut = AssertUnwindSafe(reg.hook.on_session_shutdown(ctx));
            if fut.catch_unwind().await.is_err() {
                log_hook_panic(reg.owner, "session_shutdown");
            }
        }
    }

    /// Look up and invoke an extension command by name. Returns `None`
    /// if no extension registered this name OR if the owner extension is
    /// not in the calling context's active set (per-session enforcement).
    pub async fn try_dispatch_command(
        &self,
        name: &str,
        args: &str,
        ctx: &HookContext,
    ) -> Option<ExtensionCommandOutcome> {
        let reg = self.commands.get(name)?;
        if !ctx.active_extensions.contains(reg.owner) {
            return None;
        }
        Some(reg.handler.invoke(args, ctx).await)
    }

    // -----------------------------------------------------------------
    // install_all — instance-only lifecycle
    // -----------------------------------------------------------------

    /// Register every extension in `extensions` with the hub:
    ///
    /// 1. Collect manifests and run per-manifest validation.
    /// 2. For each extension whose declared scope set includes
    ///    `Scope::Global`, instantiate now (via `Extension::instantiate`)
    ///    and drain the instance's tools / commands / hook handlers
    ///    into the hub's runtime registries. Per-session and per-agent
    ///    extensions instantiate lazily at their lifecycle event
    ///    (`ensure_session_instances`, etc.) — they're only registered
    ///    here, not instantiated.
    ///
    /// Idempotent across calls: an extension already present in
    /// `global_instances` is skipped.
    ///
    /// Without `peer_handles` set, the Global instantiation phase is a
    /// no-op (extensions are recorded but their tools/commands/hooks
    /// don't surface). The host wires `peer_handles` after constructing
    /// the hub but before opening any session.
    pub async fn install_all(&mut self, extensions: Vec<Arc<dyn Extension>>) -> anyhow::Result<()> {
        let manifests: Vec<manifest::ExtensionManifest> =
            extensions.iter().map(|e| e.manifest()).collect();
        for m in &manifests {
            m.validate()?;
        }
        for ext in &extensions {
            self.extensions.push(ext.clone());
        }

        let Some(peer) = self.peer_handles.clone() else {
            return Ok(());
        };

        for ext in extensions.iter() {
            if !ext.scopes().contains(&instance::Scope::Global) {
                continue;
            }
            let m = ext.manifest();
            if self.global_instances.contains_key(&m.name) {
                continue;
            }
            let scope_ctx = instance::ScopeCtx::Global { peer: &peer };
            let inst = ext.instantiate(scope_ctx).await?;
            self.drain_global_instance(&m.name, &inst);
            self.global_instances.insert(m.name.clone(), inst);
        }

        Ok(())
    }

    /// Drain one Global instance's tools / commands / hook handlers
    /// into the legacy registries, so the existing `fire_*` paths and
    /// the [`crate::tool::ToolRegistry`] keep working unchanged.
    ///
    /// Called from [`Self::install_all`] right after a Global instance
    /// is built. Migrated extensions return real values here; legacy
    /// extensions (default `LegacyInstance`) return everything empty
    /// and the drain is a no-op for them.
    fn drain_global_instance(&mut self, owner: &str, inst: &Arc<dyn instance::ExtensionInstance>) {
        for tool in inst.tools() {
            self.register_tool_attributed(owner, tool);
        }
        for (name, handler) in inst.commands() {
            self.register_command_attributed_arc(owner, name, handler);
        }
        let owner_static = self.intern_name(owner);
        if let Some(h) = inst.before_agent_start_hook() {
            self.hooks_by_extension
                .entry(owner_static)
                .or_default()
                .insert(HookKind::BeforeAgentStart);
            self.before_agent_start.push(RegisteredHook {
                owner: owner_static,
                hook: Box::new(hook_bridge::BeforeAgentStartAdapter::new(Box::new(h))),
            });
        }
        if let Some(h) = inst.tool_call_hook() {
            self.hooks_by_extension
                .entry(owner_static)
                .or_default()
                .insert(HookKind::ToolCall);
            self.tool_call.push(RegisteredHook {
                owner: owner_static,
                hook: Box::new(hook_bridge::ToolCallAdapter::new(Box::new(h))),
            });
        }
        if let Some(h) = inst.tool_result_hook() {
            self.hooks_by_extension
                .entry(owner_static)
                .or_default()
                .insert(HookKind::ToolResult);
            self.tool_result.push(RegisteredHook {
                owner: owner_static,
                hook: Box::new(hook_bridge::ToolResultAdapter::new(Box::new(h))),
            });
        }
        if let Some(h) = inst.agent_end_hook() {
            self.hooks_by_extension
                .entry(owner_static)
                .or_default()
                .insert(HookKind::AgentEnd);
            self.agent_end.push(RegisteredHook {
                owner: owner_static,
                hook: Box::new(hook_bridge::AgentEndAdapter::new(Box::new(h))),
            });
        }
        if let Some(h) = inst.session_start_hook() {
            self.hooks_by_extension
                .entry(owner_static)
                .or_default()
                .insert(HookKind::SessionStart);
            self.session_start.push(RegisteredHook {
                owner: owner_static,
                hook: Box::new(hook_bridge::SessionStartAdapter::new(Box::new(h))),
            });
        }
        if let Some(h) = inst.session_shutdown_hook() {
            self.hooks_by_extension
                .entry(owner_static)
                .or_default()
                .insert(HookKind::SessionShutdown);
            self.session_shutdown.push(RegisteredHook {
                owner: owner_static,
                hook: Box::new(hook_bridge::SessionShutdownAdapter::new(Box::new(h))),
            });
        }
        if let Some(h) = inst.routine_handler() {
            // dispatch_routine consults `installed[name].routine_handler`.
            self.installed
                .entry(owner.to_string())
                .or_insert_with(handler::InstalledExtension::empty)
                .routine_handler = Some(Box::new(h));
        }
    }

    /// Collect system prompt augmentations from all installed extensions
    /// that provide the PromptAugmentation cap.
    ///
    /// Iterates installed extensions, calls each PromptAugmentation provider,
    /// concatenates non-empty results with blank-line separators.
    /// Per-session extension filtering: if active_extensions is Some, only
    /// extensions in that set participate.
    pub async fn augment_system_prompt(
        &self,
        agent_name: &str,
        recent_message_text: &[String],
        active_extensions: Option<&[String]>,
        session_db: Option<&Database>,
    ) -> String {
        let mut parts: Vec<String> = Vec::new();

        for inst in self.context_instances(agent_name, session_db).await {
            let name = inst.manifest().name.clone();
            if let Some(active) = active_extensions
                && !active.iter().any(|a| a == name.as_str())
            {
                continue;
            }
            if let Some(pa) = inst.prompt_augmentation()
                && let Ok(Some(text)) = pa
                    .augment_system_prompt(agent_name, recent_message_text)
                    .await
                && !text.trim().is_empty()
            {
                parts.push(text);
            }
        }
        parts.join("\n\n")
    }

    /// Collect context tail augmentations from all per-session instances
    /// that publish a `ContextTail` endpoint.
    ///
    /// Mirrors [`Self::augment_system_prompt`] but fires at the end of
    /// context assembly — appended after the conversation messages.
    /// Per-session instances are lazily instantiated via
    /// [`Self::ensure_session_instances`]; per-session extension
    /// filtering applies (`active_extensions`).
    pub async fn context_tails(
        &self,
        agent_name: &str,
        recent_message_text: &[String],
        active_extensions: Option<&[String]>,
        session_db: Option<&Database>,
    ) -> String {
        let mut parts: Vec<String> = Vec::new();

        for inst in self.context_instances(agent_name, session_db).await {
            let name = inst.manifest().name.clone();
            if let Some(active) = active_extensions
                && !active.iter().any(|a| a == name.as_str())
            {
                continue;
            }
            if let Some(ct) = inst.context_tail()
                && let Ok(Some(text)) = ct.context_tail(agent_name, recent_message_text).await
                && !text.trim().is_empty()
            {
                parts.push(text);
            }
        }
        parts.join("\n\n")
    }
} // impl ExtensionHub

impl ExtensionHub {
    /// Intern a `String` extension name into a `&'static str`. Required
    /// for the legacy `RegisteredHook<...>::owner: &'static str` field
    /// the existing fire paths consult. Box-leaks once per unique name;
    /// safe under the chaz invariant that the hub lives for the whole
    /// process and extension names are bounded (<< 100).
    fn intern_name(&mut self, name: &str) -> &'static str {
        if let Some(existing) = self.name_intern.iter().find(|n| **n == name) {
            return existing;
        }
        let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
        self.name_intern.insert(leaked);
        leaked
    }

    /// Internal: register a tool against an explicit owner name. Used
    /// to drain the install_all pending queues, applying first-write-wins
    /// collision handling plus the reverse-index bookkeeping.
    fn register_tool_attributed(&mut self, owner: &str, tool: Arc<dyn Tool>) {
        let owner_static = self.intern_name(owner);
        let name = tool.descriptor().name;
        if self.tools.contains_key(&name) {
            warn!(
                tool = %name,
                extension = %owner_static,
                "Duplicate tool registration; keeping first registration"
            );
            return;
        }
        self.note_hook(owner_static, HookKind::Tool);
        self.tools_by_extension
            .entry(owner_static)
            .or_default()
            .insert(name.clone());
        self.tools.insert(
            name,
            RegisteredTool {
                owner: owner_static,
                tool,
            },
        );
    }

    /// Internal: register a slash command against an explicit owner,
    /// applying the first-write-wins + built-in-name reservation policy.
    /// Used by the [`ExtensionInstance`] drain path where commands
    /// arrive as `Arc<dyn ExtensionCommand>`.
    fn register_command_attributed_arc(
        &mut self,
        owner: &str,
        name: String,
        handler: Arc<dyn ExtensionCommand>,
    ) {
        let owner_static = self.intern_name(owner);
        if self.reserved_command_names.contains(&name) {
            warn!(
                command = %name,
                extension = %owner_static,
                "Extension command shadows a built-in; ignoring registration"
            );
            return;
        }
        if self.commands.contains_key(&name) {
            warn!(
                command = %name,
                extension = %owner_static,
                "Duplicate extension command registration; keeping first registration"
            );
            return;
        }
        self.note_hook(owner_static, HookKind::Command);
        self.commands_by_extension
            .entry(owner_static)
            .or_default()
            .insert(name.clone());
        self.commands.insert(
            name,
            RegisteredCommand {
                owner: owner_static,
                handler,
            },
        );
    }
}

/// Turn-scoped cap lookup that walks the live [`ExtensionInstance`]
/// set composed for the current turn. Built once per turn by
/// [`ExtensionHub::cap_resolver_for_turn`] and dropped after.
///
/// The instance set is already deduped by
/// [`ExtensionHub::instances_for_turn`] — session > agent > global
/// precedence is baked in (later wins for same-name extensions). For
/// caps with multiple providers the first instance reporting `Some`
/// from its endpoint wins; iteration order matches `HashMap`'s, which
/// is non-deterministic but irrelevant today since no two extensions
/// publish the same cap on the instance side. Once that stops being
/// true the resolver should grow explicit precedence — likely
/// last-installed-wins to match the `RoutineEngine` schedule rules.
///
/// This is the forward resolution surface for extension-to-extension
/// caps ([`caps::Messenger`] / [`caps::MemoryAccess`]) and the contract
/// [`instance::TurnCtx`] hands to endpoints. It has no production
/// consumer yet — context assembly resolves [`caps::PromptAugmentation`]
/// / [`caps::ContextTail`] by iterating instances directly (see
/// `context_instances`), and the memory/messenger caps are published
/// but not yet consumed. Kept (not deleted with the old `ExtensionCaps`
/// bundle) because it's the instance-based resolver the WASM host
/// boundary will bind against. `allow(dead_code)` until a consumer
/// wires it.
#[allow(dead_code)]
pub struct HubCapResolver {
    instances: Vec<Arc<dyn instance::ExtensionInstance>>,
}

impl HubCapResolver {
    fn snapshot(instances: Vec<Arc<dyn instance::ExtensionInstance>>) -> Self {
        Self { instances }
    }
}

impl instance::CapResolver for HubCapResolver {
    fn memory(&self) -> Option<Arc<dyn caps::MemoryAccess>> {
        self.instances.iter().find_map(|inst| inst.memory_access())
    }

    fn messenger(&self) -> Option<Arc<dyn caps::Messenger>> {
        self.instances.iter().find_map(|inst| inst.messenger())
    }

    fn context_tail(&self) -> Option<Arc<dyn caps::ContextTail>> {
        self.instances.iter().find_map(|inst| inst.context_tail())
    }

    fn prompt_augmentation(&self) -> Option<Arc<dyn caps::PromptAugmentation>> {
        self.instances
            .iter()
            .find_map(|inst| inst.prompt_augmentation())
    }

    fn extension_cap_by_id(
        &self,
        type_id: std::any::TypeId,
    ) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.instances
            .iter()
            .find_map(|inst| inst.extension_cap(type_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeMessage;
    use crate::types::ConversationId;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;
    use eidetica::{Instance, NewUser};

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
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
    }

    // ── Instance-model test helpers ─────────────────────────────────
    //
    // The hub no longer has a legacy `install()` path — every
    // extension publishes its tools / commands / hook handlers through
    // an `ExtensionInstance` drained at `install_all`. These helpers
    // let tests build a Global extension from a bag of `Arc`-d
    // handlers without spelling out a bespoke `Extension` +
    // `ExtensionInstance` pair each time.

    #[derive(Default, Clone)]
    struct TestParts {
        tool_call: Option<Arc<dyn handler::HookHandlerToolCall>>,
        before_agent_start: Option<Arc<dyn handler::HookHandlerBeforeAgentStart>>,
        tool_result: Option<Arc<dyn handler::HookHandlerToolResult>>,
        agent_end: Option<Arc<dyn handler::HookHandlerAgentEnd>>,
        session_start: Option<Arc<dyn handler::HookHandlerSessionStart>>,
        session_shutdown: Option<Arc<dyn handler::HookHandlerSessionShutdown>>,
        routine_handler: Option<Arc<dyn handler::RoutineHandler>>,
        prompt_augmentation: Option<Arc<dyn caps::PromptAugmentation>>,
        tools: Vec<Arc<dyn Tool>>,
        commands: Vec<(String, Arc<dyn ExtensionCommand>)>,
    }

    struct TestExt {
        name: &'static str,
        supported: Vec<HookKind>,
        scopes: Vec<instance::Scope>,
        parts: TestParts,
    }

    impl TestExt {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                supported: Vec::new(),
                scopes: vec![instance::Scope::Global],
                parts: TestParts::default(),
            }
        }
        fn scopes(mut self, scopes: Vec<instance::Scope>) -> Self {
            self.scopes = scopes;
            self
        }
        fn prompt_augmentation(mut self, p: Arc<dyn caps::PromptAugmentation>) -> Self {
            self.parts.prompt_augmentation = Some(p);
            self
        }
        fn tool_call(mut self, h: Arc<dyn handler::HookHandlerToolCall>) -> Self {
            self.supported.push(HookKind::ToolCall);
            self.parts.tool_call = Some(h);
            self
        }
        fn before_agent_start(mut self, h: Arc<dyn handler::HookHandlerBeforeAgentStart>) -> Self {
            self.supported.push(HookKind::BeforeAgentStart);
            self.parts.before_agent_start = Some(h);
            self
        }
        fn routine_handler(mut self, h: Arc<dyn handler::RoutineHandler>) -> Self {
            self.parts.routine_handler = Some(h);
            self
        }
        fn command(mut self, name: &str, h: Arc<dyn ExtensionCommand>) -> Self {
            self.supported.push(HookKind::Command);
            self.parts.commands.push((name.to_string(), h));
            self
        }
    }

    impl Extension for TestExt {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &self.supported
        }
        fn scopes(&self) -> &[instance::Scope] {
            &self.scopes
        }
        fn instantiate<'a>(
            &'a self,
            _scope_ctx: instance::ScopeCtx<'a>,
        ) -> instance::InstantiateFuture<'a> {
            let manifest = self.manifest();
            let parts = self.parts.clone();
            Box::pin(async move {
                Ok(Arc::new(TestInstance { manifest, parts })
                    as Arc<dyn instance::ExtensionInstance>)
            })
        }
    }

    struct TestInstance {
        manifest: manifest::ExtensionManifest,
        parts: TestParts,
    }
    impl instance::ExtensionInstance for TestInstance {
        fn manifest(&self) -> &manifest::ExtensionManifest {
            &self.manifest
        }
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            self.parts.tools.clone()
        }
        fn commands(&self) -> Vec<(String, Arc<dyn ExtensionCommand>)> {
            self.parts.commands.clone()
        }
        fn tool_call_hook(&self) -> Option<Arc<dyn handler::HookHandlerToolCall>> {
            self.parts.tool_call.clone()
        }
        fn tool_result_hook(&self) -> Option<Arc<dyn handler::HookHandlerToolResult>> {
            self.parts.tool_result.clone()
        }
        fn before_agent_start_hook(&self) -> Option<Arc<dyn handler::HookHandlerBeforeAgentStart>> {
            self.parts.before_agent_start.clone()
        }
        fn agent_end_hook(&self) -> Option<Arc<dyn handler::HookHandlerAgentEnd>> {
            self.parts.agent_end.clone()
        }
        fn session_start_hook(&self) -> Option<Arc<dyn handler::HookHandlerSessionStart>> {
            self.parts.session_start.clone()
        }
        fn session_shutdown_hook(&self) -> Option<Arc<dyn handler::HookHandlerSessionShutdown>> {
            self.parts.session_shutdown.clone()
        }
        fn routine_handler(&self) -> Option<Arc<dyn handler::RoutineHandler>> {
            self.parts.routine_handler.clone()
        }
        fn prompt_augmentation(&self) -> Option<Arc<dyn caps::PromptAugmentation>> {
            self.parts.prompt_augmentation.clone()
        }
    }

    fn test_peer_handles(registry: Arc<SessionRegistry>) -> Arc<instance::PeerHandles> {
        Arc::new(instance::PeerHandles {
            registry,
            agent_index: HostedIndex::empty("agent"),
            memory_bank_index: HostedIndex::empty("bank"),
            skill_bank_index: HostedIndex::empty("skill_bank"),
            embedder: None,
            secrets: None,
            server_cell: Arc::new(std::sync::OnceLock::new()),
            mcp_registry: Arc::new(crate::mcp::McpRegistry::new()),
            agent_state_allowlist: Default::default(),
        })
    }

    /// Hub with `session_registry` + `peer_handles` wired, so
    /// `install_all` runs the Global-instance drain (tools, commands,
    /// hooks, routine handlers all surface).
    async fn test_hub() -> ExtensionHub {
        use crate::agent::AgentRegistry;
        let backend = InMemory::new();
        let (inst, user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
        let agents = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(SessionRegistry::new(inst, user, agents).await.unwrap());
        let mut hub = ExtensionHub::new();
        hub.set_session_registry(registry.clone());
        hub.set_peer_handles(test_peer_handles(registry));
        hub
    }

    async fn make_session_db() -> (Instance, Database) {
        let backend = InMemory::new();
        let (instance, mut user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = Doc::new();
        s.set("name", "session");
        let db = user.create_database(s, &key).await.unwrap();
        (instance, db)
    }

    struct FixedAug(&'static str);
    impl caps::PromptAugmentation for FixedAug {
        fn augment_system_prompt<'a>(
            &'a self,
            _agent_name: &'a str,
            _recent: &'a [String],
        ) -> caps::CapFuture<'a, Option<String>> {
            let text = self.0.to_string();
            Box::pin(async move { Ok(Some(text)) })
        }
    }

    #[tokio::test]
    async fn ensure_agent_instances_instantiates_per_agent_extensions() {
        let mut hub = test_hub().await;
        hub.install_all(vec![Arc::new(
            TestExt::new("per_agent_ext")
                .scopes(vec![instance::Scope::PerAgent])
                .prompt_augmentation(Arc::new(FixedAug("AGENT-AUG"))),
        )])
        .await
        .unwrap();

        // A PerAgent-only extension is not drained as a Global instance.
        assert!(hub.global_instances.is_empty());

        let (_inst, agent_db) = make_session_db().await;
        let got = hub.ensure_agent_instances("alpha", &agent_db).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].manifest().name, "per_agent_ext");
        assert!(got[0].prompt_augmentation().is_some());

        // Idempotent: a second call returns the same cached instance.
        let again = hub.ensure_agent_instances("alpha", &agent_db).await;
        assert_eq!(again.len(), 1);
        assert!(Arc::ptr_eq(&got[0], &again[0]));
    }

    #[tokio::test]
    async fn ensure_agent_instances_keyed_per_agent_db() {
        let mut hub = test_hub().await;
        hub.install_all(vec![Arc::new(
            TestExt::new("per_agent_ext").scopes(vec![instance::Scope::PerAgent]),
        )])
        .await
        .unwrap();

        let (_i1, db_a) = make_session_db().await;
        let (_i2, db_b) = make_session_db().await;
        let a = hub.ensure_agent_instances("alpha", &db_a).await;
        let b = hub.ensure_agent_instances("beta", &db_b).await;
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        // Distinct agent DBs get distinct instances.
        assert!(!Arc::ptr_eq(&a[0], &b[0]));
        assert_eq!(hub.agent_instances.read().await.len(), 2);
    }

    #[tokio::test]
    async fn ensure_agent_instances_noop_without_per_agent_scope() {
        // A Global-only extension yields no per-agent instances.
        let mut hub = test_hub().await;
        hub.install_all(vec![Arc::new(TestExt::new("global_ext"))])
            .await
            .unwrap();
        let (_inst, agent_db) = make_session_db().await;
        assert!(
            hub.ensure_agent_instances("alpha", &agent_db)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn settings_missing_key_returns_empty_object() {
        let (_inst, db) = make_session_db().await;
        let got = read_settings(&db, "memory").await;
        assert_eq!(got, serde_json::json!({}));
    }

    #[tokio::test]
    async fn settings_round_trip_via_helpers() {
        let (_inst, db) = make_session_db().await;
        let value = serde_json::json!({
            "max_results": 8,
            "embedder": "nomic"
        });
        write_settings(&db, "memory", value.clone()).await.unwrap();
        let got = read_settings(&db, "memory").await;
        assert_eq!(got, value);
    }

    #[tokio::test]
    async fn settings_for_two_extensions_dont_collide() {
        let (_inst, db) = make_session_db().await;
        write_settings(&db, "memory", serde_json::json!({"k": 1}))
            .await
            .unwrap();
        write_settings(&db, "heartbeat", serde_json::json!({"k": 2}))
            .await
            .unwrap();
        assert_eq!(
            read_settings(&db, "memory").await,
            serde_json::json!({"k": 1})
        );
        assert_eq!(
            read_settings(&db, "heartbeat").await,
            serde_json::json!({"k": 2})
        );
    }

    #[tokio::test]
    async fn settings_overwrite_replaces_prior_value() {
        let (_inst, db) = make_session_db().await;
        write_settings(&db, "x", serde_json::json!({"a": 1}))
            .await
            .unwrap();
        write_settings(&db, "x", serde_json::json!({"b": 2}))
            .await
            .unwrap();
        assert_eq!(read_settings(&db, "x").await, serde_json::json!({"b": 2}));
    }

    #[tokio::test]
    async fn hook_context_settings_round_trip() {
        // Build the ctx inline so the `Instance` stays alive for the
        // duration of the read/write calls — `fixture_ctx` drops it
        // before returning, which is fine for fire_* tests that don't
        // touch the DB but not for settings ops.
        let (_inst, db) = make_session_db().await;
        let session = Session::new(ConversationId("conv".into()), db).await;
        let ctx = HookContext {
            agent_name: "test_agent".into(),
            model: None,
            call_depth: 0,
            session: Arc::new(Mutex::new(session)),
            active_extensions: HashSet::new(),
            routine_engine: None,
        };
        ctx.set_settings("heartbeat", serde_json::json!({"poll_secs": 60}))
            .await
            .unwrap();
        let got = ctx.get_settings("heartbeat").await;
        assert_eq!(got, serde_json::json!({"poll_secs": 60}));
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
        hub.install_all(vec![
            Arc::new(NamedExt("alpha")),
            Arc::new(NamedExt("beta")),
        ])
        .await
        .unwrap();

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
        hub.install_all(vec![Arc::new(NamedExt("alpha"))])
            .await
            .unwrap();

        hub.record_active(&db).await.unwrap();
        let after_first = list_events(&db).await.unwrap().len();
        assert_eq!(after_first, 1);

        // Second call with no changes must not append a duplicate.
        hub.record_active(&db).await.unwrap();
        let after_second = list_events(&db).await.unwrap().len();
        assert_eq!(after_second, 1);
    }

    #[tokio::test]
    async fn record_active_respects_deactivation_across_restarts() {
        // `record_active` is the session_start reconciler. The old behavior
        // re-activated anything currently in the hub regardless of prior
        // Deactivated events — which would have undone every
        // `/extensions remove X` on the next session_start. The new
        // contract: respect explicit removal across restarts.
        let (_inst, db) = make_session_db().await;
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![Arc::new(NamedExt("alpha"))])
            .await
            .unwrap();

        hub.record_active(&db).await.unwrap();
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

        // A subsequent record_active does NOT reactivate — the Deactivated
        // event stands. Only an explicit `/extensions add X` (which writes
        // a fresh Activated) can bring it back.
        hub.record_active(&db).await.unwrap();
        assert!(
            read_active(&db).await.unwrap().is_empty(),
            "record_active should respect prior Deactivated"
        );
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

    #[tokio::test]
    async fn read_disabled_returns_only_latest_deactivated_names() {
        let (_inst, db) = make_session_db().await;
        let t0 = Utc::now();
        // memory: removed (Deactivated) — should be in the disabled set.
        append_event(
            &db,
            ExtensionEvent::Deactivated {
                name: "memory".into(),
                timestamp: t0,
            },
        )
        .await
        .unwrap();
        // web: removed then re-added — latest is Activated, so NOT disabled.
        append_event(
            &db,
            ExtensionEvent::Deactivated {
                name: "web".into(),
                timestamp: t0,
            },
        )
        .await
        .unwrap();
        append_event(
            &db,
            ExtensionEvent::Activated {
                name: "web".into(),
                extension_ref: ExtensionRef::builtin("web"),
                timestamp: t0 + chrono::Duration::seconds(10),
            },
        )
        .await
        .unwrap();

        let disabled = read_disabled(&db).await.unwrap();
        assert_eq!(disabled.len(), 1);
        assert!(disabled.contains("memory"));
        assert!(!disabled.contains("web"));
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
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
    }

    #[tokio::test]
    async fn record_active_writes_new_event_when_version_bumps() {
        let (_inst, db) = make_session_db().await;

        let mut hub_v1 = ExtensionHub::new();
        hub_v1
            .install_all(vec![Arc::new(VersionedExt("loop", "sha1"))])
            .await
            .unwrap();
        hub_v1.record_active(&db).await.unwrap();
        assert_eq!(list_events(&db).await.unwrap().len(), 1);

        // Fresh hub with the same name but different SHA: must write a new
        // event so the upgrade is captured in the log.
        let mut hub_v2 = ExtensionHub::new();
        hub_v2
            .install_all(vec![Arc::new(VersionedExt("loop", "sha2"))])
            .await
            .unwrap();
        hub_v2.record_active(&db).await.unwrap();
        let events = list_events(&db).await.unwrap();
        assert_eq!(events.len(), 2);
        let active = read_active(&db).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].version(), "sha2");
    }

    /// Always-`Continue` tool-call hook used by registration-tracking
    /// tests.
    struct PassToolCall;
    impl handler::HookHandlerToolCall for PassToolCall {
        fn on_tool_call<'a>(
            &'a self,
            _: &'a str,
            _: &'a mut serde_json::Value,
        ) -> handler::HandlerFuture<'a, ToolCallDecision> {
            Box::pin(async { ToolCallDecision::Continue })
        }
    }

    fn tool_call_ext(name: &'static str) -> Arc<dyn Extension> {
        Arc::new(TestExt::new(name).tool_call(Arc::new(PassToolCall)))
    }

    #[tokio::test]
    async fn hub_records_owner_for_each_hook_registration() {
        let mut hub = test_hub().await;
        hub.install_all(vec![tool_call_ext("alpha"), tool_call_ext("beta")])
            .await
            .unwrap();

        let alpha_kinds = hub.hooks_for("alpha");
        assert!(alpha_kinds.contains(&HookKind::ToolCall));
        let beta_kinds = hub.hooks_for("beta");
        assert!(beta_kinds.contains(&HookKind::ToolCall));
        // Other kinds untouched.
        assert!(!alpha_kinds.contains(&HookKind::ToolResult));
    }

    #[tokio::test]
    async fn extensions_for_kind_returns_only_handlers_in_registration_order() {
        let mut hub = test_hub().await;
        hub.install_all(vec![
            Arc::new(NamedExt("noop")),
            tool_call_ext("alpha"),
            tool_call_ext("beta"),
        ])
        .await
        .unwrap();
        let owners = hub.extensions_for_kind(HookKind::ToolCall);
        assert_eq!(owners, vec!["alpha", "beta"]);
        let none = hub.extensions_for_kind(HookKind::AgentEnd);
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn commands_track_owner_and_are_queryable() {
        let mut hub = test_hub().await;
        hub.install_all(vec![Arc::new(
            TestExt::new("with_command").command("dance", Arc::new(DummyCmd)),
        )])
        .await
        .unwrap();
        assert!(hub.commands_for("with_command").contains("dance"));
        assert_eq!(hub.command_owner("dance"), Some("with_command"));
        assert_eq!(hub.command_owner("not_real"), None);
    }

    #[tokio::test]
    async fn hub_extension_refs_returns_one_per_extension_in_order() {
        let mut hub = test_hub().await;
        hub.install_all(vec![
            Arc::new(NamedExt("alpha")),
            Arc::new(NamedExt("beta")),
        ])
        .await
        .unwrap();
        let refs = hub.extension_refs();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name(), "alpha");
        assert_eq!(refs[1].name(), "beta");
        for r in &refs {
            assert!(matches!(r, ExtensionRef::Builtin { .. }));
        }
    }

    /// Test-only ctx that pretends *every* extension name is active so
    /// fire_* tests can exercise handler dispatch without manually
    /// listing owners. Production code always builds a real per-session
    /// set via `Server::active_extensions_for`.
    fn all_active(names: &[&'static str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    async fn fixture_ctx_with_active(active: HashSet<String>) -> HookContext {
        let mut ctx = fixture_ctx().await;
        ctx.active_extensions = active;
        ctx
    }

    async fn fixture_ctx() -> HookContext {
        let backend = InMemory::new();
        let (_instance, mut user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
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
            active_extensions: HashSet::new(),
            routine_engine: None,
        }
    }

    struct CountingHook {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl handler::HookHandlerBeforeAgentStart for CountingHook {
        fn on_before_agent_start<'a>(&'a self) -> handler::HandlerFuture<'a, Vec<RuntimeMessage>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![RuntimeMessage::System("injected".into())]
            })
        }
    }

    fn counting_ext(
        name: &'static str,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Arc<dyn Extension> {
        Arc::new(TestExt::new(name).before_agent_start(Arc::new(CountingHook { calls })))
    }

    #[tokio::test]
    async fn before_agent_start_runs_in_registration_order() {
        let mut hub = test_hub().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        hub.install_all(vec![
            counting_ext("a", calls.clone()),
            counting_ext("b", calls.clone()),
        ])
        .await
        .unwrap();
        let ctx = fixture_ctx_with_active(all_active(&["a", "b"])).await;
        let injected = hub.fire_before_agent_start(&ctx).await;
        assert_eq!(injected.len(), 2);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn inactive_extension_does_not_fire_hooks() {
        // Only "a" is active; "b" must be skipped despite being registered.
        let mut hub = test_hub().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        hub.install_all(vec![
            counting_ext("a", calls.clone()),
            counting_ext("b", calls.clone()),
        ])
        .await
        .unwrap();
        let ctx = fixture_ctx_with_active(all_active(&["a"])).await;
        hub.fire_before_agent_start(&ctx).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    struct BlockingHook;
    impl handler::HookHandlerToolCall for BlockingHook {
        fn on_tool_call<'a>(
            &'a self,
            name: &'a str,
            _args: &'a mut serde_json::Value,
        ) -> handler::HandlerFuture<'a, ToolCallDecision> {
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
    impl handler::HookHandlerToolCall for MutatingHook {
        fn on_tool_call<'a>(
            &'a self,
            _name: &'a str,
            args: &'a mut serde_json::Value,
        ) -> handler::HandlerFuture<'a, ToolCallDecision> {
            Box::pin(async move {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("touched".into(), serde_json::Value::Bool(true));
                }
                ToolCallDecision::Continue
            })
        }
    }

    struct PanickingBeforeAgentStartHook;
    impl handler::HookHandlerBeforeAgentStart for PanickingBeforeAgentStartHook {
        fn on_before_agent_start<'a>(&'a self) -> handler::HandlerFuture<'a, Vec<RuntimeMessage>> {
            Box::pin(async move { panic!("intentional panic for catch_unwind test") })
        }
    }

    #[tokio::test]
    async fn panicking_hook_is_isolated_and_subsequent_hooks_still_run() {
        let mut hub = test_hub().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        hub.install_all(vec![
            Arc::new(
                TestExt::new("boom").before_agent_start(Arc::new(PanickingBeforeAgentStartHook)),
            ),
            counting_ext("after", calls.clone()),
        ])
        .await
        .unwrap();
        let ctx = fixture_ctx_with_active(all_active(&["boom", "after"])).await;

        // Must not panic-propagate out of `fire_*` — that's the whole
        // point of the catch_unwind wrap.
        let injected = hub.fire_before_agent_start(&ctx).await;

        // The second handler still ran (one message injected, counter
        // incremented). The panicking handler contributed nothing.
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(injected.len(), 1);
    }

    #[tokio::test]
    async fn tool_call_block_short_circuits_and_mutation_propagates() {
        let mut hub = test_hub().await;
        hub.install_all(vec![
            Arc::new(TestExt::new("mutating").tool_call(Arc::new(MutatingHook))),
            Arc::new(TestExt::new("blocking").tool_call(Arc::new(BlockingHook))),
        ])
        .await
        .unwrap();
        let ctx = fixture_ctx_with_active(all_active(&["mutating", "blocking"])).await;

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

    fn cmd_ext(name: &'static str, cmd: &'static str) -> Arc<dyn Extension> {
        Arc::new(TestExt::new(name).command(cmd, Arc::new(DummyCmd)))
    }

    #[tokio::test]
    async fn command_collision_with_builtin_is_rejected() {
        let mut hub = test_hub().await;
        hub.reserve_builtin_commands(["info"]);
        hub.install_all(vec![cmd_ext("ext", "info")]).await.unwrap();
        assert!(!hub.has_command("info"));
    }

    #[tokio::test]
    async fn duplicate_extension_command_keeps_first() {
        let mut hub = test_hub().await;
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
        // Drain order = vec order: "first" registers "greet" before
        // "second" tries to — first-write-wins keeps "first".
        hub.install_all(vec![
            cmd_ext("first", "greet"),
            Arc::new(TestExt::new("second").command("greet", Arc::new(OtherCmd))),
        ])
        .await
        .unwrap();
        // "first" wins the collision and owns the command; needs to be
        // in the active set or dispatch will return None.
        let ctx = fixture_ctx_with_active(all_active(&["first"])).await;
        let out = hub
            .try_dispatch_command("greet", "x", &ctx)
            .await
            .expect("command registered");
        match out {
            ExtensionCommandOutcome::Text(s) => assert_eq!(s, "got: x"),
            _ => panic!("expected text outcome"),
        }
    }

    // -----------------------------------------------------------------
    // install_all coverage
    // -----------------------------------------------------------------

    /// Extension with no scopes-relevant contribution: default
    /// `instantiate` yields a no-op `LegacyInstance`, so it registers
    /// nothing through the drain.
    struct MinimalCapExt(&'static str);
    impl Extension for MinimalCapExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
    }

    #[tokio::test]
    async fn install_all_minimal_extension_is_a_noop() {
        let mut hub = test_hub().await;
        hub.install_all(vec![Arc::new(MinimalCapExt("solo"))])
            .await
            .unwrap();
        // Registered, but nothing drained: no routine handler, no
        // tools, no commands, no hooks.
        assert!(hub.extension_names().contains(&"solo"));
        assert!(hub.installed_for("solo").is_none());
        assert!(hub.hooks_for("solo").is_empty());
        assert!(hub.tools_for("solo").is_empty());
        assert!(hub.commands_for("solo").is_empty());
    }

    #[tokio::test]
    async fn install_all_is_idempotent() {
        let mut hub = test_hub().await;
        let ext: Arc<dyn Extension> = Arc::new(MinimalCapExt("solo"));
        hub.install_all(vec![ext.clone()]).await.unwrap();
        hub.install_all(vec![ext]).await.unwrap();
        // Re-installing the same Global extension doesn't double its
        // instance.
        assert!(hub.global_instances.contains_key("solo"));
    }

    // -----------------------------------------------------------------
    // dispatch_routine coverage
    // -----------------------------------------------------------------

    /// Routine handler that records every payload it receives.
    struct Recorder {
        seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }
    impl handler::RoutineHandler for Recorder {
        fn on_fire<'a>(
            &'a self,
            payload: serde_json::Value,
        ) -> handler::HandlerFuture<'a, anyhow::Result<()>> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(payload);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn dispatch_routine_invokes_registered_handler() {
        let mut hub = test_hub().await;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        hub.install_all(vec![Arc::new(
            TestExt::new("heartbeat").routine_handler(Arc::new(Recorder { seen: seen.clone() })),
        )])
        .await
        .unwrap();

        hub.dispatch_routine(
            "heartbeat",
            &RoutineScope::Global,
            serde_json::json!({"task": "ping"}),
        )
        .await
        .unwrap();

        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], serde_json::json!({"task": "ping"}));
    }

    #[tokio::test]
    async fn dispatch_routine_unknown_extension_errors() {
        let hub = ExtensionHub::new();
        let err = hub
            .dispatch_routine("ghost", &RoutineScope::Global, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost"), "got: {err}");
    }

    #[tokio::test]
    async fn dispatch_routine_extension_without_handler_errors() {
        // A minimal extension publishes no routine handler.
        let mut hub = test_hub().await;
        hub.install_all(vec![Arc::new(MinimalCapExt("solo"))])
            .await
            .unwrap();
        let err = hub
            .dispatch_routine("solo", &RoutineScope::Global, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("routine"), "got: {err}");
    }

    #[tokio::test]
    async fn dispatch_routine_handler_error_propagates() {
        struct AlwaysFails;
        impl handler::RoutineHandler for AlwaysFails {
            fn on_fire<'a>(
                &'a self,
                _payload: serde_json::Value,
            ) -> handler::HandlerFuture<'a, anyhow::Result<()>> {
                Box::pin(async { Err(anyhow::anyhow!("simulated failure")) })
            }
        }
        let mut hub = test_hub().await;
        hub.install_all(vec![Arc::new(
            TestExt::new("broken").routine_handler(Arc::new(AlwaysFails)),
        )])
        .await
        .unwrap();
        let err = hub
            .dispatch_routine("broken", &RoutineScope::Global, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated"), "got: {err}");
    }

    /// Publishes a no-op Messenger through the instance `messenger()`
    /// endpoint. Used by `cap_resolver_walks_instance_published_caps`.
    fn instance_messenger_ext() -> Arc<dyn Extension> {
        struct NoopMessenger;
        impl caps::Messenger for NoopMessenger {
            fn send<'a>(
                &'a self,
                _target: String,
                _body: caps::MessageBody,
            ) -> caps::CapFuture<'a, ()> {
                Box::pin(async { Ok(()) })
            }
        }
        struct MessengerInstance {
            manifest: manifest::ExtensionManifest,
        }
        impl instance::ExtensionInstance for MessengerInstance {
            fn manifest(&self) -> &manifest::ExtensionManifest {
                &self.manifest
            }
            fn messenger(&self) -> Option<Arc<dyn caps::Messenger>> {
                Some(Arc::new(NoopMessenger))
            }
        }
        struct MessengerExt;
        impl Extension for MessengerExt {
            fn name(&self) -> &'static str {
                "inst-messenger"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                &[]
            }
            fn instantiate<'a>(
                &'a self,
                _scope_ctx: instance::ScopeCtx<'a>,
            ) -> instance::InstantiateFuture<'a> {
                let manifest = self.manifest();
                Box::pin(async move {
                    Ok(Arc::new(MessengerInstance { manifest })
                        as Arc<dyn instance::ExtensionInstance>)
                })
            }
        }
        Arc::new(MessengerExt)
    }

    #[tokio::test]
    async fn cap_resolver_walks_instance_published_caps() {
        let mut hub = test_hub().await;
        hub.install_all(vec![instance_messenger_ext()])
            .await
            .unwrap();

        let resolver = hub.cap_resolver_for_turn(None, None).await;
        use instance::CapResolver as _;
        assert!(
            resolver.messenger().is_some(),
            "HubCapResolver should expose an instance-published Messenger"
        );
    }

    #[tokio::test]
    async fn install_all_validates_manifests_before_instantiation() {
        // Manifest with empty name — should reject before any
        // instantiation runs.
        struct BadManifestExt;
        impl Extension for BadManifestExt {
            fn name(&self) -> &'static str {
                "named-in-trait-but-not-in-manifest"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                &[]
            }
            fn manifest(&self) -> manifest::ExtensionManifest {
                manifest::ExtensionManifest {
                    name: String::new(), // <-- triggers EmptyName
                    extension_ref: ExtensionRef::builtin("x"),
                    supported_hooks: Vec::new(),
                    required_capabilities: Vec::new(),
                    requested_capabilities: Vec::new(),
                    provides_capabilities: Vec::new(),
                }
            }
        }
        let mut hub = ExtensionHub::new();
        let err = hub
            .install_all(vec![Arc::new(BadManifestExt)])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("name"), "got: {err}");
    }
}
