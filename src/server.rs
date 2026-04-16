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

/// Per-session metadata needed for agent processing.
struct SessionMeta {
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
///
/// Watches session databases for new entries. When a non-agent message appears,
/// spawns an agent task. Transport-agnostic — gateways handle their own response
/// delivery by registering callbacks on session DBs.
pub struct Server {
    registry: Arc<SessionRegistry>,
    agents: Arc<AgentRegistry>,
    tools: Arc<ToolRegistry>,
    policies: Arc<ToolPolicyRegistry>,
    security: SecurityContext,
    semaphore: Arc<Semaphore>,
    /// Named tool profiles from config
    tool_profiles: HashMap<String, ToolProfile>,
    /// Context window management config
    context_config: ContextConfig,
    /// Per-session metadata (backend, agent override, approval channel)
    sessions: Arc<Mutex<HashMap<String, SessionMeta>>>,
    /// Track which session DBs have server callbacks registered
    watched: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Sessions currently being processed (prevents concurrent agent runs per session)
    processing: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Internal notification channel — callbacks send transport_id here
    notify_tx: mpsc::Sender<String>,
}

impl Server {
    pub fn new(
        registry: Arc<SessionRegistry>,
        agents: Arc<AgentRegistry>,
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

        // Spawn the processing loop
        let server_clone = server.clone();
        tokio::spawn(async move {
            server_clone.processing_loop(notify_rx).await;
        });

        server
    }

    /// Get the session registry
    pub fn registry(&self) -> &SessionRegistry {
        &self.registry
    }

    /// Get a shared Arc handle to the session registry
    pub fn registry_arc(&self) -> Arc<SessionRegistry> {
        self.registry.clone()
    }

    /// Get the agent registry
    pub fn agents(&self) -> &AgentRegistry {
        &self.agents
    }

    /// Register a session for callback-driven agent processing.
    ///
    /// Call this when a gateway first encounters a transport (e.g., a Matrix room).
    /// Registers an `on_local_write` callback on the session database that triggers
    /// agent processing when new non-agent messages appear.
    ///
    /// Gateways should register their own callbacks on the session DB to handle
    /// response delivery (e.g., sending agent messages to a Matrix room).
    ///
    /// Safe to call multiple times — updates metadata, skips duplicate callback registration.
    pub async fn register_session(
        &self,
        transport_id: &str,
        session_db: &eidetica::Database,
        backend: BackendManager,
        agent_override: Option<String>,
        approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
    ) -> anyhow::Result<()> {
        // Update session metadata
        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(
                transport_id.to_string(),
                SessionMeta {
                    backend,
                    agent_override,
                    approval_tx,
                    call_depth: 0,
                    max_call_depth: 0, // 0 = use agent's own max_iterations
                    parent_tools: None,
                    completion_tx: None,
                },
            );
        }

        // Register server callback if not already done for this session DB
        let db_id = session_db.root_id().to_string();
        let mut watched = self.watched.lock().await;
        if watched.contains(&db_id) {
            return Ok(());
        }
        watched.insert(db_id);
        drop(watched);

        let tx = self.notify_tx.clone();
        let tid = transport_id.to_string();
        session_db.on_local_write(move |_entry, _db, _instance| {
            let tx = tx.clone();
            let tid = tid.clone();
            Box::pin(async move {
                let _ = tx.send(tid).await;
                Ok(())
            })
        })?;

        info!("Server watching session DB for {}", transport_id);
        Ok(())
    }

