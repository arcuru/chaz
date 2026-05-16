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

pub mod caps;
pub mod caps_inproc;
pub mod handler;
pub(crate) mod hook_bridge;
pub mod hooks;
pub mod manifest;
pub mod registry;

use crate::extension::caps_inproc::{InProcSessionRead, InProcSessionWrite, InProcSettings};
use crate::routine::RoutineScope;
use crate::runtime::RuntimeMessage;
use crate::session::{Session, SessionRegistry};
use crate::tool::Tool;
use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::{DocStore, Table};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
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
/// [`crate::routine::RoutineEngine`] introduced in steps 7–8 of the
/// cap refactor.
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

    /// Phase 1 of `install_all`: produce the cap impls this extension
    /// publishes for others to consume. Default: no providers.
    fn build_providers(&self) -> anyhow::Result<HashMap<caps::CapabilityKind, caps::CapProvider>> {
        Ok(HashMap::new())
    }

    /// Phase 2 of `install_all`: receive the fully-resolved consumer
    /// bundle and produce the per-hook + routine handlers. Default:
    /// no handlers (an extension that only provides caps, or nothing).
    fn install<'a>(
        &'a self,
        _caps: caps::ExtensionCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
    {
        Box::pin(async move { Ok(handler::InstalledExtension::empty()) })
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
struct RegisteredCommand {
    owner: &'static str,
    handler: Box<dyn ExtensionCommand>,
}

/// Internal wrapper that tags a registered tool with its owner.
struct RegisteredTool {
    owner: &'static str,
    tool: Arc<dyn Tool>,
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

    // ---- Cap refactor — step 5 -----------------------------------------
    /// Capability registry — host-only impls plus per-kind extension
    /// providers. Built up across `install_all`. Empty until `install_all`
    /// runs; extensions still using the legacy `register` path leave it
    /// empty for now.
    cap_registry: registry::CapRegistry,
    /// Operator-configured default-provider picks per extension-providable
    /// kind, captured from `Config::capability_defaults` at hub construction.
    capability_defaults: HashMap<caps::CapabilityKind, String>,
    /// Per-extension `InstalledExtension` returned from `install`.
    /// Populated by `install_all`; the legacy `register` path doesn't
    /// touch this map.
    installed: HashMap<String, handler::InstalledExtension>,
    /// Bump on every extension-name-keyed string the hub needs to
    /// pass to legacy methods that demand `&'static str`. Lookup-only
    /// during the migration window; can be dropped once legacy
    /// registration is gone.
    name_intern: HashSet<&'static str>,
    /// Session registry handle the hub uses to resolve session-scoped
    /// routine fires into a `(ConversationId, Database)` so it can
    /// build per-session caps (SessionRead/Write/Settings). `None` in
    /// tests that exercise the hub in isolation; production builds set
    /// it via [`Self::set_session_registry`] after both the hub and
    /// the registry are constructed.
    session_registry: Option<Arc<SessionRegistry>>,
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
            cap_registry: registry::CapRegistry::new(),
            capability_defaults: HashMap::new(),
            installed: HashMap::new(),
            name_intern: HashSet::new(),
            session_registry: None,
        }
    }

    /// Install the session registry the hub uses to resolve session-
    /// scoped routine fires into per-session caps. Called once at
    /// startup from chaz's main, after both [`Self::new`] and
    /// [`SessionRegistry::new`]. Idempotent — later calls overwrite.
    pub fn set_session_registry(&mut self, registry: Arc<SessionRegistry>) {
        self.session_registry = Some(registry);
    }

    /// Replace the hub's operator-default-provider map. Called once at
    /// startup from chaz's main, sourcing the map from
    /// `Config::capability_defaults`. The map is consulted from inside
    /// [`Self::install_all`] when applying defaults to the cap registry.
    pub fn set_capability_defaults(&mut self, defaults: HashMap<caps::CapabilityKind, String>) {
        self.capability_defaults = defaults;
    }

    /// Snapshot of `InstalledExtension` for a registered extension.
    /// Returns `None` for extensions still using the legacy `register`
    /// path (their slot in `installed` is unset).
    pub fn installed_for(&self, name: &str) -> Option<&handler::InstalledExtension> {
        self.installed.get(name)
    }

    /// Snapshot of the cap registry. Useful for `/extensions list -v`
    /// and similar surfaces that want to introspect provider routing.
    pub fn cap_registry(&self) -> &registry::CapRegistry {
        &self.cap_registry
    }

    /// Dispatch one routine fire to the named extension's routine
    /// handler (added in step 8 of the cap refactor; session-scoped
    /// caps wired in step 9).
    ///
    /// `scope` controls the caps bundle handed to the handler:
    /// * [`RoutineScope::Global`] — extension-providable caps' default
    ///   providers only; host-only slots stay `None`.
    /// * [`RoutineScope::Session(id)`] — same defaults, plus per-
    ///   session [`caps::SessionRead`] / [`caps::SessionWrite`] /
    ///   [`caps::Settings`] resolved through the hub's
    ///   [`SessionRegistry`]. The owner string on `SessionWrite` is
    ///   the dispatching extension's name so audit trails record who
    ///   wrote what.
    ///
    /// Returns `Ok(())` if dispatch succeeded (the handler returned
    /// `Ok`); `Err(...)` if the handler errored, the extension isn't
    /// installed, the installed extension didn't register a routine
    /// handler, or session resolution failed for a session-scoped
    /// fire. The engine's failure-handling pass uses the `Err` path
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
        let caps = self.build_routine_caps(extension, scope).await?;
        handler.on_fire(&caps, payload).await
    }

    /// Assemble the routine-fire caps bundle for engine dispatch.
    ///
    /// Always populates the extension-providable cap defaults
    /// (Messenger, Memory) from the registry. For
    /// [`RoutineScope::Session`] also resolves the target session
    /// through [`SessionRegistry`] and fills SessionRead, SessionWrite,
    /// and Settings with per-session in-process impls owned by
    /// `extension`. Global-scope fires leave the host-only slots
    /// `None`.
    async fn build_routine_caps(
        &self,
        extension: &str,
        scope: &RoutineScope,
    ) -> anyhow::Result<caps::ExtensionCaps> {
        let mut bundle = caps::ExtensionCaps::empty();
        if let Some(map) = self
            .cap_registry
            .by_kind
            .get(&caps::CapabilityKind::Messenger)
            && let Some(default_name) = map.default.as_deref()
            && let Some(caps::CapProvider::Messenger(m)) = map.providers.get(default_name)
        {
            bundle.messengers.default = Some(m.clone());
        }
        if let Some(map) = self.cap_registry.by_kind.get(&caps::CapabilityKind::Memory)
            && let Some(default_name) = map.default.as_deref()
            && let Some(caps::CapProvider::Memory(m)) = map.providers.get(default_name)
        {
            bundle.memory.default = Some(m.clone());
        }

        if let RoutineScope::Session(session_db_id) = scope {
            let registry = self.session_registry.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "session-scoped routine fire for extension '{extension}' \
                     requires a SessionRegistry; call ExtensionHub::set_session_registry \
                     during startup"
                )
            })?;
            let (conv_id, session_db) = registry.open_session(session_db_id).await?;
            let session = Session::new(conv_id, session_db.clone()).await;
            let session = Arc::new(Mutex::new(session));
            bundle.session_read = Some(Arc::new(InProcSessionRead::new(session.clone())));
            bundle.session_write = Some(Arc::new(InProcSessionWrite::new(session, extension)));
            bundle.settings = Some(Arc::new(InProcSettings::new(session_db, extension)));
        }

        Ok(bundle)
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

    /// Fire `before_agent_start` for every active handler. Each handler
    /// may append messages, which are flattened into a single vector
    /// preserving registration order.
    pub async fn fire_before_agent_start(&self, ctx: &HookContext) -> Vec<RuntimeMessage> {
        let mut out = Vec::new();
        for reg in &self.before_agent_start {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            out.extend(reg.hook.on_before_agent_start(ctx).await);
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
            match reg.hook.on_tool_call(ctx, tool_name, args).await {
                ToolCallDecision::Continue => {}
                ToolCallDecision::Block { reason } => return ToolCallDecision::Block { reason },
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
            acc = reg.hook.on_tool_result(ctx, tool_name, acc).await;
        }
        acc
    }

    pub async fn fire_agent_end(&self, ctx: &HookContext) {
        for reg in &self.agent_end {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            reg.hook.on_agent_end(ctx).await;
        }
    }

    pub async fn fire_session_start(&self, ctx: &HookContext) {
        for reg in &self.session_start {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            reg.hook.on_session_start(ctx).await;
        }
    }

    pub async fn fire_session_shutdown(&self, ctx: &HookContext) {
        for reg in &self.session_shutdown {
            if !ctx.active_extensions.contains(reg.owner) {
                continue;
            }
            reg.hook.on_session_shutdown(ctx).await;
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
    // Cap refactor — install_all (step 5)
    // -----------------------------------------------------------------

    /// Drive the two-phase cap-based install for `extensions`:
    ///
    /// 1. Collect manifests, run per-manifest validation.
    /// 2. Phase 1 — `build_providers()` on every extension; register
    ///    each impl in the cap registry.
    /// 3. Apply operator default-provider picks.
    /// 4. Phase 2 — for each extension, build its install-time
    ///    `ExtensionCaps` bundle, call `install(caps)`, store the
    ///    returned `InstalledExtension`. Tool / command registrations
    ///    buffered into pending queues during install are drained
    ///    through the existing owner-attribution helpers so the legacy
    ///    fire paths still see them.
    ///
    /// This runs alongside `register_extension`: extensions still using
    /// the legacy `register` path leave `installed[name]` empty and
    /// register hooks the old way. Step 6 migrates each built-in.
    ///
    /// Idempotent across calls: an extension already present is
    /// skipped (its first install wins). Tools / commands collected
    /// here flow through the legacy collision policy (first
    /// registration wins) at drain time.
    pub async fn install_all(&mut self, extensions: Vec<Arc<dyn Extension>>) -> anyhow::Result<()> {
        let manifests: Vec<manifest::ExtensionManifest> =
            extensions.iter().map(|e| e.manifest()).collect();
        for m in &manifests {
            m.validate()?;
        }

        // Phase 1 — every extension's providers register before any
        // consumer install runs.
        for (ext, m) in extensions.iter().zip(&manifests) {
            let providers = ext.build_providers()?;
            for (kind, provider) in providers {
                self.cap_registry
                    .register_provider(m.name.clone(), kind, provider)?;
            }
        }

        // Apply operator picks + auto-default any single-provider kinds.
        self.cap_registry
            .apply_operator_defaults(&self.capability_defaults)?;

        // Phase 2 — build a per-extension install bundle, call install,
        // capture the returned InstalledExtension.
        let tool_pending: Arc<Mutex<Vec<caps_inproc::PendingTool>>> =
            Arc::new(Mutex::new(Vec::new()));
        let command_pending: Arc<Mutex<Vec<caps_inproc::PendingCommand>>> =
            Arc::new(Mutex::new(Vec::new()));

        for (ext, m) in extensions.iter().zip(&manifests) {
            if self.installed.contains_key(&m.name) {
                // Already installed in a prior `install_all` call; skip
                // to keep the operation idempotent.
                continue;
            }
            let caps = self.build_install_caps(m, &tool_pending, &command_pending);
            let installed = ext.install(caps).await?;
            self.installed.insert(m.name.clone(), installed);
            self.extensions.push(ext.clone());
        }

        // Drain pending tool / command registrations through the legacy
        // owner-attributed registration. Same collision policy
        // (first-write-wins) and same reverse-index bookkeeping.
        let pending_tools = std::mem::take(&mut *tool_pending.lock().await);
        for p in pending_tools {
            self.register_tool_attributed(&p.owner, p.tool);
        }
        let pending_commands = std::mem::take(&mut *command_pending.lock().await);
        for p in pending_commands {
            self.register_command_attributed(&p.owner, p.descriptor.name, p.command);
        }

        // Bridge cap-based hook handlers (`installed[name].tool_call`,
        // `installed[name].tool_result`, ...) into the legacy hook
        // vectors so the existing `fire_*` paths run unchanged. The
        // adapter builds a per-fire `ExtensionCaps` bundle from the
        // legacy `HookContext` so cap-based handlers see the same
        // session view their cap traits promise.
        //
        // Take<Option<Box<dyn ...>>> moves the handler out of
        // `installed[name]` — the slot then reads as `None` to
        // `installed_for(name)`, which is fine: the legacy fire path
        // is now the source of truth for the handler.
        let names: Vec<String> = self.installed.keys().cloned().collect();
        for name in names {
            let Some(slot) = self.installed.get_mut(&name) else {
                continue;
            };
            let tool_call = slot.tool_call.take();
            let tool_result = slot.tool_result.take();
            let before_agent_start = slot.before_agent_start.take();
            let agent_end = slot.agent_end.take();
            let session_start = slot.session_start.take();
            let session_shutdown = slot.session_shutdown.take();
            let owner: &'static str = self.intern_name(&name);
            if let Some(inner) = tool_call {
                self.hooks_by_extension
                    .entry(owner)
                    .or_default()
                    .insert(HookKind::ToolCall);
                self.tool_call.push(RegisteredHook {
                    owner,
                    hook: Box::new(hook_bridge::ToolCallAdapter::new(owner, inner)),
                });
            }
            if let Some(inner) = tool_result {
                self.hooks_by_extension
                    .entry(owner)
                    .or_default()
                    .insert(HookKind::ToolResult);
                self.tool_result.push(RegisteredHook {
                    owner,
                    hook: Box::new(hook_bridge::ToolResultAdapter::new(owner, inner)),
                });
            }
            if let Some(inner) = before_agent_start {
                self.hooks_by_extension
                    .entry(owner)
                    .or_default()
                    .insert(HookKind::BeforeAgentStart);
                self.before_agent_start.push(RegisteredHook {
                    owner,
                    hook: Box::new(hook_bridge::BeforeAgentStartAdapter::new(owner, inner)),
                });
            }
            if let Some(inner) = agent_end {
                self.hooks_by_extension
                    .entry(owner)
                    .or_default()
                    .insert(HookKind::AgentEnd);
                self.agent_end.push(RegisteredHook {
                    owner,
                    hook: Box::new(hook_bridge::AgentEndAdapter::new(owner, inner)),
                });
            }
            if let Some(inner) = session_start {
                self.hooks_by_extension
                    .entry(owner)
                    .or_default()
                    .insert(HookKind::SessionStart);
                self.session_start.push(RegisteredHook {
                    owner,
                    hook: Box::new(hook_bridge::SessionStartAdapter::new(owner, inner)),
                });
            }
            if let Some(inner) = session_shutdown {
                self.hooks_by_extension
                    .entry(owner)
                    .or_default()
                    .insert(HookKind::SessionShutdown);
                self.session_shutdown.push(RegisteredHook {
                    owner,
                    hook: Box::new(hook_bridge::SessionShutdownAdapter::new(owner, inner)),
                });
            }
        }

        Ok(())
    }

    /// Assemble the install-time consumer bundle for `manifest`.
    ///
    /// Install-time scope: session-scoped slots stay `None`
    /// (session_read/write/settings — no session yet), tool and
    /// command registration are populated with buffered impls, and
    /// extension-providable caps resolve through the registry.
    fn build_install_caps(
        &self,
        m: &manifest::ExtensionManifest,
        tool_pending: &Arc<Mutex<Vec<caps_inproc::PendingTool>>>,
        command_pending: &Arc<Mutex<Vec<caps_inproc::PendingCommand>>>,
    ) -> caps::ExtensionCaps {
        let mut bundle = caps::ExtensionCaps::empty();

        // Walk required + requested; populate the slot if any matches.
        // Required-vs-requested distinction only matters for what the
        // extension does on missing caps (it gets `None` and decides);
        // bundle building treats them uniformly here, since step-5
        // hub-side enforcement of required-absence cascade lives with
        // step-6 migration of consumers.
        let requests = m
            .required_capabilities
            .iter()
            .chain(m.requested_capabilities.iter());
        for req in requests {
            use caps::CapabilityKind as K;
            match req.kind() {
                K::SessionRead | K::SessionWrite | K::Settings => {
                    // Session-scoped — populated at handler-fire time,
                    // not install time.
                }
                K::ToolRegistration => {
                    bundle.tool_registration =
                        Some(Arc::new(caps_inproc::InProcToolRegistration::new(
                            m.name.clone(),
                            tool_pending.clone(),
                        )));
                }
                K::CommandRegistration => {
                    bundle.command_registration =
                        Some(Arc::new(caps_inproc::InProcCommandRegistration::new(
                            m.name.clone(),
                            command_pending.clone(),
                        )));
                }
                K::Messenger => {
                    populate_capset(
                        &mut bundle.messengers,
                        &self.cap_registry,
                        K::Messenger,
                        req.provider(),
                        |p| match p {
                            caps::CapProvider::Messenger(m) => Some(m.clone()),
                            _ => None,
                        },
                    );
                }
                K::Memory => {
                    populate_capset(
                        &mut bundle.memory,
                        &self.cap_registry,
                        K::Memory,
                        req.provider(),
                        |p| match p {
                            caps::CapProvider::Memory(m) => Some(m.clone()),
                            _ => None,
                        },
                    );
                }
            }
        }
        bundle
    }

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
    fn register_command_attributed(
        &mut self,
        owner: &str,
        name: String,
        handler: Box<dyn ExtensionCommand>,
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

/// Resolve one extension-providable cap request into a `CapSet` slot.
///
/// `extractor` peels the right `Arc<dyn T>` out of `CapProvider`.
/// Mirrors the consumer-side resolution rules: bare requests fill the
/// `default` slot from the registry's resolved default; named
/// requests fill the corresponding `named` entry. Misses pass through
/// silently (consumer code checks `Option`).
fn populate_capset<T: ?Sized>(
    set: &mut caps::CapSet<T>,
    reg: &registry::CapRegistry,
    kind: caps::CapabilityKind,
    requested_provider: Option<&str>,
    extractor: impl Fn(&caps::CapProvider) -> Option<Arc<T>>,
) {
    let Some(map) = reg.by_kind.get(&kind) else {
        return;
    };
    match requested_provider {
        Some(name) => {
            if let Some(p) = map.providers.get(name)
                && let Some(arc) = extractor(p)
            {
                set.named.insert(name.into(), arc);
            }
        }
        None => {
            if let Some(default_name) = map.default.as_deref()
                && let Some(p) = map.providers.get(default_name)
                && let Some(arc) = extractor(p)
            {
                set.default = Some(arc);
            }
        }
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
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
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

    struct ToolCallExt(&'static str);
    impl Extension for ToolCallExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::ToolCall]
        }
        fn install<'a>(
            &'a self,
            _caps: caps::ExtensionCaps,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
        {
            Box::pin(async move {
                struct Pass;
                impl handler::HookHandlerToolCall for Pass {
                    fn on_tool_call<'a>(
                        &'a self,
                        _: &'a caps::ExtensionCaps,
                        _: &'a str,
                        _: &'a mut serde_json::Value,
                    ) -> handler::HandlerFuture<'a, ToolCallDecision> {
                        Box::pin(async { ToolCallDecision::Continue })
                    }
                }
                let mut installed = handler::InstalledExtension::empty();
                installed.tool_call = Some(Box::new(Pass));
                Ok(installed)
            })
        }
    }

    #[tokio::test]
    async fn hub_records_owner_for_each_hook_registration() {
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![
            Arc::new(ToolCallExt("alpha")),
            Arc::new(ToolCallExt("beta")),
        ])
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
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![
            Arc::new(NamedExt("noop")),
            Arc::new(ToolCallExt("alpha")),
            Arc::new(ToolCallExt("beta")),
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
        struct CmdExt;
        impl Extension for CmdExt {
            fn name(&self) -> &'static str {
                "with_command"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                &[HookKind::Command]
            }
            fn manifest(&self) -> manifest::ExtensionManifest {
                manifest::ExtensionManifest {
                    name: "with_command".to_string(),
                    extension_ref: ExtensionRef::builtin("with_command"),
                    supported_hooks: vec![HookKind::Command],
                    required_capabilities: Vec::new(),
                    requested_capabilities: vec![caps::CapabilityRequest::CommandRegistration],
                    provides_capabilities: Vec::new(),
                }
            }
            fn install<'a>(
                &'a self,
                caps: caps::ExtensionCaps,
            ) -> Pin<
                Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>,
            > {
                Box::pin(async move {
                    let reg = caps
                        .command_registration
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("command_registration cap not granted"))?;
                    reg.register(
                        caps::CommandDescriptor {
                            name: "dance".into(),
                            description: "test command".into(),
                        },
                        Box::new(DummyCmd),
                    )
                    .await?;
                    Ok(handler::InstalledExtension::empty())
                })
            }
        }
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![Arc::new(CmdExt)]).await.unwrap();
        assert!(hub.commands_for("with_command").contains("dance"));
        assert_eq!(hub.command_owner("dance"), Some("with_command"));
        assert_eq!(hub.command_owner("not_real"), None);
    }

    #[tokio::test]
    async fn hub_extension_refs_returns_one_per_extension_in_order() {
        let mut hub = ExtensionHub::new();
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
            active_extensions: HashSet::new(),
        }
    }

    struct CountingHook {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl handler::HookHandlerBeforeAgentStart for CountingHook {
        fn on_before_agent_start<'a>(
            &'a self,
            _caps: &'a caps::ExtensionCaps,
        ) -> handler::HandlerFuture<'a, Vec<RuntimeMessage>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![RuntimeMessage::System("injected".into())]
            })
        }
    }

    struct CountingExt {
        name_: &'static str,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Extension for CountingExt {
        fn name(&self) -> &'static str {
            self.name_
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::BeforeAgentStart]
        }
        fn install<'a>(
            &'a self,
            _caps: caps::ExtensionCaps,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
        {
            let calls = self.calls.clone();
            Box::pin(async move {
                let mut installed = handler::InstalledExtension::empty();
                installed.before_agent_start = Some(Box::new(CountingHook { calls }));
                Ok(installed)
            })
        }
    }

    #[tokio::test]
    async fn before_agent_start_runs_in_registration_order() {
        let mut hub = ExtensionHub::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        hub.install_all(vec![
            Arc::new(CountingExt {
                name_: "a",
                calls: calls.clone(),
            }),
            Arc::new(CountingExt {
                name_: "b",
                calls: calls.clone(),
            }),
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
        let mut hub = ExtensionHub::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        hub.install_all(vec![
            Arc::new(CountingExt {
                name_: "a",
                calls: calls.clone(),
            }),
            Arc::new(CountingExt {
                name_: "b",
                calls: calls.clone(),
            }),
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
            _caps: &'a caps::ExtensionCaps,
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
            _caps: &'a caps::ExtensionCaps,
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

    struct MutatingExt;
    impl Extension for MutatingExt {
        fn name(&self) -> &'static str {
            "mutating"
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::ToolCall]
        }
        fn install<'a>(
            &'a self,
            _caps: caps::ExtensionCaps,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
        {
            Box::pin(async move {
                let mut installed = handler::InstalledExtension::empty();
                installed.tool_call = Some(Box::new(MutatingHook));
                Ok(installed)
            })
        }
    }
    struct BlockingExt;
    impl Extension for BlockingExt {
        fn name(&self) -> &'static str {
            "blocking"
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::ToolCall]
        }
        fn install<'a>(
            &'a self,
            _caps: caps::ExtensionCaps,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
        {
            Box::pin(async move {
                let mut installed = handler::InstalledExtension::empty();
                installed.tool_call = Some(Box::new(BlockingHook));
                Ok(installed)
            })
        }
    }

    #[tokio::test]
    async fn tool_call_block_short_circuits_and_mutation_propagates() {
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![Arc::new(MutatingExt), Arc::new(BlockingExt)])
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

    struct DummyCmdExt(&'static str, &'static str);
    impl Extension for DummyCmdExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::Command]
        }
        fn manifest(&self) -> manifest::ExtensionManifest {
            manifest::ExtensionManifest {
                name: self.0.to_string(),
                extension_ref: ExtensionRef::builtin(self.0),
                supported_hooks: vec![HookKind::Command],
                required_capabilities: Vec::new(),
                requested_capabilities: vec![caps::CapabilityRequest::CommandRegistration],
                provides_capabilities: Vec::new(),
            }
        }
        fn install<'a>(
            &'a self,
            caps: caps::ExtensionCaps,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
        {
            let cmd_name = self.1.to_string();
            Box::pin(async move {
                let reg = caps
                    .command_registration
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("command_registration cap not granted"))?;
                reg.register(
                    caps::CommandDescriptor {
                        name: cmd_name,
                        description: "test command".into(),
                    },
                    Box::new(DummyCmd),
                )
                .await?;
                Ok(handler::InstalledExtension::empty())
            })
        }
    }

    #[tokio::test]
    async fn command_collision_with_builtin_is_rejected() {
        let mut hub = ExtensionHub::new();
        hub.reserve_builtin_commands(["info"]);
        hub.install_all(vec![Arc::new(DummyCmdExt("ext", "info"))])
            .await
            .unwrap();
        assert!(!hub.has_command("info"));
    }

    #[tokio::test]
    async fn duplicate_extension_command_keeps_first() {
        let mut hub = ExtensionHub::new();
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
        struct OtherCmdExt;
        impl Extension for OtherCmdExt {
            fn name(&self) -> &'static str {
                "second"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                // Declares Command even though the actual registration
                // will be rejected as a duplicate — keeps the declaration
                // honest about intent.
                &[HookKind::Command]
            }
            fn manifest(&self) -> manifest::ExtensionManifest {
                manifest::ExtensionManifest {
                    name: "second".to_string(),
                    extension_ref: ExtensionRef::builtin("second"),
                    supported_hooks: vec![HookKind::Command],
                    required_capabilities: Vec::new(),
                    requested_capabilities: vec![caps::CapabilityRequest::CommandRegistration],
                    provides_capabilities: Vec::new(),
                }
            }
            fn install<'a>(
                &'a self,
                caps: caps::ExtensionCaps,
            ) -> Pin<
                Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>,
            > {
                Box::pin(async move {
                    let reg = caps
                        .command_registration
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("command_registration cap not granted"))?;
                    reg.register(
                        caps::CommandDescriptor {
                            name: "greet".into(),
                            description: "other".into(),
                        },
                        Box::new(OtherCmd),
                    )
                    .await?;
                    Ok(handler::InstalledExtension::empty())
                })
            }
        }
        // Drain order = vec order: "first" registers "greet" before
        // "second" tries to — first-write-wins keeps "first".
        hub.install_all(vec![
            Arc::new(DummyCmdExt("first", "greet")),
            Arc::new(OtherCmdExt),
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
    // install_all coverage (cap refactor — step 5)
    // -----------------------------------------------------------------

    /// Extension that exists for the cap-install tests. Declares no
    /// hooks, no caps; install returns the default empty
    /// `InstalledExtension`.
    struct MinimalCapExt(&'static str);
    impl Extension for MinimalCapExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
    }

    /// Extension that declares it provides a `Messenger`. Returns a
    /// no-op impl from `build_providers`.
    struct ProvidingExt(&'static str);
    impl ProvidingExt {
        fn provider() -> caps::CapProvider {
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
            caps::CapProvider::Messenger(Arc::new(NoopMessenger))
        }
    }
    impl Extension for ProvidingExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
        fn manifest(&self) -> manifest::ExtensionManifest {
            manifest::ExtensionManifest {
                name: self.0.to_string(),
                extension_ref: ExtensionRef::builtin(self.0),
                supported_hooks: Vec::new(),
                required_capabilities: Vec::new(),
                requested_capabilities: Vec::new(),
                provides_capabilities: vec![caps::CapabilityKind::Messenger],
            }
        }
        fn build_providers(
            &self,
        ) -> anyhow::Result<HashMap<caps::CapabilityKind, caps::CapProvider>> {
            Ok([(caps::CapabilityKind::Messenger, Self::provider())]
                .into_iter()
                .collect())
        }
    }

    /// Extension that requires a `Messenger` (bare — default provider).
    /// `install` reaches into the bundle, asserts the messenger slot
    /// is filled, and returns an empty `InstalledExtension`.
    struct ConsumingExt(&'static str);
    impl Extension for ConsumingExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
        fn manifest(&self) -> manifest::ExtensionManifest {
            manifest::ExtensionManifest {
                name: self.0.to_string(),
                extension_ref: ExtensionRef::builtin(self.0),
                supported_hooks: Vec::new(),
                required_capabilities: vec![caps::CapabilityRequest::Messenger { provider: None }],
                requested_capabilities: Vec::new(),
                provides_capabilities: Vec::new(),
            }
        }
        fn install<'a>(
            &'a self,
            caps: caps::ExtensionCaps,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
        {
            Box::pin(async move {
                if caps.messengers.default.is_none() {
                    return Err(anyhow::anyhow!("expected default messenger"));
                }
                Ok(handler::InstalledExtension::empty())
            })
        }
    }

    #[tokio::test]
    async fn install_all_minimal_extension_is_a_noop() {
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![Arc::new(MinimalCapExt("solo"))])
            .await
            .unwrap();
        // No providers registered, no tools / commands drained.
        assert!(hub.installed_for("solo").is_some());
        assert!(hub.installed_for("solo").unwrap().is_empty());
        assert!(
            hub.cap_registry()
                .providers_for(caps::CapabilityKind::Messenger)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn install_all_wires_provider_into_consumer_via_auto_default() {
        // Single Messenger provider → auto-default → bare consumer
        // request resolves to it.
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![
            Arc::new(ProvidingExt("matrix")),
            Arc::new(ConsumingExt("notifier")),
        ])
        .await
        .unwrap();

        assert_eq!(
            hub.cap_registry()
                .default_provider_for(caps::CapabilityKind::Messenger),
            Some("matrix")
        );
        assert!(hub.installed_for("notifier").is_some());
        assert!(hub.installed_for("matrix").is_some());
    }

    #[tokio::test]
    async fn install_all_with_operator_default_overrides_auto() {
        // Two providers, no auto-default → operator must pick.
        let mut hub = ExtensionHub::new();
        hub.set_capability_defaults(
            [(caps::CapabilityKind::Messenger, "email".to_string())]
                .into_iter()
                .collect(),
        );
        hub.install_all(vec![
            Arc::new(ProvidingExt("matrix")),
            Arc::new(ProvidingExt("email")),
            Arc::new(ConsumingExt("notifier")),
        ])
        .await
        .unwrap();

        assert_eq!(
            hub.cap_registry()
                .default_provider_for(caps::CapabilityKind::Messenger),
            Some("email")
        );
    }

    #[tokio::test]
    async fn install_all_unknown_operator_default_errors() {
        let mut hub = ExtensionHub::new();
        hub.set_capability_defaults(
            [(caps::CapabilityKind::Messenger, "ghost".to_string())]
                .into_iter()
                .collect(),
        );
        let err = hub
            .install_all(vec![Arc::new(ProvidingExt("matrix"))])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("ghost"),
            "error should mention bad provider: {err}"
        );
    }

    #[tokio::test]
    async fn install_all_missing_required_provider_does_not_silently_pass() {
        // ConsumingExt requires a default Messenger; with no provider,
        // its `install` returns an error which install_all propagates.
        let mut hub = ExtensionHub::new();
        let err = hub
            .install_all(vec![Arc::new(ConsumingExt("notifier"))])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("messenger"), "got: {err}");
    }

    #[tokio::test]
    async fn install_all_is_idempotent() {
        let mut hub = ExtensionHub::new();
        let ext: Arc<dyn Extension> = Arc::new(MinimalCapExt("solo"));
        hub.install_all(vec![ext.clone()]).await.unwrap();
        hub.install_all(vec![ext]).await.unwrap();
        // Same `installed` slot; provider registry untouched.
        assert!(hub.installed_for("solo").is_some());
    }

    // -----------------------------------------------------------------
    // dispatch_routine coverage (cap refactor — step 8)
    // -----------------------------------------------------------------

    /// Extension whose `install` registers a routine handler that
    /// records every payload it receives.
    struct RoutineRecorderExt {
        name: &'static str,
        seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }

    impl Extension for RoutineRecorderExt {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
        fn install<'a>(
            &'a self,
            _caps: caps::ExtensionCaps,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
        {
            let seen = self.seen.clone();
            Box::pin(async move {
                struct Recorder {
                    seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
                }
                impl handler::RoutineHandler for Recorder {
                    fn on_fire<'a>(
                        &'a self,
                        _caps: &'a caps::ExtensionCaps,
                        payload: serde_json::Value,
                    ) -> handler::HandlerFuture<'a, anyhow::Result<()>> {
                        let seen = self.seen.clone();
                        Box::pin(async move {
                            seen.lock().unwrap().push(payload);
                            Ok(())
                        })
                    }
                }
                let mut installed = handler::InstalledExtension::empty();
                installed.routine_handler = Some(Box::new(Recorder { seen }));
                Ok(installed)
            })
        }
    }

    #[tokio::test]
    async fn dispatch_routine_invokes_registered_handler() {
        let mut hub = ExtensionHub::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        hub.install_all(vec![Arc::new(RoutineRecorderExt {
            name: "heartbeat",
            seen: seen.clone(),
        })])
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
        // Install an extension that doesn't override `install` — its
        // default returns an empty `InstalledExtension` with no
        // routine handler.
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![Arc::new(MinimalCapExt("solo"))])
            .await
            .unwrap();
        let err = hub
            .dispatch_routine("solo", &RoutineScope::Global, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("routine_handler"), "got: {err}");
    }

    #[tokio::test]
    async fn dispatch_routine_handler_error_propagates() {
        struct ErroringExt;
        impl Extension for ErroringExt {
            fn name(&self) -> &'static str {
                "broken"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                &[]
            }
            fn install<'a>(
                &'a self,
                _caps: caps::ExtensionCaps,
            ) -> Pin<
                Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>,
            > {
                Box::pin(async {
                    struct AlwaysFails;
                    impl handler::RoutineHandler for AlwaysFails {
                        fn on_fire<'a>(
                            &'a self,
                            _caps: &'a caps::ExtensionCaps,
                            _payload: serde_json::Value,
                        ) -> handler::HandlerFuture<'a, anyhow::Result<()>>
                        {
                            Box::pin(async { Err(anyhow::anyhow!("simulated failure")) })
                        }
                    }
                    let mut installed = handler::InstalledExtension::empty();
                    installed.routine_handler = Some(Box::new(AlwaysFails));
                    Ok(installed)
                })
            }
        }
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![Arc::new(ErroringExt)]).await.unwrap();
        let err = hub
            .dispatch_routine("broken", &RoutineScope::Global, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated"), "got: {err}");
    }

    /// Extension whose routine handler asserts the per-session caps
    /// (`session_read`, `session_write`, `settings`) are populated
    /// and then writes a directive via `caps.session_write`.
    struct SessionScopedRoutineExt;
    impl Extension for SessionScopedRoutineExt {
        fn name(&self) -> &'static str {
            "session-scoped"
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
        fn install<'a>(
            &'a self,
            _caps: caps::ExtensionCaps,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<handler::InstalledExtension>> + Send + 'a>>
        {
            Box::pin(async {
                struct H;
                impl handler::RoutineHandler for H {
                    fn on_fire<'a>(
                        &'a self,
                        caps: &'a caps::ExtensionCaps,
                        payload: serde_json::Value,
                    ) -> handler::HandlerFuture<'a, anyhow::Result<()>> {
                        Box::pin(async move {
                            let writer = caps
                                .session_write
                                .as_ref()
                                .ok_or_else(|| anyhow::anyhow!("session_write not populated"))?;
                            // Read + Settings should also be wired for
                            // session-scoped fires — they're the host
                            // contract for "this fire knows its session".
                            anyhow::ensure!(
                                caps.session_read.is_some(),
                                "session_read should be populated"
                            );
                            anyhow::ensure!(
                                caps.settings.is_some(),
                                "settings should be populated"
                            );
                            writer
                                .append(caps::SessionEntryDraft {
                                    kind: "directive".into(),
                                    data: payload,
                                })
                                .await?;
                            Ok(())
                        })
                    }
                }
                let mut installed = handler::InstalledExtension::empty();
                installed.routine_handler = Some(Box::new(H));
                Ok(installed)
            })
        }
    }

    #[tokio::test]
    async fn dispatch_routine_session_scope_populates_session_caps_and_writes() {
        use crate::agent::AgentRegistry;
        use crate::session::{EntryType, Session, SessionRegistry};

        // Build a minimal SessionRegistry with one session.
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(
            SessionRegistry::new(instance.clone(), user, agents)
                .await
                .unwrap(),
        );
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_db_id = session_db.root_id().to_string();

        // Hub knows the registry.
        let mut hub = ExtensionHub::new();
        hub.set_session_registry(registry.clone());
        hub.install_all(vec![Arc::new(SessionScopedRoutineExt)])
            .await
            .unwrap();

        hub.dispatch_routine(
            "session-scoped",
            &RoutineScope::Session(session_db_id.clone()),
            serde_json::json!({"task": "summarize"}),
        )
        .await
        .unwrap();

        // The handler wrote a `directive` entry through SessionWrite.
        // Re-open the session and confirm the entry landed.
        let (conv_id, db) = registry.open_session(&session_db_id).await.unwrap();
        let session = Session::new(conv_id, db).await;
        let entries = session.entries();
        assert_eq!(entries.len(), 1, "expected one entry, got {entries:?}");
        assert!(matches!(entries[0].entry_type, EntryType::Directive));
        assert_eq!(entries[0].sender, "session-scoped");
    }

    #[tokio::test]
    async fn dispatch_routine_session_scope_errors_without_registry() {
        let mut hub = ExtensionHub::new();
        hub.install_all(vec![Arc::new(SessionScopedRoutineExt)])
            .await
            .unwrap();
        let err = hub
            .dispatch_routine(
                "session-scoped",
                &RoutineScope::Session("anything".into()),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("SessionRegistry"), "got: {err}");
    }

    #[tokio::test]
    async fn install_all_validates_manifests_before_phase_1() {
        // Manifest with empty name — should reject before any
        // provider registration runs.
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
