//! Extension lifecycle types — scope, instantiation context, instance trait,
//! turn-time cap resolver.
//!
//! The previous extension model treats extensions as global compile-time
//! singletons whose providers live in a flat [`crate::extension::registry::CapRegistry`].
//! That works for caps that don't vary across sessions, but the
//! autonomous-memory work showed two cracks:
//!
//! 1. Per-session providers had to be faked with a per-turn rebuild
//!    (`build_session_providers`) that re-reads the session DB on every
//!    context assembly.
//! 2. Per-agent extension state has no home — agents that should travel
//!    with their tools (Ava's brain index, schedule-driven personas) have
//!    nowhere to keep instance state.
//!
//! The lifecycle here treats extensions as instantiable at a declared
//! [`Scope`]. The host fires lifecycle events (peer up / session opened /
//! agent loaded), each event constructs the instances for that scope, and
//! the per-turn dispatch composes the live set:
//!
//!   `instances_for_turn(agent, session) = global ∪ agent ∪ session`
//!
//! Today's compiled-in extensions stay opt-in to the new model — the
//! default `Scope::Global` plus a no-op [`ExtensionInstance`] keeps the
//! legacy install / cap-registry path running unchanged.
//!
//! The trait surface is deliberately the shape the WASM component model
//! will reach for later: a typed instance with typed endpoints, no
//! ambient global state. Each cap / hook becomes one optional endpoint
//! the host invokes at a known moment, with a [`TurnCtx`] that carries
//! the [`CapResolver`] so providers and consumers can find each other
//! at call time rather than at install time.
//!
//! Phase A landed only the types and the hub bookkeeping — `TurnCtx`
//! and `CapResolver` have no production consumers yet, hence the
//! `#[allow(dead_code)]` on them. Phase B (memory → PerSession) is
//! where they start being constructed and called.

#![allow(dead_code)]

use crate::extension::caps::{ContextTail, MemoryAccess, Messenger, PromptAugmentation};
use crate::extension::handler::{
    HookHandlerAgentEnd, HookHandlerBeforeAgentStart, HookHandlerSessionShutdown,
    HookHandlerSessionStart, HookHandlerToolCall, HookHandlerToolResult, RoutineHandler,
};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::ExtensionCommand;
use crate::tool::Tool;
use eidetica::Database;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

/// Where an extension lives and how long an instance survives.
///
/// | Scope         | Instances per peer       | Lifetime              |
/// |---------------|--------------------------|-----------------------|
/// | `Global`      | 1                        | peer up → peer down   |
/// | `PerAgent`    | 1 per loaded agent       | agent loaded → unloaded |
/// | `PerSession`  | 1 per opened session     | session opened → closed |
///
/// State lives in the instance for as long as the scope is alive. State
/// that needs to persist across the scope (e.g. memory entries, settings)
/// keeps living in eidetica DBs — the instance is a runtime view, not a
/// store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Global,
    PerAgent,
    PerSession,
}

/// Handles every instance can reach for at instantiate time. The set
/// every extension uses on startup today (registry, hosted indices,
/// embedder, secrets) — bundled here so a single `ScopeCtx` carries
/// everything regardless of scope.
///
/// This is the moral equivalent of pi's `ExtensionContext`: the
/// peer-level bag that an extension closes over on construction.
pub struct PeerHandles {
    pub registry: Arc<crate::session::SessionRegistry>,
    pub agent_index: crate::hosted_index::HostedIndex,
    pub memory_bank_index: crate::hosted_index::HostedIndex,
    pub skill_bank_index: crate::hosted_index::HostedIndex,
    pub embedder: Option<Arc<dyn crate::embedding::Embedder>>,
    pub secrets: Option<Arc<crate::security::SecretStore>>,
    /// Server cell — set by main.rs after `Server::new`. Empty until
    /// then. Instances that need a back-reference to the running server
    /// (today: the schedule and agent_schedule extensions, which spawn
    /// sessions / fire agent turns) close over the `Arc<OnceLock>` and
    /// dereference at fire time.
    pub server_cell: Arc<std::sync::OnceLock<Arc<crate::server::Server>>>,
    /// Operator-configured `agent_state` allowlist, keyed by extension
    /// name. Mirrors the field on the hub — instances that build a
    /// `ScopedAgentStateAdmin` apply this map themselves rather than
    /// going through hub-side cap resolution.
    pub agent_state_allowlist: HashMap<String, Vec<String>>,
}