    /// Register a child session for agent-to-agent communication.
    ///
    /// Creates a new session DB via the registry, registers it for callback-driven
    /// processing, and returns the session info plus a completion receiver. The caller
    /// writes a Directive entry to trigger agent execution, then awaits the receiver
    /// for the response.
    pub async fn register_child_session(
        &self,
        agent_name: &str,
        backend: BackendManager,
        approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
        call_depth: usize,
        max_call_depth: usize,
        parent_tools: ScopedTools,
    ) -> anyhow::Result<(
        String,
        ConversationId,
        eidetica::Database,
        mpsc::Receiver<()>,
    )> {
        let transport_id = format!("spawn:{}", uuid::Uuid::new_v4());

        let (conversation_id, session_db) = self
            .registry
            .get_or_create_session_db(&transport_id)
            .await?;

        let (completion_tx, completion_rx) = mpsc::channel(1);

        // Store metadata with spawn context
        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(
                transport_id.clone(),
                SessionMeta {
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

        // Register on_local_write callback for server processing
        let db_id = session_db.root_id().to_string();
        let mut watched = self.watched.lock().await;
        if !watched.contains(&db_id) {
            watched.insert(db_id);
            drop(watched);

            let tx = self.notify_tx.clone();
            let tid = transport_id.clone();
            session_db.on_local_write(move |_entry, _db, _instance| {
                let tx = tx.clone();
                let tid = tid.clone();
                Box::pin(async move {
                    let _ = tx.send(tid).await;
                    Ok(())
                })
            })?;

            info!(
                "Server watching child session DB for {} (agent: {})",
                transport_id, agent_name
            );
        } else {
            drop(watched);
        }

        Ok((transport_id, conversation_id, session_db, completion_rx))
    }

    /// Main processing loop. Receives notifications from callbacks and processes sessions.
    async fn processing_loop(&self, mut notify_rx: mpsc::Receiver<String>) {
        while let Some(transport_id) = notify_rx.recv().await {
            // Debounce: drain any pending notifications, dedup
            let mut to_process = vec![transport_id];
            while let Ok(tid) = notify_rx.try_recv() {
                if !to_process.contains(&tid) {
                    to_process.push(tid);
                }
            }

            for transport_id in to_process {
                if let Err(e) = self.process_session(&transport_id).await {
                    error!("Error processing session {}: {e}", transport_id);
                }
            }
        }
    }

    /// Check a session for new entries and act on them.
    ///
    /// Skips processing if an agent task is already running for this session,
    /// preventing duplicate responses from concurrent writes.
    async fn process_session(&self, transport_id: &str) -> anyhow::Result<()> {
        let (conversation_id, session_db) =
            self.registry.get_or_create_session_db(transport_id).await?;

        let session = Session::new(conversation_id.clone(), session_db.clone()).await;

        let latest = match session.latest_entry() {
            Some(e) => e.clone(),
            None => return Ok(()),
        };

        // Determine if this entry should trigger agent execution:
        // - Message from a non-agent sender (user input)
        // - Directive from any sender (spawn_agent, scheduler, system)
        let should_process = match latest.entry_type {
            EntryType::Message => self.agents.get(&latest.sender).is_none(),
            EntryType::Directive => true,
            _ => false,
        };
        if !should_process {
            return Ok(());
        }

        // Per-session serialization: skip if an agent task is already running
        {
            let mut processing = self.processing.lock().await;
            if !processing.insert(transport_id.to_string()) {
                // Already processing — the running task will see the new entry
                return Ok(());
            }
        }
        let (backend, agent_override, approval_tx, spawn_ctx) = {
            let sessions = self.sessions.lock().await;
            match sessions.get(transport_id) {
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
                None => return Ok(()),
            }
        };

        let agent = self
            .registry
            .resolve_agent(transport_id, agent_override.as_deref())
            .await;

        self.spawn_agent_task(
            transport_id.to_string(),
            session,
            agent,
            approval_tx,
            backend,
            spawn_ctx,
        )
        .await;

        Ok(())
    }

    /// Spawn a tokio task to run an agent's ReAct loop.
    async fn spawn_agent_task(
        &self,
        transport_id: String,
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
        // Use override if set, otherwise fall back to agent's own max_iterations
        let max_call_depth = if spawn.max_call_depth > 0 {
            spawn.max_call_depth
        } else {
            agent.max_iterations as usize
        };

        // Resolve tool profile: agent default (preset resolution happens at spawn_agent level)
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

            // Write Ack entry so clients show "thinking..." immediately
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

            // Build tool scope: if parent provided a narrowed scope, narrow further
            // with this agent's allowed_tools. Otherwise use agent defaults.
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
            };

            // Build context using ContextBuilder (token-budgeted)
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

            // Set up event sink for ToolCall/ToolResult audit trail
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

            // event_tx was moved into runtime::execute and dropped on return,
            // so event_writer will drain remaining events and exit
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
                    error!("Agent error for {}: {err}", transport_id);
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

            // Clear per-session processing lock
            {
                let mut proc = spawn.processing.lock().await;
                proc.remove(&transport_id);
            }

            // Signal completion for synchronous callers (e.g., spawn_agent)
            if let Some(tx) = spawn.completion_tx {
                let _ = tx.send(()).await;
            }
        });
    }
}
