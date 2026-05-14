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
use crate::tool::Tool;
use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::Table;
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
/// Variants that exist today fire through `fire_<kind>` methods on
/// [`ExtensionHub`]. Reserved variants ([`HookKind::Cron`]) are accepted
/// for declaration but not yet fired by the framework.
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
    /// Reserved — scheduled-event hook. Extensions may declare it, but
    /// the framework does not yet have a firing path.
    Cron,
}

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

    /// Declare every hook kind this extension intends to register. Used
    /// at startup to validate that `register_*` calls inside [`register`]
    /// match the declaration, and at runtime for inspection / future
    /// sandboxing surfaces.
    ///
    /// Tools and commands count: an extension that registers any tool
    /// must include [`HookKind::Tool`]; any command requires
    /// [`HookKind::Command`].
    fn supported_hooks(&self) -> &[HookKind];

    /// Register hooks, tools, and commands. Called once at startup with
    /// the hub in "registering-as-this-extension" mode so every
    /// `register_*` call captures ownership.
    fn register(self: Arc<Self>, hub: &mut ExtensionHub);

    /// Hook ABI version. Bumped when the hook interface changes shape in
    /// a backwards-incompatible way. Orthogonal to [`extension_ref`] —
    /// `extension_ref` identifies *which* extension is loaded;
    /// `extension_api_version` identifies *which hook contract* it expects.
    fn extension_api_version(&self) -> u32 {
        1
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
    /// Set to `Some(name)` for the duration of one extension's
    /// `Extension::register` call so `on_<kind>` and `register_command`
    /// know who's currently registering. None outside that window —
    /// callers that hit `None` are calling from the wrong place and the
    /// hub panics rather than producing un-owned handlers.
    current_registering: Option<&'static str>,
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
            current_registering: None,
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

    /// Register one extension. Sets the hub into "registering as `name`"
    /// mode for the duration of the extension's `register` call so the
    /// `on_<kind>` calls inside can capture the owner, then validates
    /// that every registered kind was declared in `supported_hooks()`.
    pub fn register_extension(&mut self, ext: Arc<dyn Extension>) {
        let name = ext.name();
        assert!(
            self.current_registering.is_none(),
            "register_extension is not re-entrant; '{name}' tried to register \
             while another extension was mid-registration"
        );
        self.current_registering = Some(name);
        self.hooks_by_extension.entry(name).or_default();
        self.commands_by_extension.entry(name).or_default();
        self.tools_by_extension.entry(name).or_default();
        ext.clone().register(self);
        self.current_registering = None;

        let declared: HashSet<HookKind> = ext.supported_hooks().iter().copied().collect();
        let registered = self
            .hooks_by_extension
            .get(name)
            .cloned()
            .unwrap_or_default();
        let undeclared: Vec<HookKind> = registered.difference(&declared).copied().collect();
        assert!(
            undeclared.is_empty(),
            "Extension '{name}' registered hook kinds {undeclared:?} that were \
             not in supported_hooks() {declared:?} — declare every kind your \
             register() call uses"
        );

        self.extensions.push(ext);
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

    /// Iterate every registered tool as `(name, Arc<dyn Tool>)`. Used by
    /// `main.rs` to populate the legacy [`ToolRegistry`] from the hub's
    /// hook-registered tools.
    pub fn tools_for_registry(&self) -> Vec<(String, Arc<dyn Tool>)> {
        self.tools
            .iter()
            .map(|(name, reg)| (name.clone(), reg.tool.clone()))
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

    fn current_owner(&self, method: &str) -> &'static str {
        self.current_registering.unwrap_or_else(|| {
            panic!(
                "{method} called outside Extension::register(); \
                 every hook registration must happen inside register_extension's window"
            )
        })
    }

    fn note_hook(&mut self, owner: &'static str, kind: HookKind) {
        self.hooks_by_extension
            .entry(owner)
            .or_default()
            .insert(kind);
    }

    pub fn on_before_agent_start(&mut self, hook: Box<dyn HookBeforeAgentStart>) {
        let owner = self.current_owner("on_before_agent_start");
        self.note_hook(owner, HookKind::BeforeAgentStart);
        self.before_agent_start.push(RegisteredHook { owner, hook });
    }

    pub fn on_tool_call(&mut self, hook: Box<dyn HookToolCall>) {
        let owner = self.current_owner("on_tool_call");
        self.note_hook(owner, HookKind::ToolCall);
        self.tool_call.push(RegisteredHook { owner, hook });
    }

    pub fn on_tool_result(&mut self, hook: Box<dyn HookToolResult>) {
        let owner = self.current_owner("on_tool_result");
        self.note_hook(owner, HookKind::ToolResult);
        self.tool_result.push(RegisteredHook { owner, hook });
    }

    pub fn on_agent_end(&mut self, hook: Box<dyn HookAgentEnd>) {
        let owner = self.current_owner("on_agent_end");
        self.note_hook(owner, HookKind::AgentEnd);
        self.agent_end.push(RegisteredHook { owner, hook });
    }

    pub fn on_session_start(&mut self, hook: Box<dyn HookSessionStart>) {
        let owner = self.current_owner("on_session_start");
        self.note_hook(owner, HookKind::SessionStart);
        self.session_start.push(RegisteredHook { owner, hook });
    }

    pub fn on_session_shutdown(&mut self, hook: Box<dyn HookSessionShutdown>) {
        let owner = self.current_owner("on_session_shutdown");
        self.note_hook(owner, HookKind::SessionShutdown);
        self.session_shutdown.push(RegisteredHook { owner, hook });
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
        let owner = self.current_owner("register_command");
        let name = name.into();
        if self.reserved_command_names.contains(&name) {
            warn!(
                command = %name,
                extension = %owner,
                "Extension command shadows a built-in; ignoring registration"
            );
            return;
        }
        if self.commands.contains_key(&name) {
            warn!(
                command = %name,
                extension = %owner,
                "Duplicate extension command registration; keeping first registration"
            );
            return;
        }
        self.note_hook(owner, HookKind::Command);
        self.commands_by_extension
            .entry(owner)
            .or_default()
            .insert(name.clone());
        self.commands
            .insert(name, RegisteredCommand { owner, handler });
    }

    /// Register a tool provided by the currently-registering extension.
    /// The tool is indexed by its descriptor name; collisions log a
    /// warning and keep the first registration (mirrors the command
    /// collision policy).
    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        let owner = self.current_owner("register_tool");
        let name = tool.descriptor().name;
        if self.tools.contains_key(&name) {
            warn!(
                tool = %name,
                extension = %owner,
                "Duplicate tool registration; keeping first registration"
            );
            return;
        }
        self.note_hook(owner, HookKind::Tool);
        self.tools_by_extension
            .entry(owner)
            .or_default()
            .insert(name.clone());
        self.tools.insert(name, RegisteredTool { owner, tool });
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

    /// Fire `before_agent_start` for every registered handler. Each
    /// handler may append messages, which are flattened into a single
    /// vector preserving registration order.
    pub async fn fire_before_agent_start(&self, ctx: &HookContext) -> Vec<RuntimeMessage> {
        let mut out = Vec::new();
        for reg in &self.before_agent_start {
            out.extend(reg.hook.on_before_agent_start(ctx).await);
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
        for reg in &self.tool_call {
            match reg.hook.on_tool_call(ctx, tool_name, args).await {
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
        for reg in &self.tool_result {
            acc = reg.hook.on_tool_result(ctx, tool_name, acc).await;
        }
        acc
    }

    pub async fn fire_agent_end(&self, ctx: &HookContext) {
        for reg in &self.agent_end {
            reg.hook.on_agent_end(ctx).await;
        }
    }

    pub async fn fire_session_start(&self, ctx: &HookContext) {
        for reg in &self.session_start {
            reg.hook.on_session_start(ctx).await;
        }
    }

    pub async fn fire_session_shutdown(&self, ctx: &HookContext) {
        for reg in &self.session_shutdown {
            reg.hook.on_session_shutdown(ctx).await;
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
        let reg = self.commands.get(name)?;
        Some(reg.handler.invoke(args, ctx).await)
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
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
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

    struct ToolCallExt(&'static str);
    impl Extension for ToolCallExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::ToolCall]
        }
        fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
            struct Pass;
            impl HookToolCall for Pass {
                fn on_tool_call<'a>(
                    &'a self,
                    _: &'a HookContext,
                    _: &'a str,
                    _: &'a mut serde_json::Value,
                ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>> {
                    Box::pin(async { ToolCallDecision::Continue })
                }
            }
            hub.on_tool_call(Box::new(Pass));
        }
    }

    #[test]
    fn hub_records_owner_for_each_hook_registration() {
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(ToolCallExt("alpha")));
        hub.register_extension(Arc::new(ToolCallExt("beta")));

        let alpha_kinds = hub.hooks_for("alpha");
        assert!(alpha_kinds.contains(&HookKind::ToolCall));
        let beta_kinds = hub.hooks_for("beta");
        assert!(beta_kinds.contains(&HookKind::ToolCall));
        // Other kinds untouched.
        assert!(!alpha_kinds.contains(&HookKind::ToolResult));
    }

    #[test]
    fn extensions_for_kind_returns_only_handlers_in_registration_order() {
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(NamedExt("noop")));
        hub.register_extension(Arc::new(ToolCallExt("alpha")));
        hub.register_extension(Arc::new(ToolCallExt("beta")));
        let owners = hub.extensions_for_kind(HookKind::ToolCall);
        assert_eq!(owners, vec!["alpha", "beta"]);
        let none = hub.extensions_for_kind(HookKind::AgentEnd);
        assert!(none.is_empty());
    }

    #[test]
    #[should_panic(expected = "registered hook kinds")]
    fn undeclared_hook_registration_panics() {
        struct Sneaky;
        impl Extension for Sneaky {
            fn name(&self) -> &'static str {
                "sneaky"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                // Declares nothing, but tries to register a hook anyway.
                &[]
            }
            fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
                struct Pass;
                impl HookToolCall for Pass {
                    fn on_tool_call<'a>(
                        &'a self,
                        _: &'a HookContext,
                        _: &'a str,
                        _: &'a mut serde_json::Value,
                    ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>>
                    {
                        Box::pin(async { ToolCallDecision::Continue })
                    }
                }
                hub.on_tool_call(Box::new(Pass));
            }
        }
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(Sneaky));
    }

    #[test]
    fn commands_track_owner_and_are_queryable() {
        struct CmdExt;
        impl Extension for CmdExt {
            fn name(&self) -> &'static str {
                "with_command"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                &[HookKind::Command]
            }
            fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
                hub.register_command("dance", Box::new(DummyCmd));
            }
        }
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(CmdExt));
        assert!(hub.commands_for("with_command").contains("dance"));
        assert_eq!(hub.command_owner("dance"), Some("with_command"));
        assert_eq!(hub.command_owner("not_real"), None);
    }

    #[test]
    fn cron_kind_is_declarable_even_though_not_yet_fired() {
        struct Scheduled;
        impl Extension for Scheduled {
            fn name(&self) -> &'static str {
                "scheduled"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                &[HookKind::Cron]
            }
            fn register(self: Arc<Self>, _hub: &mut ExtensionHub) {
                // No on_cron yet — declaration is forward-compatible.
            }
        }
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(Scheduled));
        // Registered no actual handler — `extensions_for_kind` reflects
        // registrations, not declarations, so this stays empty.
        assert!(hub.extensions_for_kind(HookKind::Cron).is_empty());
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
        fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
            hub.on_before_agent_start(Box::new(CountingHook {
                calls: self.calls.clone(),
            }));
        }
    }

    #[tokio::test]
    async fn before_agent_start_runs_in_registration_order() {
        let mut hub = ExtensionHub::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        hub.register_extension(Arc::new(CountingExt {
            name_: "a",
            calls: calls.clone(),
        }));
        hub.register_extension(Arc::new(CountingExt {
            name_: "b",
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

    struct MutatingExt;
    impl Extension for MutatingExt {
        fn name(&self) -> &'static str {
            "mutating"
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::ToolCall]
        }
        fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
            hub.on_tool_call(Box::new(MutatingHook));
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
        fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
            hub.on_tool_call(Box::new(BlockingHook));
        }
    }

    #[tokio::test]
    async fn tool_call_block_short_circuits_and_mutation_propagates() {
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(MutatingExt));
        hub.register_extension(Arc::new(BlockingExt));
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

    struct DummyCmdExt(&'static str, &'static str);
    impl Extension for DummyCmdExt {
        fn name(&self) -> &'static str {
            self.0
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::Command]
        }
        fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
            hub.register_command(self.1, Box::new(DummyCmd));
        }
    }

    #[tokio::test]
    async fn command_collision_with_builtin_is_rejected() {
        let mut hub = ExtensionHub::new();
        hub.reserve_builtin_commands(["info"]);
        hub.register_extension(Arc::new(DummyCmdExt("ext", "info")));
        assert!(!hub.has_command("info"));
    }

    #[tokio::test]
    async fn duplicate_extension_command_keeps_first() {
        let mut hub = ExtensionHub::new();
        hub.register_extension(Arc::new(DummyCmdExt("first", "greet")));
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
            fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
                hub.register_command("greet", Box::new(OtherCmd));
            }
        }
        hub.register_extension(Arc::new(OtherCmdExt));
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