/// Context handed to [`crate::extension::Extension::instantiate`]. The
/// variant matches the scope being instantiated and gives the
/// instance scope-specific borrows it needs (the agent name for
/// `PerAgent`, the session DB for `PerSession`).
///
/// All variants carry [`PeerHandles`] — even `PerSession` instances
/// often need the registry or hosted index, and copying handles per
/// variant adds noise without value.
pub enum ScopeCtx<'a> {
    Global {
        peer: &'a PeerHandles,
    },
    Agent {
        peer: &'a PeerHandles,
        agent_name: &'a str,
        agent_db: &'a Database,
    },
    Session {
        peer: &'a PeerHandles,
        session_db_id: &'a str,
        session_db: &'a Database,
    },
}

impl ScopeCtx<'_> {
    pub fn scope(&self) -> Scope {
        match self {
            Self::Global { .. } => Scope::Global,
            Self::Agent { .. } => Scope::PerAgent,
            Self::Session { .. } => Scope::PerSession,
        }
    }

    pub fn peer(&self) -> &PeerHandles {
        match self {
            Self::Global { peer } => peer,
            Self::Agent { peer, .. } => peer,
            Self::Session { peer, .. } => peer,
        }
    }
}

/// What an instantiated extension exposes back to the host. Each
/// optional endpoint corresponds to one cap or hook moment; an instance
/// overrides only the endpoints it actually provides.
///
/// This collapses today's parallel `CapRegistry` (providers) and
/// `HookHandler` (event handlers) registries into one shape. The host
/// composes the live instance set at turn time and invokes the relevant
/// endpoint on each one.
///
/// The defaults are deliberately empty so existing extensions can
/// declare `Scope::Global` and return a no-op instance from
/// `instantiate` — the legacy install path keeps wiring tools /
/// commands / caps through the old registries until each extension
/// chooses to migrate.
#[allow(unused_variables)]
pub trait ExtensionInstance: Send + Sync + 'static {
    fn manifest(&self) -> &ExtensionManifest;

    // ── Lifecycle ──────────────────────────────────────────────────

    /// Called before the host drops this instance. Async so flush /
    /// teardown can complete. Default: no-op.
    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    // ── Per-turn endpoints (formerly caps) ─────────────────────────

    fn prompt_augmentation(&self) -> Option<Arc<dyn PromptAugmentation>> {
        None
    }
    fn context_tail(&self) -> Option<Arc<dyn ContextTail>> {
        None
    }

    // ── Extension-to-extension service endpoints ───────────────────

    fn memory_access(&self) -> Option<Arc<dyn MemoryAccess>> {
        None
    }
    fn messenger(&self) -> Option<Arc<dyn Messenger>> {
        None
    }

    // ── Tools + commands ───────────────────────────────────────────
    //
    // The host drains these at instance construction time. For Global
    // instances that's the tail of `install_all` (so tools flow into
    // the runtime ToolRegistry and commands into the hub's command
    // map). Per-session/per-agent tool & command scopes will land
    // alongside the dispatch-time scoping work — today, only Global
    // instances may publish tools/commands.

    /// Tools this instance publishes. Drained once after instantiation.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    /// Slash commands as `(name, handler)`. Drained once after
    /// instantiation; collisions follow the hub's first-write-wins
    /// policy (built-in reservations win over extension registrations).
    fn commands(&self) -> Vec<(String, Arc<dyn ExtensionCommand>)> {
        Vec::new()
    }

    // ── Hook endpoints ─────────────────────────────────────────────
    //
    // Each endpoint mirrors a slot on the legacy
    // [`crate::extension::handler::InstalledExtension`] — same trait
    // shape, `Arc` instead of `Box` so the instance can keep
    // ownership and the host clones the handle into its fire path.
    //
    // For Global instances the hub drains the returned handles at
    // install_all and pushes them through the legacy fire path. Once
    // dispatch consults instances directly (task #28 / #30), this
    // drain becomes the source of truth.

    fn before_agent_start_hook(&self) -> Option<Arc<dyn HookHandlerBeforeAgentStart>> {
        None
    }
    fn tool_call_hook(&self) -> Option<Arc<dyn HookHandlerToolCall>> {
        None
    }
    fn tool_result_hook(&self) -> Option<Arc<dyn HookHandlerToolResult>> {
        None
    }
    fn agent_end_hook(&self) -> Option<Arc<dyn HookHandlerAgentEnd>> {
        None
    }
    fn session_start_hook(&self) -> Option<Arc<dyn HookHandlerSessionStart>> {
        None
    }
    fn session_shutdown_hook(&self) -> Option<Arc<dyn HookHandlerSessionShutdown>> {
        None
    }

    /// Routine engine dispatch endpoint. Returned handles flow into
    /// `installed[name].routine_handler`, where `ExtensionHub::
    /// dispatch_routine` looks them up. Only Global instances are
    /// drained for routine handlers — per-session/per-agent routine
    /// fires aren't supported yet.
    fn routine_handler(&self) -> Option<Arc<dyn RoutineHandler>> {
        None
    }

    // ── Extension-defined caps (TypeId-keyed) ──────────────────────

    /// Custom cap published under a `TypeId` key. Returns
    /// `Some(Arc<dyn Any + Send + Sync>)` that the caller downcasts to
    /// the concrete trait object. See [`CapResolver::get`].
    ///
    /// Use this for extension-defined services that don't fit the
    /// well-known cap set (e.g. an Ava-internal `BrainQuery` cap
    /// shared between brain-indexing and skill extensions).
    fn extension_cap(&self, type_id: std::any::TypeId) -> Option<Arc<dyn Any + Send + Sync>> {
        let _ = type_id;
        None
    }
}

