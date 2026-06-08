//! Callback-driven agent server.
//!
//! The server watches session databases for new entries via eidetica's
//! `on_write` callbacks. When a new message from a non-agent sender
//! is detected, it spawns an agent task that runs the ReAct loop and writes
//! the response back to the session database.
//!
//! The server is transport-agnostic — it only cares about session DBs and
//! agent execution. Gateways (Matrix, TUI) register their own callbacks on
//! session DBs to detect agent responses and deliver them to their transports.
//!
//! Per-session serialization prevents duplicate agent runs: a `processing`
//! set tracks which sessions have an active agent task. Concurrent writes
//! to the same session are skipped while an agent is running.

use crate::agent::AgentRegistry;
use crate::backends::{BackendManager, ModelInfo};
use crate::config::ContextConfig;
use crate::context::ContextBuilder;
use crate::extension::{ExtensionHub, HookContext};
use crate::gateway::ApprovalExchange;
use crate::hosted_index::HostedIndex;
use crate::runtime;
use crate::security::SecurityContext;
use crate::session::{EntryType, Session, SessionEntry, SessionRegistry};
use crate::tool::{ScopedTools, ToolContext, ToolPolicyRegistry, ToolProfile, ToolRegistry};
use crate::tool_host::ToolHost;
use crate::types::ConversationId;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tracing::{debug, error, info};

mod schedule;

/// Maximum number of concurrent LLM calls across all conversations.
const MAX_CONCURRENT_LLM_CALLS: usize = 10;

/// Consecutive home-peer skip count that triggers the operator WARN with
/// the recovery command. Keeps short outages quiet but surfaces sessions
/// that have been silent for a sustained period.
const HOME_SKIP_WARN_THRESHOLD: u32 = 3;

/// Pure decider for the per-session home-peer gate. Given a session's
/// `AgentRef` list, the target agent's DB id, and this peer's pubkey on
/// that agent DB, decide whether this peer should run the agent.
///
/// Returns true (this peer runs) when:
/// - No `AgentRef` matches `agent_db_id` (defensive — the agent isn't
///   on the session, no claim to make; let the caller decide).
/// - The matching `AgentRef.home_pubkey` is `None` (legacy: any
///   keyholder runs).
/// - The matching `home_pubkey` fails to parse as a `PublicKey`
///   (defensive: a corrupt value falls back to legacy behavior rather
///   than silencing the agent).
/// - The parsed `home_pubkey` equals `my_pubkey_on_agent`.
///
/// Split out as a free function so it can be tested without a `Server`.
pub(crate) fn is_home_for_agent_ref(
    agents: &[crate::session::AgentRef],
    agent_db_id: &str,
    my_pubkey_on_agent: &eidetica::auth::crypto::PublicKey,
) -> bool {
    let Some(agent_ref) = agents.iter().find(|a| a.db_id == agent_db_id) else {
        return true;
    };
    let Some(home_str) = agent_ref.home_pubkey.as_deref() else {
        return true;
    };
    match eidetica::auth::crypto::PublicKey::from_prefixed_string(home_str) {
        Ok(home_pk) => &home_pk == my_pubkey_on_agent,
        Err(_) => true,
    }
}

/// Per-session runtime state needed for agent processing.
/// Keyed by `session_db_id` in `Server::sessions`.
struct SessionRuntime {
    backend: BackendManager,
    agent_override: Option<String>,
    approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
    /// Spawn nesting depth (0 for gateway-originated sessions)
    call_depth: usize,
    /// Maximum spawn depth for this session's agent
    max_call_depth: usize,
    /// Parent's tool scope for transitive narrowing (None = use agent defaults)
    parent_tools: Option<ScopedTools>,
    /// Shared ReAct iteration budget inherited from the spawning parent
    /// (set by `register_child_session`). `None` on gateway-originated
    /// sessions — the task builder allocates a fresh budget then.
    iteration_budget: Option<Arc<AtomicU32>>,
    /// Signaled when the agent task completes (for synchronous spawn_agent)
    completion_tx: Option<mpsc::Sender<()>>,
}

/// Context for spawned agent tasks (call depth, tool scope, completion signal).
struct SpawnContext {
    call_depth: usize,
    max_call_depth: usize,
    parent_tools: Option<ScopedTools>,
    /// Budget inherited from the parent spawn site, or `None` for a
    /// fresh top-level run (the task builder allocates one).
    iteration_budget: Option<Arc<AtomicU32>>,
    completion_tx: Option<mpsc::Sender<()>>,
    /// Per-session processing lock — cleared when the task completes
    processing: Arc<Mutex<std::collections::HashSet<String>>>,
}

/// Built-in default for the agent→agent burst budget — the run of
/// consecutive agent-authored messages since the last human message or
/// `Directive`. Once the trailing burst reaches this, mention-chained
/// agent wakes are suppressed until a human (or a schedule) speaks
/// again. This is the runaway backstop for the chat-room model (see
/// `docs/src/design/autonomous_agents.md`). Operators override it via
/// `multi_agent.burst_budget` in config.
const DEFAULT_AGENT_BURST_BUDGET: usize = 6;

/// Resolve the per-turn `max_context_tokens` for the context builder.
///
/// A model's real context window is a hard ceiling: the runtime would
/// otherwise pack up to the static `max_context_tokens` regardless of the
/// model in use, overflowing small-window models and needlessly truncating
/// large ones. So when the window is known it bounds the budget — an explicit
/// agent/config cap may lower it further but never raise it above the window.
/// When the window is unknown, return the agent cap unchanged (`None` lets the
/// builder fall back to its configured default), preserving prior behavior.
fn resolve_context_max_tokens(
    backend: &BackendManager,
    model: Option<&str>,
    agent_cap: Option<usize>,
) -> Option<usize> {
    clamp_budget_to_window(agent_cap, model.and_then(|m| backend.context_window(m)))
}

/// Pure budget decision (split out from [`resolve_context_max_tokens`] for
/// testing).
///
/// When the model's window is known it is the budget ceiling: a model-blind
/// static `max_context_tokens` must not cap it (that's the very bug being
/// fixed — a 1M-window model would otherwise truncate at the 128k default).
/// An explicit per-agent cap may still lower the budget below the window for
/// cost control, but never raise it above. When the window is unknown, pass
/// the agent cap through untouched so the builder falls back to its
/// configured default — preserving today's model-blind behavior.
fn clamp_budget_to_window(agent_cap: Option<usize>, window: Option<usize>) -> Option<usize> {
    match window {
        Some(w) => Some(agent_cap.map_or(w, |cap| cap.min(w))),
        None => agent_cap,
    }
}

/// The model id used for context-window budgeting and the learn-on-first-use
/// window fetch: the session/agent pin when present, else the backend's
/// resolved default. The fallback mirrors the model the actual LLM call lands
/// on — `chat_with_tools_for_model` resolves an absent model to the backend
/// default — so budgeting and the call always agree on which model's window to
/// charge against. Without it, an agent that pins no model (relying on the
/// backend default, e.g. a default-routed agent like Ava) budgets against the
/// static `max_context_tokens` and never triggers the fetch that would learn
/// its real window, so a 1M-window model silently truncates at the 128k
/// default.
///
/// Unlike [`BackendManager::resolve_model_name`] this preserves any `backend:`
/// prefix, because the window overlay and YAML catalog are keyed by the
/// prefixed id; [`BackendManager::default_model`] already returns the prefixed
/// form in multi-backend setups.
fn budget_model_id(
    backend: &BackendManager,
    session_model: Option<&str>,
    agent_default: Option<&str>,
) -> Option<String> {
    session_model
        .or(agent_default)
        .map(str::to_string)
        .or_else(|| backend.default_model())
}

