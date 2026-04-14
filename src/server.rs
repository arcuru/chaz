//! Callback-driven agent server.
//!
//! The server watches session databases for new entries via eidetica's
//! `on_local_write` callbacks. When a new user message is detected, it
//! spawns an agent task that runs the ReAct loop and writes the response
//! back to the session database. Responses are delivered to transports
//! (Matrix rooms, TUI) through a response channel.
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
use crate::tool::{ToolContext, ToolRegistry};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::{error, info};

/// Maximum number of concurrent LLM calls across all conversations.
const MAX_CONCURRENT_LLM_CALLS: usize = 10;

/// A response to deliver to a transport (Matrix room, TUI, etc.)
pub struct ResponseDelivery {
    pub transport_id: String,
    pub body: String,
    pub is_markdown: bool,
}

/// Per-session metadata needed for agent processing.
struct SessionMeta {
    backend: BackendManager,
    agent_override: Option<String>,
    approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
}

/// Callback-driven agent server.
///
/// Watches session databases for new entries. When a user message appears,
/// spawns an agent task. When an agent response appears, delivers it to the
/// transport via the response channel.
pub struct Server {
    registry: Arc<SessionRegistry>,
    agents: Arc<AgentRegistry>,
    tools: Arc<ToolRegistry>,
    security: SecurityContext,
    response_tx: mpsc::Sender<ResponseDelivery>,
    /// Response receiver — gateways take this to deliver responses
    response_rx: Mutex<Option<mpsc::Receiver<ResponseDelivery>>>,
    semaphore: Arc<Semaphore>,
    /// Per-session metadata (backend, agent override, approval channel)
    sessions: Arc<Mutex<HashMap<String, SessionMeta>>>,
    /// Track which session DBs have callbacks registered
    watched: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Internal notification channel — callbacks send transport_id here
    notify_tx: mpsc::Sender<String>,
}

impl Server {
    /// Create a new server. Returns the server and a response receiver
    /// that transports should read from to deliver responses.
    pub fn new(
        registry: Arc<SessionRegistry>,
        agents: Arc<AgentRegistry>,
        tools: Arc<ToolRegistry>,
        security: SecurityContext,
    ) -> Arc<Self> {
        let (response_tx, response_rx) = mpsc::channel(256);
        let (notify_tx, notify_rx) = mpsc::channel(256);

        let server = Arc::new(Self {
            registry,
            agents,
            tools,
            security,
            response_tx,
            response_rx: Mutex::new(Some(response_rx)),
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

    /// Take the response receiver. Gateways call this to get the channel
    /// for delivering responses to transports. Can only be called once.
    pub async fn take_response_rx(&self) -> mpsc::Receiver<ResponseDelivery> {
        self.response_rx
            .lock()
            .await
            .take()
            .expect("response_rx already taken")
    }

    /// Register a session for callback-driven processing.
    ///
    /// Call this when a gateway first encounters a transport (e.g., a Matrix room).
    /// Registers an `on_local_write` callback on the session database and stores
    /// the transport-specific metadata (backend, approval channel).
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

        // Register callback if not already done for this session DB
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
                // Just notify — the processing loop does the heavy lifting
                let _ = tx.send(tid).await;
                Ok(())
            })
        })?;

        info!("Watching session DB for {}", transport_id);
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
            None => return Ok(()), // Empty session
        };

        let agent = self
            .registry
            .resolve_agent(transport_id, None)
            .await;

        match latest.entry_type {
            EntryType::Message if latest.sender != agent.name => {
                // User message — spawn agent task
                let meta = {
                    let sessions = self.sessions.lock().await;
                    match sessions.get(transport_id) {
                        Some(m) => (
                            m.backend.clone(),
                            m.agent_override.clone(),
                            m.approval_tx.clone(),
                        ),
                        None => return Ok(()), // Not registered
                    }
                };

                // Re-resolve agent with override
                let agent = self
                    .registry
                    .resolve_agent(transport_id, meta.1.as_deref())
                    .await;

                self.spawn_agent_task(
                    transport_id.to_string(),
                    session,
                    session_db,
                    agent,
                    meta.0,
                    meta.2,
                )
                .await;
            }
            EntryType::Message if latest.sender == agent.name => {
                // Agent response — deliver to transport
                let _ = self
                    .response_tx
                    .send(ResponseDelivery {
                        transport_id: transport_id.to_string(),
                        body: latest.content,
                        is_markdown: true,
                    })
                    .await;
            }
            _ => {} // Ack, Error — no action needed
        }

        Ok(())
    }

    /// Spawn a tokio task to run an agent's ReAct loop.
    async fn spawn_agent_task(
        &self,
        transport_id: String,
        mut session: Session,
        session_db: eidetica::Database,
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
        let agents = self.agents.clone();
        let security = self.security.clone();
        let semaphore = self.semaphore.clone();

        tokio::spawn(async move {
            let _permit = semaphore.acquire().await.expect("semaphore closed");

            let context = session.build_context(&agent_name, default_role, default_model);

            let filtered = tools.filtered_view(allowed_tools.as_deref());

            let request_security = SecurityContext {
                leak_detector: security.leak_detector.clone(),
                auto_approved_tools: security.auto_approved_tools.clone(),
                approval_callback: approval_tx,
            };

            let tool_ctx = ToolContext {
                agent_name: agent_name.clone(),
                call_depth: 0,
                max_call_depth,
                agent_registry: agents,
                tool_registry: tools.clone(),
                backend: backend.clone(),
                security: request_security.clone(),
                database: session_db,
            };

            let result =
                runtime::execute(&context, &backend, &filtered, &request_security, &tool_ctx)
                    .await;

            drop(_permit);

            match result {
                Ok(body) => {
                    // Write response to session DB — this triggers the callback,
                    // which will detect the agent message and deliver it to the transport.
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
