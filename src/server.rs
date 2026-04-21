//! Callback-driven agent server.
//!
//! The server watches session databases for new entries via eidetica's
//! `on_local_write` callbacks. When a new message from a non-agent sender
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
use crate::agent_index::AgentIndex;
use crate::backends::BackendManager;
use crate::config::ContextConfig;
use crate::context::ContextBuilder;
use crate::gateway::ApprovalExchange;
use crate::runtime;
use crate::security::SecurityContext;
use crate::session::{EntryType, Session, SessionEntry, SessionRegistry};
use crate::tool::{ScopedTools, ToolContext, ToolPolicyRegistry, ToolProfile, ToolRegistry};
use crate::types::ConversationId;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::{error, info};

/// Maximum number of concurrent LLM calls across all conversations.
const MAX_CONCURRENT_LLM_CALLS: usize = 10;

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

/// Callback-driven agent server.
pub struct Server {
    registry: Arc<SessionRegistry>,
    agents: Arc<AgentRegistry>,
    agent_index: AgentIndex,
    tools: Arc<ToolRegistry>,
    policies: Arc<ToolPolicyRegistry>,
    security: SecurityContext,
    semaphore: Arc<Semaphore>,
    tool_profiles: HashMap<String, ToolProfile>,
    context_config: ContextConfig,
    /// Per-session runtime state keyed by session_db_id
    sessions: Arc<Mutex<HashMap<String, SessionRuntime>>>,
    /// Track which session DBs have server callbacks registered
    watched: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Sessions currently being processed (prevents concurrent agent runs per session)
    processing: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Internal notification channel — callbacks send session_db_id here
    notify_tx: mpsc::Sender<String>,
}