/// What an endpoint sees when the host calls it. Carries the active
/// agent / session identifiers plus the resolver that lets the
/// endpoint look up other extensions' caps without holding references
/// across the install boundary.
pub struct TurnCtx<'a> {
    pub agent_name: &'a str,
    pub session_db_id: &'a str,
    pub caps: &'a dyn CapResolver,
}

/// Cap lookup service available to endpoints at call time. The host
/// composes the live instance set for the current (agent, session)
/// pair and answers each accessor by walking it in
/// `session > agent > global` precedence — per-session overrides
/// per-agent overrides global.
///
/// Named accessors cover the well-known caps. For extension-defined
/// caps, see [`Self::extension_cap_by_id`] — the dyn-compatible
/// escape hatch. A typed `get::<T>()` wrapper is deferred until we
/// have a real consumer that informs the right downcast shape
/// (handle wrapper vs `Arc<dyn TheirTrait>` vs other).
pub trait CapResolver: Send + Sync {
    fn memory(&self) -> Option<Arc<dyn MemoryAccess>>;
    fn messenger(&self) -> Option<Arc<dyn Messenger>>;
    fn context_tail(&self) -> Option<Arc<dyn ContextTail>>;
    fn prompt_augmentation(&self) -> Option<Arc<dyn PromptAugmentation>>;

    /// Walk live instances asking each for an extension-defined cap
    /// keyed by `TypeId`. The stored value's concrete type is a
    /// contract between provider and consumer — typically a
    /// `BrainQueryHandle(Arc<dyn BrainQuery>)` newtype that the
    /// consumer downcasts.
    fn extension_cap_by_id(&self, type_id: std::any::TypeId) -> Option<Arc<dyn Any + Send + Sync>>;
}

/// Boxed-future return type for [`crate::extension::Extension::instantiate`].
/// Aliased so the trait signature reads cleanly instead of leaking the
/// full `Pin<Box<dyn Future<...>>>` shape.
pub type InstantiateFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = anyhow::Result<Arc<dyn ExtensionInstance>>> + Send + 'a>,
>;

/// Marker instance used by the default `Extension::instantiate`. Used
/// by legacy extensions that haven't migrated to the new model — they
/// keep registering tools, commands, and caps via the original
/// install path, and this empty instance just satisfies the trait
/// surface so the lifecycle machinery has a uniform return type.
pub(crate) struct LegacyInstance {
    manifest: ExtensionManifest,
}

impl LegacyInstance {
    pub(crate) fn new(manifest: ExtensionManifest) -> Self {
        Self { manifest }
    }
}

impl ExtensionInstance for LegacyInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
}