/// Stable hash of an agent's intended (yaml-derived) DB config, the gate that
/// keeps reconcile from clobbering live `/agent set` edits when the yaml block
/// and prompt files are unchanged. Routed through `serde_json::Value` so the
/// `HashMap` fields (`presets`, `grants`) serialize with sorted keys —
/// `preserve_order` is off — making the hash deterministic across processes.
/// `applied_config_hash` must be cleared on `cfg` before calling, so the hash
/// doesn't depend on its own prior value.
fn config_gate_hash(cfg: &crate::agent_db::AgentDbConfig) -> anyhow::Result<String> {
    let canonical = serde_json::to_string(&serde_json::to_value(cfg)?)?;
    Ok(blake3::hash(canonical.as_bytes()).to_hex().to_string())
}

/// Outcome of [`Server::reload_config_for`]. `considered` counts the yaml agent
/// entries that passed the optional name filter — `considered == 0` with a name
/// filter means "no such agent in yaml", distinct from "matched but unchanged".
#[derive(Debug, Default, Clone)]
pub struct ReloadReport {
    /// Names of agents whose DB config was rewritten.
    pub changed: Vec<String>,
    /// Number of yaml agent entries considered after the name filter.
    pub considered: usize,
}

/// Background half of "learn a model's context window on first use", shared by
/// the schedule and worker turn paths (the latter runs inside a spawned task
/// with no `&self`). No-op when `model` is empty, already has a known window,
/// or a fetch for it is already in flight. Otherwise spawns one fetch of the
/// backend's live catalog, slots the window into the live overlay (so the next
/// turn budgets correctly without a restart), and persists the model to the
/// in-use [`ModelInfoStore`](crate::model_info_store::ModelInfoStore).
fn spawn_model_window_fetch(
    chaz_peer: eidetica::Database,
    inflight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    backend: BackendManager,
    model: String,
) {
    if model.is_empty() || backend.context_window(&model).is_some() {
        return;
    }
    {
        let mut guard = inflight.lock().unwrap();
        if !guard.insert(model.clone()) {
            return; // already being fetched
        }
    }
    let store = crate::model_info_store::ModelInfoStore::new(chaz_peer);
    tokio::spawn(async move {
        match backend.fetch_models_with_info().await {
            Ok(models) => {
                if let Some(info) = models.into_iter().find(|m| m.id == model) {
                    if let Some(w) = info.context_window {
                        backend.insert_model_window(info.id.clone(), w);
                    }
                    if let Err(e) = store.put(&info).await {
                        tracing::warn!(model = %model, "model info store write failed: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::debug!(model = %model, "background model info fetch failed: {e}");
            }
        }
        inflight.lock().unwrap().remove(&model);
    });
}

/// Callback-driven agent server.
pub struct Server {
    registry: Arc<SessionRegistry>,
    agents: Arc<AgentRegistry>,
    agent_index: HostedIndex,
    memory_bank_index: HostedIndex,
    /// Hosted index of skill banks. Wired through but no consumers yet;
    /// the upcoming `/skills` slash surface and the skills-extension
    /// PerSession migration are what start reading it.
    #[allow(dead_code)]
    skill_bank_index: HostedIndex,
    tools: Arc<ToolRegistry>,
    policies: Arc<ToolPolicyRegistry>,
    security: SecurityContext,
    semaphore: Arc<Semaphore>,
    tool_profiles: HashMap<String, ToolProfile>,
    context_config: ContextConfig,
    /// Execution host for sandboxed capability requests (Native, future WASM, bwrap)
    host: Arc<dyn ToolHost>,
    /// Compile-time extension hub: hook handlers, extension commands.
    extensions: Arc<ExtensionHub>,
    /// Default backend used for schedule-fired Fresh sessions and fallback
    /// when a Pinned session has no registered SessionRuntime.
    default_backend: BackendManager,
    /// Model ids with a background "learn this model's context window" fetch
    /// in flight, so concurrent turns on a not-yet-seen model trigger exactly
    /// one fetch. See [`Server::ensure_model_window_cached`].
    model_fetch_inflight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Cache of resolved system prompts keyed by their serialized
    /// `system_prompt_ref` snapshot. Snapshots are immutable content addresses,
    /// so a cached entry never goes stale; this spares a `chaz_peer` read on
    /// every turn's `hydrate_agent_from_db`. See [`Server::fetch_resolved_prompt`].
    prompt_cache: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Per-session runtime state keyed by session_db_id
    sessions: Arc<Mutex<HashMap<String, SessionRuntime>>>,
    /// Track which session DBs have server callbacks registered
    watched: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Sessions currently being processed (prevents concurrent agent runs per session)
    processing: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Home-peer gate skip counter keyed by `(session_db_id, agent_name)`.
    /// In-memory, peer-local. Incremented on every wake that the gate
    /// suppresses; cleared when a turn actually runs (home == self) or on
    /// `/agent rehost`. A WARN with the recovery command is emitted when
    /// the count crosses [`HOME_SKIP_WARN_THRESHOLD`] so silent sessions
    /// surface in logs instead of staying invisible.
    skip_counters: Arc<Mutex<HashMap<(String, String), u32>>>,
    /// Per-session active-extension set, folded from each session's
    /// `extensions` event log and cached in memory. Refreshed at
    /// `register_session` and on `/extensions add|remove`.
    active_extensions: Arc<Mutex<HashMap<String, std::collections::HashSet<String>>>>,
    /// Internal notification channel — callbacks send session_db_id here
    notify_tx: mpsc::Sender<String>,
    /// Agent→agent burst budget. Defaults to
    /// [`DEFAULT_AGENT_BURST_BUDGET`]; operators override it via
    /// `multi_agent.burst_budget` (applied once at startup before the
    /// gateway begins delivering messages).
    agent_burst_budget: AtomicUsize,
    /// Names of agents auto-attached to every freshly-created session, in
    /// order. Set once at startup from `Config.default_agents` via
    /// `set_default_agents`. Empty falls back to single-default attach
    /// (whatever `AgentRegistry::default_agent()` returns). The first
    /// entry effectively becomes the routing host on new sessions.
    default_agents: std::sync::RwLock<Vec<String>>,
    /// Path to the on-disk chaz yaml, set once at startup via
    /// [`Server::set_config_path`]. Lets `/agent reload` and the TUI `[r]`
    /// action re-read the config and re-run the agent reconcile without
    /// threading the path through every call site. `None` in `--print` and
    /// tests, where reload is unavailable.
    config_path: std::sync::RwLock<Option<std::path::PathBuf>>,
    /// Running `RoutineEngine`, set once at startup via
    /// `set_routine_engine` (skipped under `--print`). Threaded into the
    /// `HookContext` / `ToolContext` built for each session so that
    /// scheduling extensions and tools resync the live heap after a
    /// committed mutation. `None` in `--print` mode and in tests.
    routine_engine: OnceLock<Arc<crate::routine::RoutineEngine>>,
    /// Per-process MCP server directory. Populated by
    /// [`crate::extensions::mcp::McpExtension`] during install and
    /// exposed via [`Server::mcp_registry`] for read-only callers
    /// (today: the TUI Peer→MCP settings page). Shared `Arc` — the
    /// same registry is wired into [`crate::extension::PeerHandles`]
    /// so the install path and the readers see the same data.
    mcp_registry: Arc<crate::mcp::McpRegistry>,
}

impl Server {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<SessionRegistry>,
        agents: Arc<AgentRegistry>,
        agent_index: HostedIndex,
        memory_bank_index: HostedIndex,
        skill_bank_index: HostedIndex,
        tools: Arc<ToolRegistry>,
        policies: Arc<ToolPolicyRegistry>,
        security: SecurityContext,
        tool_profiles: HashMap<String, ToolProfile>,
        context_config: ContextConfig,
        host: Arc<dyn ToolHost>,
        extensions: Arc<ExtensionHub>,
        default_backend: BackendManager,
        mcp_registry: Arc<crate::mcp::McpRegistry>,
    ) -> Arc<Self> {
        let (notify_tx, notify_rx) = mpsc::channel(256);

        let server = Arc::new(Self {
            registry,
            agents,
            agent_index,
            memory_bank_index,
            skill_bank_index,
            tools,
            policies,
            security,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_LLM_CALLS)),
            tool_profiles,
            context_config,
            host,
            extensions,
            default_backend,
            model_fetch_inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            prompt_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            watched: Arc::new(Mutex::new(std::collections::HashSet::new())),
            processing: Arc::new(Mutex::new(std::collections::HashSet::new())),
            skip_counters: Arc::new(Mutex::new(HashMap::new())),
            active_extensions: Arc::new(Mutex::new(HashMap::new())),
            notify_tx,
            agent_burst_budget: AtomicUsize::new(DEFAULT_AGENT_BURST_BUDGET),
            default_agents: std::sync::RwLock::new(Vec::new()),
            config_path: std::sync::RwLock::new(None),
            routine_engine: OnceLock::new(),
            mcp_registry,
        });

        let server_clone = server.clone();
        tokio::spawn(async move {
            server_clone.processing_loop(notify_rx).await;
        });

        let server_clone = server.clone();
        tokio::spawn(async move {
            server_clone.new_session_watcher().await;
        });

        server
    }

    pub fn agent_index(&self) -> &HostedIndex {
        &self.agent_index
    }

    /// Set the list of agents auto-attached to new sessions. Applied
    /// once at startup from `Config.default_agents`. Order is meaningful:
    /// the first entry effectively becomes the routing host (resolution
    /// chain picks the first authorized agent when no @mention applies).
    pub fn set_default_agents(&self, names: Vec<String>) {
        *self
            .default_agents
            .write()
            .expect("default_agents lock poisoned") = names;
    }

    /// Read the current `default_agents` list. Cloned snapshot — caller
    /// doesn't hold the lock past the call.
    pub fn default_agents(&self) -> Vec<String> {
        self.default_agents
            .read()
            .expect("default_agents lock poisoned")
            .clone()
    }

    /// Register the running `RoutineEngine` so contexts built from this
    /// server can resync the live schedule after a committed routine /
    /// schedule mutation. First call wins (one engine per process);
    /// subsequent calls are ignored.
    pub fn set_routine_engine(&self, engine: Arc<crate::routine::RoutineEngine>) {
        let _ = self.routine_engine.set(engine);
    }

    /// The running `RoutineEngine`, if one has been registered. `None`
    /// under `--print` and in tests.
    pub fn routine_engine(&self) -> Option<&Arc<crate::routine::RoutineEngine>> {
        self.routine_engine.get()
    }

    /// The per-process MCP server directory. Populated by
    /// [`crate::extensions::mcp::McpExtension`] during install; read
    /// by the TUI Peer→MCP settings page.
    pub fn mcp_registry(&self) -> &Arc<crate::mcp::McpRegistry> {
        &self.mcp_registry
    }

    /// True iff `session_db_id` is currently registered (between
    /// `register_session` and `deregister_session`). Scheduled work
    /// targeting a closed session should self-skip rather than reopen
    /// an orphan DB.
    pub async fn is_session_open(&self, session_db_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_db_id)
    }

    /// Best-effort attach of the configured default agents to a
    /// freshly-created session so `SessionMeta.agents` mirrors what
    /// message routing will actually pick. Without this, fresh sessions
    /// resolve through the legacy default-fallback chain — routing
    /// works, but `/agents` reports "none attached" and the per-agent
    /// model picker has no agent scopes.
    ///
    /// Source of truth is `Config.default_agents` (set via
    /// `set_default_agents`). When that list is empty, falls back to
    /// attaching `AgentRegistry::default_agent()` — the first agent in
    /// `agents:`.
    ///
    /// Called from user-facing creation paths (TUI `/new`, CLI session
    /// create, TUI startup default). Not called from
    /// `create_child_session` — spawned children are agent-driven and
    /// shouldn't inherit the default unconditionally.
    ///
    /// Skips silently when an agent isn't in the hosted index (e.g.
    /// configured in YAML but its Living Agent DB hasn't been created
    /// yet). Per-agent attach failures are logged but don't unwind the
    /// rest. Returns the list of successfully-attached names in order.
    pub async fn auto_attach_default_agent(&self, session_db_id: &str) -> Vec<String> {
        // Snapshot the configured list; if empty, fall back to a
        // single-default attach so behavior is sane without a
        // `default_agents:` config entry.
        let configured: Vec<String> = self
            .default_agents
            .read()
            .expect("default_agents lock poisoned")
            .clone();
        let names: Vec<String> = if !configured.is_empty() {
            configured
        } else if !self.agents.is_empty() {
            vec![self.agents.default_agent().name]
        } else {
            return Vec::new();
        };

        let mut attached = Vec::with_capacity(names.len());
        for name in names {
            let Some(entry) = self.agent_index.find_by_name(&name) else {
                tracing::debug!(
                    agent = %name,
                    "Configured default agent has no hosted DB entry — skipping auto-attach"
                );
                continue;
            };
            match self
                .registry
                .attach_agent_to_session(session_db_id, &entry)
                .await
            {
                Ok(()) => attached.push(name),
                Err(e) => tracing::warn!(
                    agent = %name,
                    session_db_id,
                    "Auto-attach of default agent failed (continuing with rest): {e}"
                ),
            }
        }
        attached
    }

    /// Decide whether this peer is the home for running `agent_name` in
    /// `session_db_id`. The home-peer gate that prevents two peers from
    /// both running the ReAct loop on the same human message when they
    /// both hold a key on a co-owned agent.
    ///
    /// Returns true (i.e. "go ahead and run") when:
    /// - The agent isn't locally hosted (defensive — nothing to gate; the
    ///   resolver wouldn't have picked us anyway).
    /// - The session can't be opened (defensive — don't silence on
    ///   transient I/O).
    /// - The session's `AgentRef.home_pubkey` for this agent is unset
    ///   (legacy: any keyholder runs — sessions created before this
    ///   feature stay on today's behavior until `/agent rehost` writes
    ///   a value).
    /// - The matching `home_pubkey` equals this peer's pubkey on the
    ///   agent DB (`DbEntry.pubkey`).
    ///
    /// Otherwise returns false — another peer is the home, skip the turn.
    pub async fn peer_is_home_for(&self, session_db_id: &str, agent_name: &str) -> bool {
        let Some(entry) = self.agent_index.find_by_name(agent_name) else {
            return true;
        };
        let Ok((_conv, session_db)) = self.registry.open_session(session_db_id).await else {
            return true;
        };
        let meta = crate::session::read_meta_from_db(&session_db).await;
        is_home_for_agent_ref(&meta.agents, &entry.db_id.to_string(), &entry.pubkey)
    }

    /// Increment the in-memory skip counter for `(session, agent)` and
    /// emit an operator WARN once the count crosses
    /// [`HOME_SKIP_WARN_THRESHOLD`]. Called from the gate skip paths.
    pub(crate) async fn record_home_skip(&self, session_db_id: &str, agent_name: &str) {
        let mut counters = self.skip_counters.lock().await;
        let key = (session_db_id.to_string(), agent_name.to_string());
        let count = counters.entry(key).or_insert(0);
        *count += 1;
        if *count == HOME_SKIP_WARN_THRESHOLD {
            tracing::warn!(
                session_db_id,
                agent = %agent_name,
                skipped_wakes = *count,
                "Home peer has missed {} consecutive wakes for this session/agent. \
                 If this is a stuck home, run `/agent rehost {agent_name}` from a \
                 surviving peer to take over.",
                *count
            );
        }
    }

    /// Reset the skip counter for `(session, agent)`. Called when a turn
    /// actually runs and when `/agent rehost` rewrites the home pubkey.
    pub(crate) async fn reset_home_skip(&self, session_db_id: &str, agent_name: &str) {
        let mut counters = self.skip_counters.lock().await;
        counters.remove(&(session_db_id.to_string(), agent_name.to_string()));
    }

    /// Test-only: read the current skip count for a `(session, agent)`
    /// pair. `0` if no entry exists.
    #[cfg(test)]
    pub(crate) async fn home_skip_count(&self, session_db_id: &str, agent_name: &str) -> u32 {
        let counters = self.skip_counters.lock().await;
        counters
            .get(&(session_db_id.to_string(), agent_name.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Override the agent→agent burst budget. Called once at startup
    /// from `main.rs` when `multi_agent.burst_budget` is configured,
    /// before the gateway starts delivering messages.
    pub fn set_agent_burst_budget(&self, budget: usize) {
        self.agent_burst_budget.store(budget, Ordering::Relaxed);
    }

    /// The active agent→agent burst budget (configured or default).
    pub fn agent_burst_budget(&self) -> usize {
        self.agent_burst_budget.load(Ordering::Relaxed)
    }

    pub fn memory_bank_index(&self) -> &HostedIndex {
        &self.memory_bank_index
    }

    #[allow(dead_code)]
    pub fn skill_bank_index(&self) -> &HostedIndex {
        &self.skill_bank_index
    }

    /// Rebuild the runtime snapshot for `agent` from its Living Agent DB's
    /// `config` store. Returns the input unchanged if the agent
    /// isn't in the peer-local agent index or the DB isn't readable on this
    /// peer — preserves behavior for legacy agents without a DB.
    ///
    /// The rebuilt Agent is upserted back into the in-memory `AgentRegistry`
    /// so subsequent `default_agent` / `get` lookups see the refreshed config.
    pub async fn hydrate_agent_from_db(&self, agent: crate::agent::Agent) -> crate::agent::Agent {
        let Some(entry) = self.agent_index.find_by_name(&agent.name) else {
            return agent;
        };
        let Ok(Some(db)) = self
            .registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
        else {
            return agent;
        };
        let Ok(cfg) = db.read_config().await else {
            return agent;
        };
        let mut rebuilt = self.agents.build_from_db_config(&agent.name, &cfg);
        rebuilt.system_prompt = self.resolve_db_prompt(&cfg).await;
        self.agents.upsert(rebuilt.clone());
        rebuilt
    }

    /// Resolve the system prompt for a DB-backed agent. The normal path reads
    /// the resolved text from the prompt blob store by `system_prompt_ref` (no
    /// per-turn file IO). When no ref is set yet — a freshly bootstrapped agent
    /// that hasn't been reconciled, or one with paths but no blob — it falls
    /// back to reading the files directly so the prompt is never silently empty.
    async fn resolve_db_prompt(&self, cfg: &crate::agent_db::AgentDbConfig) -> String {
        if let Some(snap) = cfg.system_prompt_ref.as_ref()
            && let Some(text) = self.fetch_resolved_prompt(snap).await
        {
            return text;
        }
        let files: Vec<std::path::PathBuf> = cfg
            .system_prompt_files
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        crate::agent::resolve_system_prompt(&cfg.system_prompt, &files)
    }

    /// The prompt blob store on `chaz_peer`.
    fn prompt_store(&self) -> crate::prompt_store::PromptStore {
        crate::prompt_store::PromptStore::new(self.registry.chaz_peer().clone())
    }

    /// Read a resolved prompt by its `system_prompt_ref` snapshot, memoized in
    /// `prompt_cache`. Snapshots are immutable content addresses, so the cache
    /// never goes stale.
    async fn fetch_resolved_prompt(&self, snap: &eidetica::Snapshot) -> Option<String> {
        let key = serde_json::to_string(snap).ok()?;
        if let Some(hit) = self.prompt_cache.lock().unwrap().get(&key).cloned() {
            return Some(hit);
        }
        let text = self.prompt_store().get(snap).await?;
        self.prompt_cache.lock().unwrap().insert(key, text.clone());
        Some(text)
    }

    /// Resolve an inline-plus-files prompt into the blob store and return the
    /// pointer to store in an agent's config. Reuses `current_ref` when the
    /// resolved text is byte-identical to what that snapshot holds (so an
    /// unchanged prompt never churns the store); returns `None` for an empty
    /// prompt. Shared by the yaml reconcile and the `/agent set` re-resolve so
    /// both produce identical refs for identical inputs.
    async fn resolve_prompt_ref(
        &self,
        inline: &str,
        files: &[std::path::PathBuf],
        current_ref: Option<&eidetica::Snapshot>,
    ) -> anyhow::Result<Option<eidetica::Snapshot>> {
        let resolved = crate::agent::resolve_system_prompt(inline, files);
        if resolved.is_empty() {
            return Ok(None);
        }
        let unchanged = match current_ref {
            Some(snap) => self.prompt_store().get(snap).await.as_deref() == Some(&resolved),
            None => false,
        };
        if unchanged {
            Ok(current_ref.cloned())
        } else {
            Ok(Some(self.prompt_store().put(&resolved).await?))
        }
    }

    /// Re-resolve `cfg`'s prompt (inline `system_prompt` + `system_prompt_files`)
    /// into the blob store and update `cfg.system_prompt_ref` in place. Used by
    /// `/agent set` so a manual prompt edit refreshes the blob the same way a
    /// yaml reconcile does — without it, a stale ref would mask the new files.
    /// Leaves `applied_config_hash` untouched: a live edit intentionally keeps
    /// the yaml gate so it survives the next startup reconcile when yaml is
    /// unchanged.
    pub async fn refresh_prompt_ref(
        &self,
        cfg: &mut crate::agent_db::AgentDbConfig,
    ) -> anyhow::Result<()> {
        let files: Vec<std::path::PathBuf> = cfg
            .system_prompt_files
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        cfg.system_prompt_ref = self
            .resolve_prompt_ref(&cfg.system_prompt, &files, cfg.system_prompt_ref.as_ref())
            .await?;
        Ok(())
    }

    /// Record the on-disk chaz yaml path so `/agent reload` and the TUI `[r]`
    /// action can re-read it. Called once at startup.
    pub fn set_config_path(&self, path: std::path::PathBuf) {
        *self.config_path.write().expect("config_path lock poisoned") = Some(path);
    }

    /// Re-read the on-disk chaz yaml and re-run the agent reconcile, optionally
    /// scoped to a single agent name (`only`). Returns the names of agents
    /// whose DB config changed plus how many yaml entries were considered (so a
    /// caller can distinguish "no change" from "no such agent in yaml"). Errors
    /// when no config path is set, or the file can't be read or parsed. Shares
    /// [`Server::reconcile_agent_from_yaml`] with the startup path, so an
    /// on-demand reload behaves identically to a restart.
    pub async fn reload_config_for(&self, only: Option<&str>) -> anyhow::Result<ReloadReport> {
        let path = self
            .config_path
            .read()
            .expect("config_path lock poisoned")
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no config path set — reload unavailable"))?;
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let config: crate::config::Config = serde_yaml::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;

        let Some(agents) = config.agents.as_ref() else {
            return Ok(ReloadReport::default());
        };
        let mut report = ReloadReport::default();
        for ac in agents {
            if let Some(name) = only
                && ac.name != name
            {
                continue;
            }
            report.considered += 1;
            match self.reconcile_agent_from_yaml(ac).await {
                Ok(true) => {
                    report.changed.push(ac.name.clone());
                    tracing::info!(agent = %ac.name, "reloaded agent config from yaml");
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(agent = %ac.name, error = %e, "agent reload failed"),
            }
        }
        Ok(report)
    }

    /// Reconcile one agent's DB config from its yaml definition: resolve the
    /// prompt (reading `system_prompt_files`), store it in the blob store, and —
    /// only when the yaml-derived config differs from what was last applied (the
    /// `applied_config_hash` gate) — write the refreshed declarative config
    /// (including the new `system_prompt_ref`) into the agent's DB. A live
    /// `/agent set` edit survives when the yaml block and prompt files are
    /// unchanged. The blob is written only when the resolved prompt actually
    /// changes (an unchanged prompt reuses the existing ref). Returns `Ok(true)`
    /// when the DB was rewritten, `Ok(false)` when the gate matched or the agent
    /// isn't bootstrapped yet.
    pub async fn reconcile_agent_from_yaml(
        &self,
        ac: &crate::config::AgentConfig,
    ) -> anyhow::Result<bool> {
        let Some(entry) = self.agent_index.find_by_name(&ac.name) else {
            return Ok(false);
        };
        let Some(db) = self
            .registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await?
        else {
            return Ok(false);
        };
        let current = db.read_config().await.unwrap_or_default();

        // Resolve the prompt from yaml's declared sources into the blob store,
        // reusing `current`'s pointer when the text is unchanged.
        let files: Vec<std::path::PathBuf> = ac
            .system_prompt_files
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect();
        let inline = ac.system_prompt.clone().unwrap_or_default();
        let prompt_ref = self
            .resolve_prompt_ref(&inline, &files, current.system_prompt_ref.as_ref())
            .await?;

        // Build the intended declarative config and gate on its hash.
        let mut intended = crate::agent_db::AgentDbConfig::from_agent_config(ac);
        intended.system_prompt_ref = prompt_ref;
        intended.applied_config_hash = None;
        let new_hash = config_gate_hash(&intended)?;
        if current.applied_config_hash.as_deref() == Some(new_hash.as_str()) {
            return Ok(false);
        }
        intended.applied_config_hash = Some(new_hash);
        db.write_config(&intended).await?;
        Ok(true)
    }

    /// Reconcile every agent declared in `config` from yaml into its DB. Run at
    /// startup (after bootstrap) and on demand by `/agent reload`; shares the
    /// hash-gated semantics so both paths behave identically. A per-agent
    /// failure is logged and skipped — one bad entry doesn't abort the rest.
    pub async fn reconcile_agents_from_config(&self, config: &crate::config::Config) {
        let Some(agents) = config.agents.as_ref() else {
            return;
        };
        let mut changed = 0usize;
        for ac in agents {
            match self.reconcile_agent_from_yaml(ac).await {
                Ok(true) => {
                    changed += 1;
                    tracing::info!(agent = %ac.name, "reconciled agent config from yaml");
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(agent = %ac.name, error = %e, "agent reconcile failed")
                }
            }
        }
        if changed > 0 {
            tracing::info!(agents = changed, "reconciled agent configs from yaml");
        }
    }

    pub fn registry(&self) -> &SessionRegistry {
        &self.registry
    }

    /// Load context windows for in-use models from the persisted
    /// `model_info_store` into the default backend's overlay, so window-aware
    /// budgeting works with zero config (no `context_window:` hand-edited into
    /// YAML). Idempotent and cheap — one DB read of the `chaz_peer`
    /// `model_info` store. Call once at startup, before the gateway delivers
    /// messages. A miss (no model used on this machine yet) is a no-op: the
    /// runtime falls back to the static budget until a model is used or picked,
    /// at which point [`ensure_model_window_cached`](Self::ensure_model_window_cached)
    /// populates the store for next time.
    ///
    /// Because the overlay is shared via `Arc`, this also reaches every
    /// per-session worker backend cloned from `default_backend`.
    pub async fn warm_model_windows(&self) {
        let store = crate::model_info_store::ModelInfoStore::new(self.registry.chaz_peer().clone());
        let windows = store.context_windows().await;
        let n = windows.len();
        self.default_backend.set_model_windows(windows);
        if n > 0 {
            tracing::info!(models = n, "Warmed context-window overlay from model store");
        }
    }

    /// If `model`'s context window isn't known yet, fetch its info from the
    /// backend's live catalog in the background, persist it to the in-use
    /// `model_info_store`, and slot the window into the live overlay so the
    /// *next* turn budgets correctly. Non-blocking: the current turn proceeds
    /// model-blind. Deduped per model id (an in-flight or already-known model
    /// is a no-op), so a burst of turns on a new model triggers one fetch.
    ///
    /// This is the "first runtime use" half of how the store gets populated;
    /// the picker covers the "on switch" half by persisting the model you
    /// select. `model` is the id as resolved for budgeting (backend-prefixed
    /// in multi-backend setups), matching `BackendManager::context_window`.
    pub fn ensure_model_window_cached(&self, backend: &BackendManager, model: &str) {
        spawn_model_window_fetch(
            self.registry.chaz_peer().clone(),
            self.model_fetch_inflight.clone(),
            backend.clone(),
            model.to_string(),
        );
    }

    /// The concrete per-turn context budget (in tokens) the runtime would
    /// target for `model` under an optional per-agent cap — the same
    /// resolution `run_*_turn` feeds the context builder
    /// ([`resolve_context_max_tokens`]), with the static configured default
    /// applied when neither a window nor a cap narrows it. Surfaced so the
    /// TUI can render `ctx N%` against the exact denominator the runtime uses.
    ///
    /// Resolves windows through the server's own `default_backend`, which is
    /// the manager whose overlay gets warmed at startup and updated by the
    /// background first-use fetch — a caller's freshly-built `BackendManager`
    /// (e.g. the TUI's) carries an empty overlay and would miss learned
    /// windows.
    pub fn effective_context_budget(&self, model: &str, agent_cap: Option<usize>) -> usize {
        resolve_context_max_tokens(&self.default_backend, Some(model), agent_cap)
            .unwrap_or(self.context_config.max_context_tokens)
    }

    /// Persist a model's pulled info into the in-use [`ModelInfoStore`] and
    /// slot its window (if any) into the live overlay. Called when the user
    /// selects a model in the picker — the "on switch" half of populating the
    /// store (the background first-use fetch is the other half).
    pub async fn cache_model_info(&self, info: &ModelInfo) {
        if let Some(w) = info.context_window {
            self.default_backend.insert_model_window(info.id.clone(), w);
        }
        let store = crate::model_info_store::ModelInfoStore::new(self.registry.chaz_peer().clone());
        if let Err(e) = store.put(info).await {
            tracing::warn!(model = %info.id, "model info store write failed: {e}");
        }
    }

    pub fn registry_arc(&self) -> Arc<SessionRegistry> {
        self.registry.clone()
    }

    pub fn agents(&self) -> &AgentRegistry {
        &self.agents
    }

    pub fn agents_arc(&self) -> Arc<AgentRegistry> {
        self.agents.clone()
    }

    /// Access the compile-time extension hub. Held by `Server` so that
    /// runtime and command dispatch can fire hooks and look up extension
    /// commands.
    pub fn extensions(&self) -> &Arc<ExtensionHub> {
        &self.extensions
    }

    /// Per-session active-extension set, lazily computed from the
    /// session's `extensions` event log and cached. Returned cloned so
    /// the caller can hand it into a [`crate::extension::HookContext`].
    /// Falls back to an empty set if the session DB can't be opened —
    /// in that case no hook fires, which is the conservative choice.
    pub async fn active_extensions_for(
        &self,
        session_db_id: &str,
    ) -> std::collections::HashSet<String> {
        {
            let cache = self.active_extensions.lock().await;
            if let Some(s) = cache.get(session_db_id) {
                return s.clone();
            }
        }
        let active = self.compute_active_extensions(session_db_id).await;
        let mut cache = self.active_extensions.lock().await;
        cache.insert(session_db_id.to_string(), active.clone());
        active
    }

    /// Session active set narrowed by the responding agent's opt-outs.
    ///
    /// The session set is the upper bound (cached, default-on). The
    /// agent can only *remove* from it — `effective = session − agent
    /// opt-outs` — so an agent never silently re-enables an extension
    /// the session disabled. An agent with no extension records (the
    /// common case) leaves the session set untouched.
    pub async fn active_extensions_for_agent(
        &self,
        session_db_id: &str,
        agent_name: &str,
    ) -> std::collections::HashSet<String> {
        let session_active = self.active_extensions_for(session_db_id).await;
        let disabled = self.agent_disabled_extensions(agent_name).await;
        if disabled.is_empty() {
            return session_active;
        }
        session_active.difference(&disabled).cloned().collect()
    }

    /// Recompute the per-session active set from the session DB (without
    /// using the in-memory cache) and refresh the cache. Called after
    /// `/extensions add|remove` writes an event.
    pub async fn refresh_active_extensions(
        &self,
        session_db_id: &str,
    ) -> std::collections::HashSet<String> {
        let active = self.compute_active_extensions(session_db_id).await;
        let mut cache = self.active_extensions.lock().await;
        cache.insert(session_db_id.to_string(), active.clone());
        active
    }

    async fn compute_active_extensions(
        &self,
        session_db_id: &str,
    ) -> std::collections::HashSet<String> {
        let Ok((_conv, db)) = self.registry.open_session(session_db_id).await else {
            return std::collections::HashSet::new();
        };
        match crate::extension::read_active(&db).await {
            Ok(refs) => refs.into_iter().map(|r| r.name().to_string()).collect(),
            Err(e) => {
                tracing::warn!(session = %session_db_id, "Failed to read active extensions: {e}");
                std::collections::HashSet::new()
            }
        }
    }

    /// Register a session for callback-driven agent processing.
    ///
    /// Installs an `on_write` callback on the session DB (if not already
    /// present) that triggers agent processing when new entries appear,
    /// whether written locally or via remote sync. Stores per-session runtime state (backend, agent
    /// override, approval channel) keyed by the session DB ID.
    ///
    /// Gateways should register their own callbacks on the session DB to handle
    /// response delivery.
    ///
    /// Safe to call multiple times — updates metadata, skips duplicate callback registration.
    pub async fn register_session(
        &self,
        session_db: &eidetica::Database,
        backend: BackendManager,
        agent_override: Option<String>,
        approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
    ) -> anyhow::Result<()> {
        let session_db_id = session_db.root_id().to_string();
        let agent_name_for_hook = agent_override
            .clone()
            .unwrap_or_else(|| "agent".to_string());

        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(
                session_db_id.clone(),
                SessionRuntime {
                    backend,
                    agent_override,
                    approval_tx,
                    call_depth: 0,
                    max_call_depth: 0,
                    parent_tools: None,
                    iteration_budget: None,
                    completion_tx: None,
                },
            );
        }

        let mut watched = self.watched.lock().await;
        if watched.contains(&session_db_id) {
            return Ok(());
        }
        watched.insert(session_db_id.clone());
        drop(watched);

        let tx = self.notify_tx.clone();
        let sid = session_db_id.clone();
        session_db
            .on_write(move |_event, _db| {
                let tx = tx.clone();
                let sid = sid.clone();
                Box::pin(async move {
                    let _ = tx.send(sid).await;
                    Ok(())
                })
            })?
            .detach();

        info!(session_db_id = %session_db_id, "Server watching session");

        // Extension hook: session_start
        self.fire_session_start_hook(session_db.clone(), agent_name_for_hook, 0)
            .await;

        Ok(())
    }

    /// Create and register a child session for agent-to-agent communication.
    ///
    /// Creates a fresh session DB via the registry, installs server callbacks,
    /// and returns the session info plus a completion receiver. The caller
    /// writes a Directive entry to trigger execution, then awaits the receiver.
    ///
    /// If `parent_session_db_id` is provided, wires a `DelegatedTreeRef`
    /// (max = Admin(0)) from the child's auth settings back to the parent —
    /// any key with Admin on the parent inherits Admin on the child
    /// transparently. `spawn_agent`/`spawn_worker` rely on this so the
    /// invoking session's supervisor authority carries into the child.
    #[allow(clippy::too_many_arguments)]
    pub async fn register_child_session(
        &self,
        agent_name: &str,
        backend: BackendManager,
        approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
        call_depth: usize,
        max_call_depth: usize,
        parent_tools: ScopedTools,
        parent_session_db_id: Option<&str>,
        iteration_budget: Option<Arc<AtomicU32>>,
    ) -> anyhow::Result<(ConversationId, eidetica::Database, mpsc::Receiver<()>)> {
        let source = format!("spawn:{}", uuid::Uuid::new_v4());
        let (conversation_id, session_db) = match parent_session_db_id {
            Some(parent) => {
                self.registry
                    .create_child_session(parent, Some(&source))
                    .await?
            }
            None => self.registry.create_session(Some(&source)).await?,
        };
        let session_db_id = session_db.root_id().to_string();

        let (completion_tx, completion_rx) = mpsc::channel(1);

        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(
                session_db_id.clone(),
                SessionRuntime {
                    backend,
                    agent_override: Some(agent_name.to_string()),
                    approval_tx,
                    call_depth,
                    max_call_depth,
                    parent_tools: Some(parent_tools),
                    iteration_budget,
                    completion_tx: Some(completion_tx),
                },
            );
        }

        let mut watched = self.watched.lock().await;
        if !watched.contains(&session_db_id) {
            watched.insert(session_db_id.clone());
            drop(watched);

            let tx = self.notify_tx.clone();
            let sid = session_db_id.clone();
            session_db
                .on_write(move |_event, _db| {
                    let tx = tx.clone();
                    let sid = sid.clone();
                    Box::pin(async move {
                        let _ = tx.send(sid).await;
                        Ok(())
                    })
                })?
                .detach();

            info!(
                session_db_id = %session_db_id,
                agent = %agent_name,
                "Server watching child session"
            );
        } else {
            drop(watched);
        }

        // Extension hook: session_start (for child session)
        self.fire_session_start_hook(session_db.clone(), agent_name.to_string(), call_depth)
            .await;

        Ok((conversation_id, session_db, completion_rx))
    }

    /// Fire `session_shutdown` for the given session and remove its runtime
    /// state. Best-effort: process exit / abnormal termination skips this
    /// hook. Idempotent — calling on an unknown session is a no-op.
    pub async fn deregister_session(&self, session_db_id: &str) {
        // Build a hook context from whatever runtime state we still have.
        // If the session is unknown we still let the caller fire-and-forget.
        let agent_name = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(session_db_id)
                .and_then(|s| s.agent_override.clone())
                .unwrap_or_else(|| "agent".to_string())
        };

        let active_extensions = self.active_extensions_for(session_db_id).await;

        if let Ok((_, db)) = self.registry.open_session(session_db_id).await {
            let conv_id = ConversationId(session_db_id.to_string());
            let session = Session::new(conv_id, db).await;
            let ctx = HookContext {
                agent_name,
                model: None,
                call_depth: 0,
                session: Arc::new(Mutex::new(session)),
                active_extensions,
                routine_engine: self.routine_engine().cloned(),
            };
            self.extensions.fire_session_shutdown(&ctx).await;
        }

        // Drop the cached active set so a future re-register starts fresh.
        let mut cache = self.active_extensions.lock().await;
        cache.remove(session_db_id);
        drop(cache);

        // Prune this session's routines from the running engine's heap
        // so a closed session stops firing scheduled wakes.
        if let Some(engine) = self.routine_engine() {
            engine.deregister_session(session_db_id).await;
        }

        let mut sessions = self.sessions.lock().await;
        sessions.remove(session_db_id);
    }

    async fn processing_loop(&self, mut notify_rx: mpsc::Receiver<String>) {
        while let Some(session_db_id) = notify_rx.recv().await {
            // Debounce: drain any pending notifications, dedup
            let mut to_process = vec![session_db_id];
            while let Ok(sid) = notify_rx.try_recv() {
                if !to_process.contains(&sid) {
                    to_process.push(sid);
                }
            }

            for sid in to_process {
                if let Err(e) = self.process_session(&sid).await {
                    error!("Error processing session {sid}: {e}");
                }
            }
        }
    }

    /// Watch for new sessions appearing in the registry (local creates, sync, etc.)
    /// and log them. Gateways are responsible for calling `register_session` to
    /// wire up agent processing and response delivery for their channels.
    async fn new_session_watcher(&self) {
        let Some(mut rx) = self.registry.subscribe_new_sessions().await else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        while let Some(event) = rx.recv().await {
            if !seen.insert(event.session_db_id.clone()) {
                continue;
            }
            if event
                .source
                .as_deref()
                .is_some_and(|s| s.starts_with("spawn:"))
            {
                continue;
            }
            info!(
                session_db_id = %event.session_db_id,
                source = ?event.source,
                "New session detected"
            );
        }
    }

    async fn process_session(&self, session_db_id: &str) -> anyhow::Result<()> {
        let (conversation_id, session_db) = self.registry.open_session(session_db_id).await?;

        let session = Session::new(conversation_id.clone(), session_db.clone()).await;

        let latest = match session.latest_entry() {
            Some(e) => e.clone(),
            None => return Ok(()),
        };

        let sender_is_agent = self.agents.get(&latest.sender).is_some();

        // Chat-room (agent→agent) gate. A human `Message` and any
        // `Directive` wake an agent via the unchanged path below. An
        // agent-authored `Message` wakes another agent only when it
        // explicitly `@mentions` an attached agent (≠ sender) AND the
        // per-burst turn budget is not exhausted. See
        // `docs/src/design/autonomous_agents.md`.
        let agent_to_agent_target = if sender_is_agent && latest.entry_type == EntryType::Message {
            let burst = crate::session::trailing_agent_message_burst(session.entries(), |name| {
                self.agents.get(name).is_some()
            });
            if burst >= self.agent_burst_budget() {
                info!(
                    session_db_id,
                    burst,
                    budget = self.agent_burst_budget(),
                    "Agent turn budget exhausted — suppressing agent→agent wake"
                );
                None
            } else {
                self.registry
                    .resolve_mentioned_agent(
                        session_db_id,
                        &latest.content,
                        &latest.sender,
                        &self.agent_index,
                    )
                    .await
            }
        } else {
            None
        };

        let should_process = match latest.entry_type {
            EntryType::Message if !sender_is_agent => true,
            EntryType::Message => agent_to_agent_target.is_some(),
            EntryType::Directive => true,
            _ => false,
        };
        if !should_process {
            return Ok(());
        }

        {
            let mut processing = self.processing.lock().await;
            if !processing.insert(session_db_id.to_string()) {
                return Ok(());
            }
        }
        let (backend, agent_override, approval_tx, spawn_ctx) = {
            let sessions = self.sessions.lock().await;
            match sessions.get(session_db_id) {
                Some(m) => (
                    m.backend.clone(),
                    m.agent_override.clone(),
                    m.approval_tx.clone(),
                    SpawnContext {
                        call_depth: m.call_depth,
                        max_call_depth: m.max_call_depth,
                        parent_tools: m.parent_tools.clone(),
                        iteration_budget: m.iteration_budget.clone(),
                        completion_tx: m.completion_tx.clone(),
                        processing: self.processing.clone(),
                    },
                ),
                None => {
                    // Session not registered for processing — clear lock and bail.
                    self.processing.lock().await.remove(session_db_id);
                    return Ok(());
                }
            }
        };

        // For the agent→agent case the speaker is already pinned to the
        // mentioned agent (computed in the gate above); only the
        // human/Directive path runs the full resolution precedence.
        let agent = match agent_to_agent_target {
            Some(a) => a,
            None => {
                self.registry
                    .resolve_agent_for_entry(
                        session_db_id,
                        agent_override.as_deref(),
                        &self.agent_index,
                        Some(&latest.content),
                    )
                    .await
            }
        };

        // Live hydration: if the resolved agent has a Living Agent DB
        // on this peer, rebuild its runtime snapshot from the DB's `config`
        // store so edits to the DB (local or synced from origin peer)
        // propagate to the next run without a restart.
        let agent = self.hydrate_agent_from_db(agent).await;

        // Per-session home-peer gate. When the session's AgentRef for this
        // agent names a `home_pubkey`, only the peer whose local key on the
        // agent DB matches will run the ReAct loop. Co-owning peers see the
        // same wake, pass `should_process`, but skip here. See
        // `~/brain/tech/projects/chaz/home-peer-plan.md`.
        if !self.peer_is_home_for(session_db_id, &agent.name).await {
            debug!(
                session_db_id,
                agent = %agent.name,
                "Not home peer for this session/agent; skipping turn"
            );
            self.processing.lock().await.remove(session_db_id);
            self.record_home_skip(session_db_id, &agent.name).await;
            return Ok(());
        }
        self.reset_home_skip(session_db_id, &agent.name).await;

        self.spawn_agent_task(
            session_db_id.to_string(),
            session,
            agent,
            approval_tx,
            backend,
            spawn_ctx,
        )
        .await;

        Ok(())
    }

    async fn spawn_agent_task(
        &self,
        session_db_id: String,
        session: Session,
        agent: crate::agent::Agent,
        approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
        backend: BackendManager,
        spawn: SpawnContext,
    ) {
        let agent_name = agent.name.clone();
        let system_prompt = agent.system_prompt.clone();
        let default_model = agent.default_model.clone();
        let allowed_tools = agent.allowed_tools.clone();
        let agent_grants = agent.grants.clone();
        let max_call_depth = if spawn.max_call_depth > 0 {
            spawn.max_call_depth
        } else {
            agent.max_iterations as usize
        };

        let profile = agent
            .tool_profile
            .as_ref()
            .and_then(|name| self.tool_profiles.get(name))
            .cloned()
            .unwrap_or_default();

        let tools = self.tools.clone();
        let policies = self.policies.clone();
        let security = self.security.clone();
        let semaphore = self.semaphore.clone();
        let context_config = self.context_config.clone();
        let host = self.host.clone();
        let spawn_extensions = self.extensions.clone();
        let routine_engine = self.routine_engine().cloned();
        let max_context_tokens = agent.max_context_tokens;
        // Captured for the background "learn this model's window" path, since
        // the spawned task below has no `&self`.
        let chaz_peer = self.registry.chaz_peer().clone();
        let model_fetch_inflight = self.model_fetch_inflight.clone();
        let active_extensions = self
            .active_extensions_for_agent(&session_db_id, &agent_name)
            .await;

        tokio::spawn(async move {
            let _permit = semaphore.acquire().await.expect("semaphore closed");
            let session = Arc::new(Mutex::new(session));

            {
                let mut s = session.lock().await;
                s.add_entry(SessionEntry {
                    sender: agent_name.clone(),
                    content: String::new(),
                    timestamp: Utc::now(),
                    entry_type: EntryType::Ack,
                    metadata: None,
                })
                .await;
            }

            let request_security = SecurityContext {
                leak_detector: security.leak_detector.clone(),
                auto_approved_tools: security.auto_approved_tools.clone(),
                approval_callback: approval_tx,
            };

            // Layer the per-session active-extension filter on top of the
            // parent (or root) scope. `narrow` already propagates the set
            // when spawning a child, but `with_active_extensions` here
            // re-asserts the current session's set in case the parent was
            // built from a different lineage.
            let scoped_tools = match spawn.parent_tools {
                Some(parent) => parent.narrow(allowed_tools.as_deref()),
                None => ScopedTools::new(tools, allowed_tools),
            }
            .with_active_extensions(Some(active_extensions.clone()));

            // Inherit the parent's budget if a spawning Worker passed one
            // in; otherwise this is a top-level run (gateway / schedule
            // wake) and we allocate a fresh budget seeded from the
            // agent's `max_iterations`.
            let iteration_budget = spawn
                .iteration_budget
                .clone()
                .unwrap_or_else(|| Arc::new(AtomicU32::new(agent.max_iterations)));

            let tool_ctx = ToolContext {
                agent_name: agent_name.clone(),
                call_depth: spawn.call_depth,
                max_call_depth,
                tools: scoped_tools,
                profile,
                session: session.clone(),
                grants: Default::default(),
                agent_grants,
                host: host.clone(),
                active_extensions: active_extensions.clone(),
                iteration_budget: Some(iteration_budget),
                routine_engine: routine_engine.clone(),
            };

            let tool_defs = tool_ctx.tools.definitions(&tool_ctx.profile);
            let (session_model, assembled) = {
                let s = session.lock().await;
                let meta = s.read_meta().await;
                let roster: Vec<String> =
                    meta.agents.iter().map(|a| a.display_name.clone()).collect();
                // Per-agent override > session pin > backend default. The
                // backend-default fallback mirrors the actual call, so an agent
                // that pins no model still budgets against its real window.
                let session_model = meta
                    .resolve_model_for_agent(&agent_name)
                    .map(str::to_string);
                let budget_model =
                    budget_model_id(&backend, session_model.as_deref(), default_model.as_deref());
                // First use of a model we don't have a window for yet: learn it
                // in the background so the next turn budgets window-aware.
                if let Some(m) = budget_model.as_deref() {
                    spawn_model_window_fetch(
                        chaz_peer.clone(),
                        model_fetch_inflight.clone(),
                        backend.clone(),
                        m.to_string(),
                    );
                }
                let max_tokens_override = resolve_context_max_tokens(
                    &backend,
                    budget_model.as_deref(),
                    max_context_tokens,
                );
                let assembled =
                    ContextBuilder::new(s.entries(), &agent_name, &system_prompt, &context_config)
                        .with_tools(&tool_defs)
                        .with_max_tokens_override(max_tokens_override)
                        .with_room_participants(&roster)
                        .with_extension_hub(spawn_extensions.clone())
                        .with_session_db(s.database())
                        .build()
                        .await;
                (session_model, assembled)
            };
            // Per-agent override > session pin > agent default. See
            // `run_schedule_turn` for the matching path on scheduled fires.
            let effective_model = session_model.or(default_model);

            if assembled.truncated {
                info!(
                    "Context truncated for {}: {} entries, ~{} tokens",
                    agent_name, assembled.entries_included, assembled.estimated_tokens
                );
            }

            let (event_tx, mut event_rx) = mpsc::channel::<runtime::RuntimeEvent>(64);
            let event_session = session.clone();
            let event_agent = agent_name.clone();
            let event_writer = tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    let mut s = event_session.lock().await;
                    match event {
                        runtime::RuntimeEvent::ToolCall {
                            name, arguments, ..
                        } => {
                            s.add_entry(SessionEntry {
                                sender: event_agent.clone(),
                                content: format!("{name}({arguments})"),
                                timestamp: Utc::now(),
                                entry_type: EntryType::ToolCall,
                                metadata: None,
                            })
                            .await;
                        }
                        runtime::RuntimeEvent::ToolResult {
                            name,
                            output,
                            is_error,
                            ..
                        } => {
                            let content = if is_error {
                                format!("{name}: ERROR: {output}")
                            } else {
                                let t = crate::util::truncate_chars(&output, 500);
                                let truncated = if t.len() < output.len() {
                                    format!("{t}…")
                                } else {
                                    output
                                };
                                format!("{name}: {truncated}")
                            };
                            s.add_entry(SessionEntry {
                                sender: event_agent.clone(),
                                content,
                                timestamp: Utc::now(),
                                entry_type: EntryType::ToolResult,
                                metadata: None,
                            })
                            .await;
                        }
                    }
                }
            });

            let result = runtime::execute(
                effective_model.as_deref(),
                assembled.messages,
                &backend,
                &request_security,
                &tool_ctx,
                &policies,
                Some(event_tx),
                Some(spawn_extensions.as_ref()),
            )
            .await;

            let _ = event_writer.await;

            drop(_permit);

            let mut s = session.lock().await;
            match result {
                Ok(outcome) if outcome.body.trim().is_empty() => {
                    // Silent turn — the agent acted via tools or chose
                    // not to speak. Write no Message: keeps the room
                    // and LLM context uncluttered and doesn't trip the
                    // multi-agent burst counter. The turn's cost is not
                    // dropped silently — for autonomous wakes the fire
                    // path attributes it to the agent's own fire log
                    // (see agent-owned schedules); session usage stays
                    // Message-only by design.
                    debug!(
                        agent = %agent_name,
                        session = %session_db_id,
                        "Agent produced no message (silent turn) — no entry written"
                    );
                }
                Ok(outcome) => {
                    s.add_entry(SessionEntry {
                        sender: agent_name,
                        content: outcome.body,
                        timestamp: Utc::now(),
                        entry_type: EntryType::Message,
                        metadata: outcome.metadata,
                    })
                    .await;
                }
                Err(err) => {
                    error!("Agent error for {}: {err}", session_db_id);
                    s.add_entry(SessionEntry {
                        sender: agent_name,
                        content: format!("Error: {err}"),
                        timestamp: Utc::now(),
                        entry_type: EntryType::Error,
                        metadata: None,
                    })
                    .await;
                }
            }
            drop(s);

            {
                let mut proc = spawn.processing.lock().await;
                proc.remove(&session_db_id);
            }

            if let Some(tx) = spawn.completion_tx {
                let _ = tx.send(()).await;
            }
        });
    }
}

#[cfg(test)]
mod tests;