impl Server {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<SessionRegistry>,
        agents: Arc<AgentRegistry>,
        agent_index: AgentIndex,
        tools: Arc<ToolRegistry>,
        policies: Arc<ToolPolicyRegistry>,
        security: SecurityContext,
        tool_profiles: HashMap<String, ToolProfile>,
        context_config: ContextConfig,
    ) -> Arc<Self> {
        let (notify_tx, notify_rx) = mpsc::channel(256);

        let server = Arc::new(Self {
            registry,
            agents,
            agent_index,
            tools,
            policies,
            security,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_LLM_CALLS)),
            tool_profiles,
            context_config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            watched: Arc::new(Mutex::new(std::collections::HashSet::new())),
            processing: Arc::new(Mutex::new(std::collections::HashSet::new())),
            notify_tx,
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

    pub fn agent_index(&self) -> &AgentIndex {
        &self.agent_index
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
        let Ok(Some(entry)) = self.agent_index.find_by_name(&agent.name).await else {
            return agent;
        };
        let Ok(Some(db)) = self.registry.open_agent_db(&entry.db_id).await else {
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

    /// Register a session for callback-driven agent processing.
    ///
    /// Installs an `on_local_write` callback on the session DB (if not already
    /// present) that triggers agent processing when new non-agent messages or
    /// directives appear. Stores per-session runtime state (backend, agent
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
        session_db.on_local_write(move |_entry, _db, _instance| {
            let tx = tx.clone();
            let sid = sid.clone();
            Box::pin(async move {
                let _ = tx.send(sid).await;
                Ok(())
            })
        })?;

        info!(session_db_id = %session_db_id, "Server watching session");
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
            session_db.on_local_write(move |_entry, _db, _instance| {
                let tx = tx.clone();
                let sid = sid.clone();
                Box::pin(async move {
                    let _ = tx.send(sid).await;
                    Ok(())
                })
            })?;

            info!(
                session_db_id = %session_db_id,
                agent = %agent_name,
                "Server watching child session"
            );
        } else {
            drop(watched);
        }

        Ok((conversation_id, session_db, completion_rx))
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

        let should_process = match latest.entry_type {
            EntryType::Message => self.agents.get(&latest.sender).is_none(),
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

        let agent = self
            .registry
            .resolve_agent_for_entry(
                session_db_id,
                agent_override.as_deref(),
                &self.agent_index,
                Some(&latest.content),
            )
            .await;

        // Stage 8 live hydration: if the resolved agent has a Living Agent DB
        // on this peer, rebuild its runtime snapshot from the DB's `config`
        // store so edits to the DB (local or synced from origin peer)
        // propagate to the next run without a restart.
        let agent = self.hydrate_agent_from_db(agent).await;

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
        let default_role = agent.default_role.clone();
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
        let max_context_tokens = agent.max_context_tokens;

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
                })
                .await;
            }

            let request_security = SecurityContext {
                leak_detector: security.leak_detector.clone(),
                auto_approved_tools: security.auto_approved_tools.clone(),
                approval_callback: approval_tx,
            };

            let scoped_tools = match spawn.parent_tools {
                Some(parent) => parent.narrow(allowed_tools.as_deref()),
                None => ScopedTools::new(tools, allowed_tools),
            };

            let tool_ctx = ToolContext {
                agent_name: agent_name.clone(),
                call_depth: spawn.call_depth,
                max_call_depth,
                tools: scoped_tools,
                profile,
                session: session.clone(),
                grants: Default::default(),
                agent_grants,
            };

            let tool_defs = tool_ctx.tools.definitions(&tool_ctx.profile);
            let assembled = {
                let s = session.lock().await;
                ContextBuilder::new(s.entries(), &agent_name, &context_config)
                    .with_role(default_role.as_ref())
                    .with_tools(&tool_defs)
                    .with_max_tokens_override(max_context_tokens)
                    .build()
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
                                let truncated = if output.len() > 500 {
                                    format!("{}…", &output[..500])
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
            )
            .await;

            let _ = event_writer.await;

            drop(_permit);

            let mut s = session.lock().await;
            match result {
                Ok(body) => {
                    s.add_entry(SessionEntry {
                        sender: agent_name,
                        content: body,
                        timestamp: Utc::now(),
                        entry_type: EntryType::Message,
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
    use crate::agent_db::{create_agent_db, AgentDbConfig, AgentMeta};
    use crate::agent_index::AgentIndexEntry;
    use crate::config::Config;
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;

    fn blank_config() -> Config {
        Config {
            homeserver_url: String::new(),
            username: String::new(),
            password: None,
            allow_list: None,
            message_limit: None,
            room_size_limit: None,
            state_dir: None,
            chat_summary_model: None,
            role: None,
            roles: None,
            backends: None,
            agents: None,
            security: None,
            schedules: None,
            mcp_servers: None,
            tool_profiles: None,
            mcp_server_dir: None,
            context: None,
        }
    }

    /// Build a Server with the minimum wiring needed to exercise hydration.
    async fn server_fixture() -> (Instance, Arc<Server>, Arc<crate::session::SessionRegistry>) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents = Arc::new(AgentRegistry::from_config(&blank_config()));
        let registry = Arc::new(
            crate::session::SessionRegistry::new(instance.clone(), user, agents.clone())
                .await
                .unwrap(),
        );
        let index = AgentIndex::new(registry.central_db().clone());
        let tools = Arc::new(ToolRegistry::new());
        let policies = Arc::new(crate::tool::ToolPolicyRegistry::empty());
        let security = SecurityContext {
            leak_detector: crate::security::LeakDetector::new(
                crate::security::LeakPolicy::default(),
            ),
            auto_approved_tools: std::collections::HashSet::new(),
            approval_callback: None,
        };
        let server = Server::new(
            registry.clone(),
            agents,
            index,
            tools,
            policies,
            security,
            HashMap::new(),
            Default::default(),
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
        server
            .agent_index()
            .register(AgentIndexEntry {
                db_id: db.id(),
                display_name: "alpha".to_string(),
                pubkey,
            })
            .await
            .unwrap();

        // Seed the in-memory registry with a stale entry (model="opus", iter=999)
        // — exactly what would happen if yaml drifted from DB, or if a prior
        // hydration happened before a DB edit.
        let mut stale = crate::agent::Agent {
            name: "alpha".to_string(),
            default_role: None,
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
            default_role: None,
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
}
