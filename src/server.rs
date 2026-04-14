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
//! **Known limitation**: concurrent writes to the same session may cause
//! duplicate agent runs. Each callback independently checks the latest
//! entry — no per-session locking. Acceptable for now.

use crate::agent::AgentRegistry;
use crate::backends::BackendManager;
use crate::gateway::ApprovalExchange;
use crate::runtime;
use crate::security::SecurityContext;
use crate::session::{EntryType, Session, SessionEntry, SessionRegistry};
use crate::tool::{ScopedTools, ToolContext, ToolPolicyRegistry, ToolRegistry};
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
    /// Per-session metadata (backend, agent override, approval channel)
    sessions: Arc<Mutex<HashMap<String, SessionMeta>>>,
    /// Track which session DBs have server callbacks registered
    watched: Arc<Mutex<std::collections::HashSet<String>>>,
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
    ) -> Arc<Self> {
        let (notify_tx, notify_rx) = mpsc::channel(256);

        let server = Arc::new(Self {
            registry,
            agents,
            tools,
            policies,
            security,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_LLM_CALLS)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            watched: Arc::new(Mutex::new(std::collections::HashSet::new())),
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
    async fn process_session(&self, transport_id: &str) -> anyhow::Result<()> {
        let (conversation_id, session_db) = self
            .registry
            .get_or_create_session_db(transport_id)
            .await?;

        let session = Session::new(conversation_id.clone(), session_db.clone()).await;

        let latest = match session.latest_entry() {
            Some(e) => e.clone(),
            None => return Ok(()),
        };

        // Only act on Messages from non-agent senders
        if latest.entry_type != EntryType::Message {
            return Ok(());
        }

        // Check if sender is a known agent — if so, ignore (it's a response, not a request)
        if self.agents.get(&latest.sender).is_some() {
            return Ok(());
        }

        // User message — spawn agent task
        let meta = {
            let sessions = self.sessions.lock().await;
            match sessions.get(transport_id) {
                Some(m) => (
                    m.backend.clone(),
                    m.agent_override.clone(),
                    m.approval_tx.clone(),
                ),
                None => return Ok(()),
            }
        };

        let agent = self
            .registry
            .resolve_agent(transport_id, meta.1.as_deref())
            .await;

        self.spawn_agent_task(
            transport_id.to_string(),
            session,
            agent,
            meta.0,
            meta.2,
        )
        .await;

        Ok(())
    }

    /// Spawn a tokio task to run an agent's ReAct loop.
    async fn spawn_agent_task(
        &self,
        transport_id: String,
        mut session: Session,
        agent: crate::agent::Agent,
        backend: BackendManager,
        approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
    ) {
        let agent_name = agent.name.clone();
        let default_role = agent.default_role.clone();
        let default_model = agent.default_model.clone();
        let allowed_tools = agent.allowed_tools.clone();
        let max_call_depth = agent.max_iterations as usize;

        let tools = self.tools.clone();
        let policies = self.policies.clone();
        let security = self.security.clone();
        let semaphore = self.semaphore.clone();

        tokio::spawn(async move {
            let _permit = semaphore.acquire().await.expect("semaphore closed");

            let context = session.build_context(&agent_name, default_role, default_model);

            let request_security = SecurityContext {
                leak_detector: security.leak_detector.clone(),
                auto_approved_tools: security.auto_approved_tools.clone(),
                approval_callback: approval_tx,
            };

            let tool_ctx = ToolContext {
                agent_name: agent_name.clone(),
                call_depth: 0,
                max_call_depth,
                tools: ScopedTools::new(tools, allowed_tools),
            };

            let result = runtime::execute(
                &context,
                &backend,
                &request_security,
                &tool_ctx,
                &policies,
            )
            .await;

            drop(_permit);

            match result {
                Ok(body) => {
                    session
                        .add_entry(SessionEntry {
                            sender: agent_name,
                            content: body,
                            timestamp: Utc::now(),
                            entry_type: EntryType::Message,
                        })
                        .await;
                }
                Err(err) => {
                    error!("Agent error for {}: {err}", transport_id);
                    session
                        .add_entry(SessionEntry {
                            sender: agent_name,
                            content: format!("Error: {err}"),
                            timestamp: Utc::now(),
                            entry_type: EntryType::Error,
                        })
                        .await;
                }
            }
        });
    }
}
