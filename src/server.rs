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
use crate::backends::BackendManager;
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
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tracing::{debug, error, info};

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
    /// Signaled when the agent task completes (for synchronous spawn_agent)
    completion_tx: Option<mpsc::Sender<()>>,
}

/// Context for spawned agent tasks (call depth, tool scope, completion signal).
struct SpawnContext {
    call_depth: usize,
    max_call_depth: usize,
    parent_tools: Option<ScopedTools>,
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
            sessions: Arc::new(Mutex::new(HashMap::new())),
            watched: Arc::new(Mutex::new(std::collections::HashSet::new())),
            processing: Arc::new(Mutex::new(std::collections::HashSet::new())),
            skip_counters: Arc::new(Mutex::new(HashMap::new())),
            active_extensions: Arc::new(Mutex::new(HashMap::new())),
            notify_tx,
            agent_burst_budget: AtomicUsize::new(DEFAULT_AGENT_BURST_BUDGET),
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
    /// `config` store (Stage 8). Returns the input unchanged if the agent
    /// isn't in the peer-local agent index or the DB isn't readable on this
    /// peer — preserves behavior for legacy agents without a DB.
    ///
    /// The rebuilt Agent is upserted back into the in-memory `AgentRegistry`
    /// so subsequent `can_spawn` / `default_agent` / legacy lookups see the
    /// refreshed config too.
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
        let rebuilt = self.agents.build_from_db_config(&agent.name, &cfg);
        self.agents.upsert(rebuilt.clone());
        rebuilt
    }

    pub fn registry(&self) -> &SessionRegistry {
        &self.registry
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
    /// transparently. Stage 5 `spawn_agent`/`spawn_task` rely on this so the
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

    // -----------------------------------------------------------------
    // Agent-Owned Schedule fire path (Stage 3)
    // -----------------------------------------------------------------

    /// Standalone execution path for agent-owned schedule fires.
    ///
    /// This is deliberately separate from [`Self::process_session`] — it
    /// calls [`crate::runtime::execute`] directly without touching the
    /// session's `SessionRuntime` (agent_override, backend, completion
    /// channel). A Pinned schedule firing into a live session the user is
    /// chatting in does not hijack the interactive routing.
    ///
    /// Steps:
    /// 1. Host check — skip if the agent isn't on this peer.
    /// 2. Resolve target — Fresh creates a session + attaches the owner;
    ///    Pinned opens the existing session + idempotently attaches.
    /// 3. Acquire the session's `processing` lock; skip if busy.
    /// 4. Load the agent, build context with the wake-prompt as a private
    ///    System message, run the ReAct loop.
    /// 5. Write ToolCall/ToolResult entries + a terminal Message (only if
    ///    non-empty; silent turns produce no entry).
    /// 6. Record a [`crate::agent_db::ScheduleFire`] with usage on the
    ///    agent's DB.
    /// 7. One-shot cleanup: delete the Schedule row from the agent DB.
    pub async fn fire_agent_schedule(
        &self,
        payload: crate::routine::AgentSchedulePayload,
    ) -> anyhow::Result<()> {
        use crate::agent_db::{ScheduleFire, ScheduleTarget};

        // 1. Host check — unparseable IDs are silently skipped (they
        //    can't possibly be hosted on this peer).
        let owner_id = match eidetica::entry::ID::parse(&payload.owner_agent_db_id) {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!(
                    agent_db_id = %payload.owner_agent_db_id,
                    schedule = %payload.schedule_id,
                    "Unparseable owner_agent_db_id; skipping schedule fire: {e}"
                );
                return Ok(());
            }
        };
        let Some(agent_entry) = self.agent_index.find_by_id(&owner_id) else {
            tracing::debug!(
                agent_db_id = %payload.owner_agent_db_id,
                schedule = %payload.schedule_id,
                "Agent not hosted on this peer; skipping schedule fire"
            );
            return Ok(());
        };

        // 1b. Lifecycle bound check (Gap 4). The agent DB is the
        //     authoritative store; the engine's in-memory routine is
        //     rebuilt from it. If the schedule has hit its expiry or
        //     max_fires bound, persist `enabled = false` and skip —
        //     this is what actually retires a recurring schedule (the
        //     in-memory routine keeps ticking until the next
        //     reload/restart, but every tick now early-returns here).
        if let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await
            && let Ok(Some(mut schedule)) = adb.find_schedule(&payload.schedule_id).await
            && let Some(reason) = schedule.retirement_reason(Utc::now())
        {
            if schedule.enabled {
                schedule.enabled = false;
                if let Err(e) = adb.upsert_schedule(schedule).await {
                    tracing::error!(
                        agent = %agent_entry.display_name,
                        schedule = %payload.schedule_id,
                        "Failed to persist schedule retirement: {e}"
                    );
                }
            }
            tracing::info!(
                agent = %agent_entry.display_name,
                schedule = %payload.schedule_id,
                "Schedule retired ({reason}); skipping fire"
            );
            return Ok(());
        }

        // 2. Resolve target
        let target: ScheduleTarget = serde_json::from_value(payload.target)
            .map_err(|e| anyhow::anyhow!("invalid schedule target in payload: {e}"))?;

        let agent_name = agent_entry.display_name.clone();

        let (session_db, is_fresh, session_db_id) = match &target {
            ScheduleTarget::Fresh => {
                // Home-peer gate (agent-level). Fresh schedules have no
                // session yet to carry `AgentRef.home_pubkey`, so the gate
                // falls back to the agent DB's `meta.home_pubkey`. Legacy
                // `None` lets every keyholder run (pre-feature default).
                if let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await
                    && let Some(home) =
                        crate::db_kind::read_agent_home_pubkey(adb.database()).await
                    && home != agent_entry.pubkey
                {
                    tracing::debug!(
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Not home peer for agent's Fresh schedule fire; skipping"
                    );
                    return Ok(());
                }

                let source = format!(
                    "schedule:{}:{}",
                    payload.owner_agent_db_id, payload.schedule_id
                );
                let (_conv, db) = self.registry.create_session(Some(&source)).await?;
                let sid = db.root_id().to_string();

                // Attach the owner agent to the session so it has Write
                // permission and the session meta records membership.
                self.registry
                    .attach_agent_to_session(&sid, &agent_entry)
                    .await?;

                // Register with the server so the session has a
                // SessionRuntime (backend + on_write callback) for any
                // tools that need it (spawn_agent writes to the on_write
                // path). The agent_override is set to the schedule owner so
                // future interactive writes route to this agent.
                self.register_session(
                    &db,
                    self.default_backend.clone(),
                    Some(agent_name.clone()),
                    None,
                )
                .await?;

                tracing::info!(
                    session = %sid,
                    agent = %agent_name,
                    schedule = %payload.schedule_id,
                    "Created Fresh session for agent schedule fire"
                );

                (db, true, sid)
            }
            ScheduleTarget::Pinned { session_db_id } => {
                let (_conv, db) = self.registry.open_session(session_db_id).await?;
                let sid = session_db_id.clone();

                // Home-peer gate (per-session). Pinned schedules fire into
                // an existing session, so the gate uses that session's
                // `AgentRef.home_pubkey` — same source as `process_session`.
                // If the session was rehosted to another peer, this fire
                // belongs to them.
                if !self.peer_is_home_for(&sid, &agent_name).await {
                    tracing::debug!(
                        session = %sid,
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Not home peer for pinned schedule's session; skipping"
                    );
                    self.record_home_skip(&sid, &agent_name).await;
                    return Ok(());
                }

                // Idempotent attach: if the agent was detached after the
                // schedule was created, the fire-time membership check
                // catches it and re-attaches. If attach fails (session
                // gone, auth broken), skip this fire.
                let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
                let meta = session.read_meta().await;
                let already_member = meta
                    .agents
                    .iter()
                    .any(|a| a.db_id == agent_entry.db_id.to_string());
                if !already_member {
                    if let Err(e) = self
                        .registry
                        .attach_agent_to_session(&sid, &agent_entry)
                        .await
                    {
                        tracing::warn!(
                            session = %sid,
                            agent = %agent_name,
                            schedule = %payload.schedule_id,
                            "Pinned schedule: failed to re-attach agent to session: {e}"
                        );
                        return Ok(()); // self-skip
                    }
                    tracing::info!(
                        session = %sid,
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Pinned schedule: re-attached agent to session"
                    );
                }

                (db, false, sid)
            }
        };

        // 3. Acquire the processing lock (skip if session is busy).
        //    Release at the end of this scope via the deferred block.
        {
            let mut processing = self.processing.lock().await;
            if !processing.insert(session_db_id.clone()) {
                tracing::debug!(
                    session = %session_db_id,
                    schedule = %payload.schedule_id,
                    "Session busy; skipping schedule fire"
                );
                return Ok(());
            }
        }

        // 4. Load the agent + build context + run the turn.
        let outcome = self
            .run_schedule_turn(
                &agent_name,
                &session_db,
                &session_db_id,
                &payload.prompt,
                &payload.schedule_id,
            )
            .await;

        // Release the processing lock.
        {
            let mut processing = self.processing.lock().await;
            processing.remove(&session_db_id);
        }

        // 5. Record ScheduleFire on the agent's DB (best-effort audit).
        let fired_at = Utc::now();
        if let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await {
            let fire = ScheduleFire {
                schedule_id: payload.schedule_id.clone(),
                fired_at,
                session_db_id: session_db_id.clone(),
                fresh: is_fresh,
                usage: outcome.as_ref().ok().and_then(|o| o.metadata.clone()),
            };
            if let Err(e) = adb.record_schedule_fire(fire).await {
                tracing::error!(
                    agent = %agent_name,
                    schedule = %payload.schedule_id,
                    "Failed to record ScheduleFire: {e}"
                );
            }

            // Lifecycle accounting (Gap 4): a recurring schedule that
            // ran its turn increments `fire_count`. When that reaches
            // `max_fires`, retire it now (persist enabled=false) so the
            // next tick's pre-check — and any reload — drops it. One-shot
            // schedules are deleted below, so they don't count.
            if !payload.one_shot
                && outcome.is_ok()
                && let Ok(Some(mut schedule)) = adb.find_schedule(&payload.schedule_id).await
            {
                schedule.fire_count = schedule.fire_count.saturating_add(1);
                if let Some(reason) = schedule.retirement_reason(fired_at) {
                    schedule.enabled = false;
                    tracing::info!(
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        fire_count = schedule.fire_count,
                        "Schedule retired after fire ({reason})"
                    );
                }
                if let Err(e) = adb.upsert_schedule(schedule).await {
                    tracing::error!(
                        agent = %agent_name,
                        schedule = %payload.schedule_id,
                        "Failed to persist schedule fire_count: {e}"
                    );
                }
            }
        }

        // 6. One-shot cleanup: delete the Schedule row after a successful fire.
        if payload.one_shot
            && let Some(adb) = self.open_agent_db_for_schedule(&agent_entry).await
        {
            if let Err(e) = adb.remove_schedule(&payload.schedule_id).await {
                tracing::error!(
                    agent = %agent_name,
                    schedule = %payload.schedule_id,
                    "Failed to remove one-shot schedule: {e}"
                );
            } else {
                tracing::info!(
                    agent = %agent_name,
                    schedule = %payload.schedule_id,
                    "Removed one-shot schedule after successful fire"
                );
            }
        }

        // Surface any runtime error
        outcome.map(|_| ())
    }

    /// Open a hosted agent's Living Agent DB by display name. `None` if
    /// the agent isn't in this peer's hosted index or the DB can't be
    /// opened (no key / read error). Used by per-agent extension
    /// activation (`/extensions … agent`) and the dispatch-time filter.
    pub async fn open_agent_db_by_name(
        &self,
        agent_name: &str,
    ) -> Option<crate::agent_db::AgentDb> {
        let entry = self.agent_index.find_by_name(agent_name)?;
        self.open_agent_db_for_schedule(&entry).await
    }

    /// Per-agent narrowing filter: extension names the agent has
    /// explicitly opted out of, folded from its Living Agent DB's
    /// sparse `extensions` log. Empty when the agent has no records or
    /// isn't hosted here — i.e. "no opinion, allow everything the
    /// session allows".
    pub async fn agent_disabled_extensions(
        &self,
        agent_name: &str,
    ) -> std::collections::HashSet<String> {
        let Some(adb) = self.open_agent_db_by_name(agent_name).await else {
            return std::collections::HashSet::new();
        };
        crate::extension::read_disabled(adb.database())
            .await
            .unwrap_or_default()
    }

    /// Open the agent's Living Agent DB if this peer hosts the agent.
    async fn open_agent_db_for_schedule(
        &self,
        entry: &crate::hosted_index::DbEntry,
    ) -> Option<crate::agent_db::AgentDb> {
        match self
            .registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
        {
            Ok(Some(adb)) => Some(adb),
            Ok(None) => {
                tracing::warn!(agent = %entry.display_name, "No key for agent DB; can't write fire audit");
                None
            }
            Err(e) => {
                tracing::error!(agent = %entry.display_name, "Failed to open agent DB: {e}");
                None
            }
        }
    }

    /// Load the agent, hydrate from DB, build context with the wake-prompt
    /// as a private System message, run the ReAct loop, and write entries.
    ///
    /// Returns the [`crate::runtime::RuntimeOutcome`] so the caller can
    /// extract usage metadata for cost attribution.
    async fn run_schedule_turn(
        &self,
        agent_name: &str,
        session_db: &eidetica::Database,
        session_db_id: &str,
        wake_prompt: &str,
        schedule_id: &str,
    ) -> anyhow::Result<crate::runtime::RuntimeOutcome> {
        // Load the agent: check the in-memory registry first; build from
        // DB config if not present (agent was never attached to a session
        // this boot).
        let mut agent = match self.agents.get(agent_name) {
            Some(a) => a,
            None => {
                // Build a minimal agent from the DB config.
                // hydrate_agent_from_db below will fill in the rest.
                self.agents
                    .build_from_db_config(agent_name, &crate::agent_db::AgentDbConfig::default())
            }
        };
        // Hydrate from the Living Agent DB (Stage 8 refresh).
        agent = self.hydrate_agent_from_db(agent).await;

        let default_model = agent.default_model.clone();
        let allowed_tools = agent.allowed_tools.clone();
        let agent_grants = agent.grants.clone();
        let max_call_depth = agent.max_iterations as usize;
        let max_context_tokens = agent.max_context_tokens;
        let profile = agent
            .tool_profile
            .as_ref()
            .and_then(|name| self.tool_profiles.get(name))
            .cloned()
            .unwrap_or_default();

        let active_extensions = self
            .active_extensions_for_agent(session_db_id, agent_name)
            .await;
        let scoped_tools = ScopedTools::new(self.tools.clone(), allowed_tools)
            .with_active_extensions(Some(active_extensions.clone()));

        // Build the session view + context
        let session = Session::new(
            ConversationId(session_db_id.to_string()),
            session_db.clone(),
        )
        .await;
        let session = Arc::new(tokio::sync::Mutex::new(session));

        let tool_ctx = ToolContext {
            agent_name: agent_name.to_string(),
            call_depth: 0,
            max_call_depth,
            tools: scoped_tools,
            profile,
            session: session.clone(),
            grants: Default::default(),
            agent_grants,
            host: self.host.clone(),
            active_extensions: active_extensions.clone(),
        };

        let tool_defs = tool_ctx.tools.definitions(&tool_ctx.profile);
        let mut assembled = {
            let s = session.lock().await;
            let roster: Vec<String> = s
                .read_meta()
                .await
                .agents
                .iter()
                .map(|a| a.display_name.clone())
                .collect();
            ContextBuilder::new(
                s.entries(),
                agent_name,
                &agent.system_prompt,
                &self.context_config,
            )
            .with_tools(&tool_defs)
            .with_max_tokens_override(max_context_tokens)
            .with_room_participants(&roster)
            .with_extension_hub(self.extensions.clone())
            .with_session_db(session_db)
            .build()
            .await
        };

        // Prepend the wake-prompt as a private System message. This is
        // invocation-scoped — it never appears as a session entry.
        assembled.messages.insert(
            0,
            crate::runtime::RuntimeMessage::System(wake_prompt.to_string()),
        );

        if assembled.truncated {
            tracing::info!(
                agent = %agent_name,
                schedule = %schedule_id,
                "Context truncated: {} entries, ~{} tokens",
                assembled.entries_included,
                assembled.estimated_tokens
            );
        }

        // Event writer: capture ToolCall/ToolResult as session entries.
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::channel::<crate::runtime::RuntimeEvent>(64);
        let event_session = session.clone();
        let event_agent = agent_name.to_string();
        let event_writer = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let mut s = event_session.lock().await;
                match event {
                    crate::runtime::RuntimeEvent::ToolCall {
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
                    crate::runtime::RuntimeEvent::ToolResult {
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

        let request_security = SecurityContext {
            leak_detector: self.security.leak_detector.clone(),
            auto_approved_tools: self.security.auto_approved_tools.clone(),
            approval_callback: None, // no interactive approval for schedule fires
        };

        let result = crate::runtime::execute(
            default_model.as_deref(),
            assembled.messages,
            &self.default_backend,
            &request_security,
            &tool_ctx,
            &self.policies,
            Some(event_tx),
            Some(self.extensions.as_ref()),
        )
        .await;

        let _ = event_writer.await;

        // Write the terminal Message (conditional — skip empty).
        let mut s = session.lock().await;
        match &result {
            Ok(outcome) if outcome.body.trim().is_empty() => {
                tracing::debug!(
                    agent = %agent_name,
                    schedule = %schedule_id,
                    "Silent schedule turn — no Message written"
                );
            }
            Ok(outcome) => {
                s.add_entry(SessionEntry {
                    sender: agent_name.to_string(),
                    content: outcome.body.clone(),
                    timestamp: Utc::now(),
                    entry_type: EntryType::Message,
                    metadata: outcome.metadata.clone(),
                })
                .await;
            }
            Err(err) => {
                s.add_entry(SessionEntry {
                    sender: agent_name.to_string(),
                    content: format!("Error: {err}"),
                    timestamp: Utc::now(),
                    entry_type: EntryType::Error,
                    metadata: None,
                })
                .await;
            }
        }

        result.map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Build a `HookContext` for the given session and fire `session_start`.
    /// Internal helper shared by `register_session` / `register_child_session`.
    async fn fire_session_start_hook(
        &self,
        session_db: eidetica::Database,
        agent_name: String,
        call_depth: usize,
    ) {
        // Framework-level: record activation events for the current extension
        // set onto the session DB. Idempotent on repeat calls; only writes
        // when the set or a version differs from the latest stored event,
        // and respects `Deactivated` (so a `/extensions remove` survives
        // restart). Failure is non-fatal — we'd rather lose provenance for
        // one session-start than block the agent turn.
        if let Err(e) = self.extensions.record_active(&session_db).await {
            tracing::warn!(
                conv = %session_db.root_id(),
                "Failed to record extension activation events: {e}"
            );
        }

        let session_db_id = session_db.root_id().to_string();
        let active_extensions = self.refresh_active_extensions(&session_db_id).await;

        let conv_id = ConversationId(session_db_id);
        let session = Session::new(conv_id, session_db).await;
        let ctx = HookContext {
            agent_name,
            model: None,
            call_depth,
            session: Arc::new(Mutex::new(session)),
            active_extensions,
        };
        self.extensions.fire_session_start(&ctx).await;
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
            };
            self.extensions.fire_session_shutdown(&ctx).await;
        }

        // Drop the cached active set so a future re-register starts fresh.
        let mut cache = self.active_extensions.lock().await;
        cache.remove(session_db_id);
        drop(cache);

        // Prune this session's routines from the running engine's heap
        // so a closed session stops firing scheduled wakes.
        crate::routine::notify_session_closed(session_db_id).await;

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

        // Stage 8 live hydration: if the resolved agent has a Living Agent DB
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
        let max_context_tokens = agent.max_context_tokens;
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
            };

            let tool_defs = tool_ctx.tools.definitions(&tool_ctx.profile);
            let assembled = {
                let s = session.lock().await;
                let roster: Vec<String> = s
                    .read_meta()
                    .await
                    .agents
                    .into_iter()
                    .map(|a| a.display_name)
                    .collect();
                ContextBuilder::new(s.entries(), &agent_name, &system_prompt, &context_config)
                    .with_tools(&tool_defs)
                    .with_max_tokens_override(max_context_tokens)
                    .with_room_participants(&roster)
                    .with_extension_hub(spawn_extensions.clone())
                    .with_session_db(s.database())
                    .build()
                    .await
            };

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
                default_model.as_deref(),
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
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
    use crate::hosted_index::DbEntry;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;

    /// Build a Server with the minimum wiring needed to exercise hydration.
    async fn server_fixture() -> (Instance, Arc<Server>, Arc<crate::session::SessionRegistry>) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(
            crate::session::SessionRegistry::new(instance.clone(), user, agents.clone())
                .await
                .unwrap(),
        );
        let index = HostedIndex::empty("agent");
        let bank_index = HostedIndex::empty("bank");
        let tools = Arc::new(ToolRegistry::new());
        let policies = Arc::new(crate::tool::ToolPolicyRegistry::empty());
        let security = SecurityContext {
            leak_detector: crate::security::LeakDetector::new(
                crate::security::LeakPolicy::default(),
            ),
            auto_approved_tools: std::collections::HashSet::new(),
            approval_callback: None,
        };
        let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
        let default_backend = crate::backends::BackendManager::new(&None, secrets);
        let server = Server::new(
            registry.clone(),
            agents,
            index,
            bank_index,
            crate::hosted_index::HostedIndex::empty("skill_bank"),
            tools,
            policies,
            security,
            HashMap::new(),
            Default::default(),
            Arc::new(crate::tool_host::NativeToolHost::new()),
            Arc::new(crate::extension::ExtensionHub::new()),
            default_backend,
        );
        (instance, server, registry)
    }

    #[tokio::test]
    async fn hydrate_picks_up_db_config_edits() {
        let (_instance, server, registry) = server_fixture().await;

        // Create an Agent DB with V1 config: haiku / 5 iters.
        let (db, pubkey) = {
            let mut user = registry.user_for_tests().await;
            create_agent_db(
                &mut user,
                "alpha",
                &AgentDbConfig {
                    model: Some("haiku".to_string()),
                    max_iterations: Some(5),
                    ..Default::default()
                },
                &AgentMeta {
                    display_name: Some("alpha".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        };
        server.agent_index().register(DbEntry {
            db_id: db.id(),
            display_name: "alpha".to_string(),
            pubkey,
        });

        // Seed the in-memory registry with a stale entry (model="opus", iter=999)
        // — exactly what would happen if yaml drifted from DB, or if a prior
        // hydration happened before a DB edit.
        let mut stale = crate::agent::Agent {
            name: "alpha".to_string(),
            system_prompt: String::new(),
            system_prompt_files: vec![],
            default_model: Some("opus".to_string()),
            allowed_tools: None,
            can_spawn: vec![],
            allowed_callers: vec![],
            max_iterations: 999,
            autonomous: false,
            presets: HashMap::new(),
            tool_profile: None,
            max_context_tokens: None,
            grants: HashMap::new(),
        };
        server.agents().upsert(stale.clone());

        // First hydrate: should pick up V1 from DB (haiku / 5).
        let input = stale.clone();
        let hydrated = server.hydrate_agent_from_db(input).await;
        assert_eq!(hydrated.default_model.as_deref(), Some("haiku"));
        assert_eq!(hydrated.max_iterations, 5);
        // And the registry reflects the live state too.
        assert_eq!(
            server
                .agents()
                .get("alpha")
                .unwrap()
                .default_model
                .as_deref(),
            Some("haiku")
        );

        // Write V2 to the DB.
        db.write_config(&AgentDbConfig {
            model: Some("sonnet".to_string()),
            max_iterations: Some(42),
            ..Default::default()
        })
        .await
        .unwrap();

        stale.default_model = Some("opus".to_string()); // re-enter with stale snapshot
        let hydrated_v2 = server.hydrate_agent_from_db(stale).await;
        assert_eq!(hydrated_v2.default_model.as_deref(), Some("sonnet"));
        assert_eq!(hydrated_v2.max_iterations, 42);
        assert_eq!(
            server
                .agents()
                .get("alpha")
                .unwrap()
                .default_model
                .as_deref(),
            Some("sonnet")
        );
    }

    #[tokio::test]
    async fn hydrate_returns_input_when_agent_not_in_index() {
        let (_instance, server, _registry) = server_fixture().await;

        // No DB for "phantom"; hydration should return the input unchanged.
        let input = crate::agent::Agent {
            name: "phantom".to_string(),
            system_prompt: String::new(),
            system_prompt_files: vec![],
            default_model: Some("ghost".to_string()),
            allowed_tools: None,
            can_spawn: vec![],
            allowed_callers: vec![],
            max_iterations: 7,
            autonomous: false,
            presets: HashMap::new(),
            tool_profile: None,
            max_context_tokens: None,
            grants: HashMap::new(),
        };
        let result = server.hydrate_agent_from_db(input.clone()).await;
        assert_eq!(result.name, "phantom");
        assert_eq!(result.default_model.as_deref(), Some("ghost"));
        assert_eq!(result.max_iterations, 7);
    }

    // -----------------------------------------------------------------
    // Agent-Owned Schedule integration tests (Stage 3)
    // -----------------------------------------------------------------
    //
    // These tests exercise `fire_agent_schedule` through the full plumbing
    // (session creation, agent attachment, schedule-fire audit, one-shot
    // cleanup, processing lock). The LLM call fails deterministically
    // (empty backend), which is fine — the plumbing around the call is
    // what we're testing.

    use crate::agent_db::Schedule;
    use crate::routine::{AgentSchedulePayload, Trigger};

    /// Create an agent DB, register it in the HostedIndex, seed its
    /// config, and return the DbEntry and AgentDb handle.
    async fn seed_agent(
        server: &Server,
        registry: &crate::session::SessionRegistry,
        name: &str,
    ) -> (DbEntry, crate::agent_db::AgentDb) {
        let (adb, pubkey) = {
            let mut user = registry.user_for_tests().await;
            create_agent_db(
                &mut user,
                name,
                &AgentDbConfig {
                    model: Some("test-model".to_string()),
                    ..Default::default()
                },
                &AgentMeta {
                    display_name: Some(name.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        };
        let entry = DbEntry {
            db_id: adb.id(),
            display_name: name.to_string(),
            pubkey,
        };
        server.agent_index().register(entry.clone());
        (entry, adb)
    }

    /// Build an `AgentSchedulePayload` for a Fresh (non-recurring) schedule.
    fn fresh_schedule_payload(
        owner_agent_db_id: &str,
        schedule_id: &str,
        prompt: &str,
    ) -> AgentSchedulePayload {
        AgentSchedulePayload {
            owner_agent_db_id: owner_agent_db_id.to_string(),
            schedule_id: schedule_id.to_string(),
            prompt: prompt.to_string(),
            target: serde_json::to_value(crate::agent_db::ScheduleTarget::Fresh).unwrap(),
            one_shot: true,
        }
    }

    /// Build an `AgentSchedulePayload` for a Pinned schedule.
    fn pinned_schedule_payload(
        owner_agent_db_id: &str,
        schedule_id: &str,
        prompt: &str,
        session_db_id: &str,
    ) -> AgentSchedulePayload {
        AgentSchedulePayload {
            owner_agent_db_id: owner_agent_db_id.to_string(),
            schedule_id: schedule_id.to_string(),
            prompt: prompt.to_string(),
            target: serde_json::to_value(crate::agent_db::ScheduleTarget::Pinned {
                session_db_id: session_db_id.to_string(),
            })
            .unwrap(),
            one_shot: true,
        }
    }

    #[tokio::test]
    async fn agent_schedule_host_check_skips_non_hosted() {
        let (_instance, server, registry) = server_fixture().await;

        // Create an agent DB but DON'T register it in the hosted index —
        // its ID is valid but find_by_id will return None.
        let (adb, _pubkey) = {
            let mut user = registry.user_for_tests().await;
            create_agent_db(
                &mut user,
                "ghost",
                &AgentDbConfig::default(),
                &AgentMeta {
                    display_name: Some("ghost".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        };
        let unhosted_id = adb.id().to_string();

        let payload = fresh_schedule_payload(&unhosted_id, "t1", "wake up");
        let result = server.fire_agent_schedule(payload).await;
        assert!(
            result.is_ok(),
            "host check should return Ok(()) — just skip: {result:?}"
        );
        // No sessions should have been created.
        let sessions = registry.list_sessions().await.unwrap_or_default();
        assert!(
            !sessions.iter().any(|s| {
                s.source
                    .as_deref()
                    .is_some_and(|src| src.contains("ghost") || src.contains("schedule:"))
            }),
            "no schedule session should exist for a non-hosted agent"
        );
    }

    #[tokio::test]
    async fn agent_schedule_fresh_creates_session_and_attaches_agent() {
        let (_instance, server, registry) = server_fixture().await;

        // Seed an agent.
        let (entry, adb) = seed_agent(&server, &registry, "alpha").await;

        // Add a schedule to the agent DB.
        adb.upsert_schedule(Schedule::new(
            "morning".to_string(),
            Trigger::OneShot {
                fire_at: chrono::Utc::now(),
            },
            "good morning".to_string(),
            crate::agent_db::ScheduleTarget::Fresh,
        ))
        .await
        .unwrap();

        let payload = fresh_schedule_payload(&entry.db_id.to_string(), "morning", "good morning");
        let result = server.fire_agent_schedule(payload).await;
        // LLM call fails (no backends), but the plumbing should succeed.
        // Errors from the LLM are propagated through the outcome.
        match result {
            Ok(()) => {} // if somehow it succeeded, that's fine too
            Err(e) => assert!(
                e.to_string().contains("No backends configured"),
                "expected backend error, got: {e}"
            ),
        }

        // Verify a Fresh session was created with the correct source tag.
        let sessions = registry.list_sessions().await.unwrap_or_default();
        let schedule_session = sessions
            .iter()
            .find(|s| {
                s.source
                    .as_deref()
                    .is_some_and(|src| src.starts_with("schedule:"))
            })
            .expect("a schedule session should exist");
        assert!(
            schedule_session
                .source
                .as_deref()
                .is_some_and(|s| s.contains("morning")),
            "session source should contain schedule id"
        );

        // Verify the agent is attached to the session.
        let (_conv, session_db) = registry
            .open_session(&schedule_session.session_db_id)
            .await
            .unwrap();
        let session = Session::new(
            ConversationId(schedule_session.session_db_id.clone()),
            session_db,
        )
        .await;
        let meta = session.read_meta().await;
        assert!(
            meta.agents.iter().any(|a| a.display_name == "alpha"),
            "agent should be attached to the fresh session: {:?}",
            meta.agents
        );

        // Verify ScheduleFire was recorded in the agent DB.
        let fires = adb.list_schedule_fires().await.unwrap();
        assert_eq!(fires.len(), 1, "one ScheduleFire should be recorded");
        let fire = &fires[0];
        assert_eq!(fire.schedule_id, "morning");
        assert!(fire.fresh, "should be marked as fresh");
        assert_eq!(
            fire.session_db_id, schedule_session.session_db_id,
            "fire should reference the created session"
        );

        // One-shot: schedule should be deleted.
        let remaining = adb.list_schedules().await.unwrap();
        assert!(
            remaining.is_empty(),
            "one-shot schedule should be deleted after fire, got {} schedules",
            remaining.len()
        );
    }

    #[tokio::test]
    async fn agent_schedule_pinned_reuses_existing_session() {
        let (_instance, server, registry) = server_fixture().await;

        // Seed an agent.
        let (entry, adb) = seed_agent(&server, &registry, "beta").await;

        // Create a session and attach the agent.
        let (_conv, session_db) = registry.create_session(Some("chat")).await.unwrap();
        let session_db_id = session_db.root_id().to_string();
        registry
            .attach_agent_to_session(&session_db_id, &entry)
            .await
            .unwrap();

        // Add a Pinned schedule targeting this session.
        adb.upsert_schedule(Schedule::new(
            "checkin".to_string(),
            Trigger::OneShot {
                fire_at: chrono::Utc::now(),
            },
            "checking in".to_string(),
            crate::agent_db::ScheduleTarget::Pinned {
                session_db_id: session_db_id.clone(),
            },
        ))
        .await
        .unwrap();

        let session_count_before = registry.list_sessions().await.unwrap_or_default().len();

        let payload = pinned_schedule_payload(
            &entry.db_id.to_string(),
            "checkin",
            "checking in",
            &session_db_id,
        );
        let result = server.fire_agent_schedule(payload).await;
        match result {
            Ok(()) => {}
            Err(e) => assert!(e.to_string().contains("No backends configured"), "{e}"),
        }

        // No new session should have been created.
        let session_count_after = registry.list_sessions().await.unwrap_or_default().len();
        assert_eq!(
            session_count_before, session_count_after,
            "Pinned fire should not create a new session"
        );

        // ScheduleFire should still be recorded.
        let fires = adb.list_schedule_fires().await.unwrap();
        assert_eq!(fires.len(), 1);
        assert!(!fires[0].fresh, "should NOT be marked as fresh");
        assert_eq!(fires[0].session_db_id, session_db_id);
    }

    #[tokio::test]
    async fn agent_schedule_processing_lock_skips_busy_session() {
        let (_instance, server, registry) = server_fixture().await;

        let (entry, _adb) = seed_agent(&server, &registry, "gamma").await;

        // Create a session and attach the agent.
        let (_conv, session_db) = registry.create_session(Some("chat")).await.unwrap();
        let session_db_id = session_db.root_id().to_string();
        registry
            .attach_agent_to_session(&session_db_id, &entry)
            .await
            .unwrap();

        // Manually insert the session into the processing set to simulate
        // a busy session.
        server.processing.lock().await.insert(session_db_id.clone());

        let payload =
            pinned_schedule_payload(&entry.db_id.to_string(), "t1", "wake", &session_db_id);
        let result = server.fire_agent_schedule(payload).await;
        assert!(result.is_ok(), "busy session should be skipped gracefully");

        // The lock should still be held (we inserted it manually).
        assert!(server.processing.lock().await.contains(&session_db_id));
        // Clean up.
        server.processing.lock().await.remove(&session_db_id);
    }

    #[tokio::test]
    async fn agent_schedule_records_fire_even_on_llm_failure() {
        let (_instance, server, registry) = server_fixture().await;

        let (entry, adb) = seed_agent(&server, &registry, "delta").await;

        let payload = fresh_schedule_payload(&entry.db_id.to_string(), "f1", "do thing");
        let _ = server.fire_agent_schedule(payload).await;

        // ScheduleFire should be recorded regardless of LLM outcome.
        let fires = adb.list_schedule_fires().await.unwrap();
        assert_eq!(
            fires.len(),
            1,
            "ScheduleFire should be recorded even on failure"
        );
        assert_eq!(fires[0].schedule_id, "f1");
        // Usage metadata will be None since the LLM call failed.
        assert!(fires[0].usage.is_none());
    }

    // ---- Home-peer gate ---------------------------------------------------

    fn make_agent_ref(db_id: &str, home: Option<&str>) -> crate::session::AgentRef {
        crate::session::AgentRef {
            db_id: db_id.to_string(),
            display_name: "x".to_string(),
            home_pubkey: home.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn is_home_returns_true_when_no_agent_ref_matches() {
        let (_inst, _server, registry) = server_fixture().await;
        let pk = registry.new_ephemeral_key("t").await.unwrap();
        let agents = vec![make_agent_ref("sha256:other", Some(&pk.to_string()))];
        assert!(is_home_for_agent_ref(&agents, "sha256:missing", &pk));
    }

    #[tokio::test]
    async fn is_home_returns_true_when_home_pubkey_unset_legacy() {
        let (_inst, _server, registry) = server_fixture().await;
        let pk = registry.new_ephemeral_key("t").await.unwrap();
        let agents = vec![make_agent_ref("sha256:agent", None)];
        assert!(is_home_for_agent_ref(&agents, "sha256:agent", &pk));
    }

    #[tokio::test]
    async fn is_home_returns_true_when_home_pubkey_matches_self() {
        let (_inst, _server, registry) = server_fixture().await;
        let pk = registry.new_ephemeral_key("t").await.unwrap();
        let agents = vec![make_agent_ref("sha256:agent", Some(&pk.to_string()))];
        assert!(is_home_for_agent_ref(&agents, "sha256:agent", &pk));
    }

    #[tokio::test]
    async fn is_home_returns_false_when_home_pubkey_is_another_peer() {
        let (_inst, _server, registry) = server_fixture().await;
        let me = registry.new_ephemeral_key("me").await.unwrap();
        let other = registry.new_ephemeral_key("other").await.unwrap();
        let agents = vec![make_agent_ref("sha256:agent", Some(&other.to_string()))];
        assert!(!is_home_for_agent_ref(&agents, "sha256:agent", &me));
    }

    #[tokio::test]
    async fn is_home_returns_true_on_corrupt_home_pubkey() {
        // Defensive: corrupt value yields legacy "any keyholder runs" rather
        // than silencing the agent permanently.
        let (_inst, _server, registry) = server_fixture().await;
        let pk = registry.new_ephemeral_key("t").await.unwrap();
        let agents = vec![make_agent_ref("sha256:agent", Some("not-a-pubkey"))];
        assert!(is_home_for_agent_ref(&agents, "sha256:agent", &pk));
    }

    #[tokio::test]
    async fn peer_is_home_for_returns_true_when_agent_not_in_index() {
        let (_inst, server, _registry) = server_fixture().await;
        // No agent registered. Any session/agent name should pass (the
        // resolver wouldn't have picked us either way).
        assert!(server.peer_is_home_for("sha256:any", "ghost").await);
    }

    #[tokio::test]
    async fn peer_is_home_for_returns_true_on_legacy_none_session() {
        let (_inst, server, registry) = server_fixture().await;
        let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
        let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
        let sid = session_db.root_id().to_string();
        // Insert an AgentRef with explicit None home (mimics a session that
        // predates this feature, or one created without using attach).
        crate::session::update_meta_on_db(&session_db, |m| {
            m.agents.push(crate::session::AgentRef {
                db_id: entry.db_id.to_string(),
                display_name: "alpha".to_string(),
                home_pubkey: None,
            });
        })
        .await
        .unwrap();
        assert!(server.peer_is_home_for(&sid, "alpha").await);
    }

    #[tokio::test]
    async fn peer_is_home_for_returns_true_when_home_matches_self() {
        let (_inst, server, registry) = server_fixture().await;
        let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
        let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
        let sid = session_db.root_id().to_string();
        // attach_agent_to_session defaults home_pubkey to the attacher's key.
        registry
            .attach_agent_to_session(&sid, &entry)
            .await
            .unwrap();
        assert!(server.peer_is_home_for(&sid, "alpha").await);
    }

    #[tokio::test]
    async fn peer_is_home_for_returns_false_when_home_is_another_peer() {
        let (_inst, server, registry) = server_fixture().await;
        let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
        let other = registry.new_ephemeral_key("other-peer").await.unwrap();
        let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
        let sid = session_db.root_id().to_string();
        crate::session::update_meta_on_db(&session_db, |m| {
            m.agents.push(crate::session::AgentRef {
                db_id: entry.db_id.to_string(),
                display_name: "alpha".to_string(),
                home_pubkey: Some(other.to_string()),
            });
        })
        .await
        .unwrap();
        assert!(!server.peer_is_home_for(&sid, "alpha").await);
    }

    // ---- process_session gate -------------------------------------------

    /// Register an Agent in the in-memory registry so resolve_agent_for_entry
    /// can return it. Mirrors the shape used in `hydrate_picks_up_db_config_edits`.
    fn register_alpha_agent_runtime(server: &Server) {
        server.agents().upsert(crate::agent::Agent {
            name: "alpha".to_string(),
            system_prompt: String::new(),
            system_prompt_files: vec![],
            default_model: Some("test-model".to_string()),
            allowed_tools: None,
            can_spawn: vec![],
            allowed_callers: vec![],
            max_iterations: 1,
            autonomous: false,
            presets: HashMap::new(),
            tool_profile: None,
            max_context_tokens: None,
            grants: HashMap::new(),
        });
    }

    async fn write_user_message(session_db: &eidetica::Database, sid: &str) {
        let mut session = crate::session::Session::new(
            crate::types::ConversationId(sid.to_string()),
            session_db.clone(),
        )
        .await;
        session
            .add_entry(crate::session::SessionEntry {
                sender: "user".to_string(),
                content: "hello".to_string(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
            })
            .await;
    }

    #[tokio::test]
    async fn process_session_skips_when_not_home_peer() {
        let (_inst, server, registry) = server_fixture().await;
        let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
        register_alpha_agent_runtime(&server);

        let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
        let sid = session_db.root_id().to_string();
        registry
            .attach_agent_to_session(&sid, &entry)
            .await
            .unwrap();

        // Pin home to a different peer so the gate fires.
        let other = registry.new_ephemeral_key("other-peer").await.unwrap();
        crate::session::update_meta_on_db(&session_db, |m| {
            m.agents[0].home_pubkey = Some(other.to_string());
        })
        .await
        .unwrap();

        let backend = crate::backends::BackendManager::new(
            &None,
            crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
        );
        server
            .register_session(&session_db, backend, Some("alpha".to_string()), None)
            .await
            .unwrap();

        write_user_message(&session_db, &sid).await;

        let entries_before = {
            let session = crate::session::Session::new(
                crate::types::ConversationId(sid.clone()),
                session_db.clone(),
            )
            .await;
            session.entries().len()
        };

        server.process_session(&sid).await.unwrap();

        // Gate released the lock inline before returning.
        assert!(!server.processing.lock().await.contains(&sid));

        let entries_after = {
            let session = crate::session::Session::new(
                crate::types::ConversationId(sid.clone()),
                session_db.clone(),
            )
            .await;
            session.entries().len()
        };
        assert_eq!(
            entries_before, entries_after,
            "non-home peer must not write any new entries"
        );
    }

    #[tokio::test]
    async fn process_session_runs_when_home_pubkey_unset_legacy() {
        let (_inst, server, registry) = server_fixture().await;
        let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
        register_alpha_agent_runtime(&server);

        let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
        let sid = session_db.root_id().to_string();
        registry
            .attach_agent_to_session(&sid, &entry)
            .await
            .unwrap();

        // Simulate a legacy session: clear the home_pubkey we just set on attach.
        crate::session::update_meta_on_db(&session_db, |m| {
            m.agents[0].home_pubkey = None;
        })
        .await
        .unwrap();

        let backend = crate::backends::BackendManager::new(
            &None,
            crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
        );
        server
            .register_session(&session_db, backend, Some("alpha".to_string()), None)
            .await
            .unwrap();

        write_user_message(&session_db, &sid).await;

        server.process_session(&sid).await.unwrap();

        // Gate passed → spawn_agent_task was called → spawned tokio task
        // is pending on current_thread runtime; lock is still held.
        assert!(server.processing.lock().await.contains(&sid));
    }

    #[tokio::test]
    async fn process_session_runs_when_home_matches_self() {
        let (_inst, server, registry) = server_fixture().await;
        let (entry, _adb) = seed_agent(&server, &registry, "alpha").await;
        register_alpha_agent_runtime(&server);

        let (_conv, session_db) = registry.create_session(Some("t")).await.unwrap();
        let sid = session_db.root_id().to_string();
        // attach defaults home_pubkey to this peer's key on alpha.
        registry
            .attach_agent_to_session(&sid, &entry)
            .await
            .unwrap();

        let backend = crate::backends::BackendManager::new(
            &None,
            crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
        );
        server
            .register_session(&session_db, backend, Some("alpha".to_string()), None)
            .await
            .unwrap();

        write_user_message(&session_db, &sid).await;

        server.process_session(&sid).await.unwrap();

        assert!(server.processing.lock().await.contains(&sid));
    }

    // ---- fire_agent_schedule gate ---------------------------------------

    #[tokio::test]
    async fn fire_fresh_skips_when_agent_home_is_another_peer() {
        let (_instance, server, registry) = server_fixture().await;
        let (entry, adb) = seed_agent(&server, &registry, "alpha").await;

        // Overwrite the agent-level home_pubkey to a foreign key.
        let other = registry.new_ephemeral_key("other-peer").await.unwrap();
        crate::db_kind::write_agent_home_pubkey(adb.database(), &other)
            .await
            .unwrap();

        let sessions_before = registry.list_sessions().await.unwrap_or_default().len();
        let payload = fresh_schedule_payload(&entry.db_id.to_string(), "f1", "do the thing");
        let result = server.fire_agent_schedule(payload).await;
        assert!(result.is_ok(), "skip path returns Ok: {result:?}");

        // No new Fresh session should have been created.
        let sessions_after = registry.list_sessions().await.unwrap_or_default().len();
        assert_eq!(sessions_before, sessions_after);

        // No ScheduleFire recorded (the gate fires before any of the
        // schedule-fire bookkeeping runs).
        let fires = adb.list_schedule_fires().await.unwrap();
        assert!(fires.is_empty(), "non-home gate should not record a fire");
    }

    #[tokio::test]
    async fn fire_fresh_runs_when_agent_home_is_unset_legacy() {
        let (_instance, server, registry) = server_fixture().await;
        let (entry, adb) = seed_agent(&server, &registry, "alpha").await;

        // Mimic a pre-feature agent DB by clearing the home_pubkey written
        // at create time.
        crate::db_kind::clear_agent_home_pubkey(adb.database())
            .await
            .unwrap();

        let payload = fresh_schedule_payload(&entry.db_id.to_string(), "f1", "wake");
        let _ = server.fire_agent_schedule(payload).await;
        // Even with the LLM call failing (no backends), a ScheduleFire is
        // recorded — which only happens when we get past the gate.
        let fires = adb.list_schedule_fires().await.unwrap();
        assert_eq!(fires.len(), 1, "legacy None must let the fire through");
    }

    #[tokio::test]
    async fn fire_pinned_skips_when_session_home_is_another_peer() {
        let (_instance, server, registry) = server_fixture().await;
        let (entry, adb) = seed_agent(&server, &registry, "alpha").await;

        let (_conv, session_db) = registry.create_session(Some("chat")).await.unwrap();
        let sid = session_db.root_id().to_string();
        registry
            .attach_agent_to_session(&sid, &entry)
            .await
            .unwrap();

        // Rewrite the AgentRef's home_pubkey to another peer.
        let other = registry.new_ephemeral_key("other-peer").await.unwrap();
        crate::session::update_meta_on_db(&session_db, |m| {
            m.agents[0].home_pubkey = Some(other.to_string());
        })
        .await
        .unwrap();

        let payload =
            pinned_schedule_payload(&entry.db_id.to_string(), "p1", "wake", &sid);
        let result = server.fire_agent_schedule(payload).await;
        assert!(result.is_ok(), "skip path returns Ok: {result:?}");

        // No ScheduleFire — gate fires before bookkeeping.
        let fires = adb.list_schedule_fires().await.unwrap();
        assert!(
            fires.is_empty(),
            "non-home pinned fire should not record a fire"
        );
    }
}
