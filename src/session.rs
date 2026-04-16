use crate::agent::{Agent, AgentRegistry};
use crate::types::ConversationId;

use chrono::{DateTime, Utc};
use eidetica::store::Table;
use eidetica::Database;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info};

/// Type of session entry. Participants (users and agents alike) write entries
/// to a session. There is no user/agent distinction at the protocol level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntryType {
    /// A chat message (from any participant)
    Message,
    /// A task/instruction from a non-user source (spawn_agent, scheduler, system).
    /// Included in LLM context as a user message.
    Directive,
    /// Record of a tool invocation (audit trail). Excluded from LLM context.
    ToolCall,
    /// Record of a tool result (audit trail). Excluded from LLM context.
    ToolResult,
    /// Acknowledgement that work is in progress
    Ack,
    /// An error that occurred during processing
    Error,
    /// A compacted summary of older messages, written by /compact or the compact tool.
    /// Context builder treats the most recent Summary as the start boundary.
    Summary,
}

/// An entry in a session. Participants (human users and AI agents) are
/// treated identically — both write SessionEntries with their name as sender.
/// The agent determines assistant vs user roles at context-building time by
/// comparing the sender to its own name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub sender: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub entry_type: EntryType,
}

/// A binding record stored in the central registry database.
/// Maps a transport ID (e.g., Matrix room ID) to a session database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBinding {
    pub transport_id: String,
    pub conversation_id: String,
    /// Eidetica database root ID for this session's DB
    pub session_db_id: String,
    /// Agent name bound to this conversation, if any
    pub agent_name: Option<String>,
    /// Human-friendly alias for this session (e.g., "daily-standup")
    #[serde(default)]
    pub name: Option<String>,
    /// Model override for this session
    #[serde(default)]
    pub model: Option<String>,
    /// Role name for this session (references a config/built-in role, or a custom one)
    #[serde(default)]
    pub role_name: Option<String>,
    /// Custom role prompt (when role_name is a session-defined role, not a config one)
    #[serde(default)]
    pub role_prompt: Option<String>,
    /// Backend name override for this session
    #[serde(default)]
    pub backend_name: Option<String>,
    /// Backend API base URL (for session-defined backends)
    #[serde(default)]
    pub backend_url: Option<String>,
    /// SecretStore reference for the backend API key (for session-defined backends)
    #[serde(default)]
    pub backend_key_ref: Option<String>,
}

/// Per-conversation state backed by its own eidetica Database.
///
/// Each session owns a dedicated eidetica Database containing a single
/// `Table<SessionEntry>` store. Entries are loaded from the DB on
/// creation and kept in memory for context building.
pub struct Session {
    pub conversation_id: ConversationId,
    database: Database,
    entries: Vec<SessionEntry>,
    /// Store name — "entries" for regular, "entries:{id}" for ephemeral
    store_name: String,
}

impl Session {
    /// Open a session, loading existing entries from its database.
    pub async fn new(conversation_id: ConversationId, database: Database) -> Self {
        let mut session = Session {
            conversation_id,
            database,
            entries: Vec::new(),
            store_name: "entries".to_string(),
        };

        session.load_from_db().await;
        session
    }

    /// Load entries from eidetica
    async fn load_from_db(&mut self) {
        let Ok(txn) = self.database.new_transaction().await else {
            return;
        };
        if let Ok(store) = txn.get_store::<Table<SessionEntry>>(&self.store_name).await {
            match store.search(|_| true).await {
                Ok(records) => {
                    let mut entries: Vec<SessionEntry> =
                        records.into_iter().map(|(_, entry)| entry).collect();
                    entries.sort_by_key(|e| e.timestamp);
                    self.entries = entries;
                }
                Err(e) => error!("Failed to load session entries from eidetica: {e}"),
            }
        }
    }

    /// Add an entry to the session with persistence
    pub async fn add_entry(&mut self, entry: SessionEntry) {
        match self.database.new_transaction().await {
            Ok(txn) => match txn.get_store::<Table<SessionEntry>>(&self.store_name).await {
                Ok(store) => {
                    if let Err(e) = store.insert(entry.clone()).await {
                        error!("Failed to persist entry to eidetica: {e}");
                    } else if let Err(e) = txn.commit().await {
                        error!("Failed to commit to eidetica: {e}");
                    }
                }
                Err(e) => error!("Failed to open eidetica store: {e}"),
            },
            Err(e) => error!("Failed to create eidetica transaction: {e}"),
        }

        self.entries.push(entry);
    }

    /// Merge backfill history from a gateway (e.g., Matrix room history).
    /// Only inserts entries that are older than our earliest entry or fill gaps.
    /// Deduplicates by timestamp+content.
    pub async fn backfill(&mut self, history: Vec<SessionEntry>) {
        if history.is_empty() {
            return;
        }

        let mut new_count = 0;
        for entry in history {
            let already_exists = self.entries.iter().any(|existing| {
                existing.timestamp == entry.timestamp && existing.content == entry.content
            });
            if !already_exists {
                if let Ok(txn) = self.database.new_transaction().await {
                    if let Ok(store) = txn.get_store::<Table<SessionEntry>>(&self.store_name).await
                    {
                        if store.insert(entry.clone()).await.is_ok() {
                            let _ = txn.commit().await;
                        }
                    }
                }
                self.entries.push(entry);
                new_count += 1;
            }
        }

        if new_count > 0 {
            self.entries.sort_by_key(|e| e.timestamp);
            info!(
                "Backfilled {} entries for {}",
                new_count, self.conversation_id
            );
        }
    }

    /// Get the most recent entry, if any
    pub fn latest_entry(&self) -> Option<&SessionEntry> {
        self.entries.last()
    }

    /// Get all entries in the session
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Get the underlying eidetica Database handle (for sharing with tools)
    pub fn database(&self) -> &Database {
        &self.database
    }
}

/// Central registry mapping transport IDs to per-session eidetica Databases.
///
/// The registry itself is backed by an eidetica Database ("chaz-registry") containing
/// a `Table<SessionBinding>` that persists transport_id → session DB mappings across
/// restarts. Each session gets its own eidetica Database for message storage.
///
/// The registry also holds a separate "chaz-central" Database for shared data
/// (memory store, secrets) that isn't per-conversation.
/// Notification emitted when a new session is created in the registry.
#[derive(Debug, Clone)]
pub struct NewSessionEvent {
    pub transport_id: String,
    pub session_db_id: String,
}

pub struct SessionRegistry {
    /// Eidetica instance (Clone = cheap Arc handle)
    instance: eidetica::Instance,
    /// User for creating new session databases (behind Mutex since create_database needs &mut)
    user: Arc<Mutex<eidetica::user::User>>,
    /// Central registry database — holds SessionBinding table
    registry_db: Database,
    /// Central shared database — for memory tools, secrets, etc.
    central_db: Database,
    /// Agent registry
    pub agents: Arc<AgentRegistry>,
    /// Channel to notify listeners when new sessions are created
    new_session_tx: mpsc::Sender<NewSessionEvent>,
    /// Receiver for new session events (taken by the consumer via subscribe())
    new_session_rx: Mutex<Option<mpsc::Receiver<NewSessionEvent>>>,
}

impl SessionRegistry {
    /// Create or open the session registry.
    ///
    /// Finds or creates two databases:
    /// - "chaz-registry" — bindings table mapping transport IDs to session DBs
    /// - "chaz-central" — shared data (memory, secrets)
    pub async fn new(
        instance: eidetica::Instance,
        mut user: eidetica::user::User,
        agents: Arc<AgentRegistry>,
    ) -> anyhow::Result<Self> {
        let registry_db = find_or_create_db(&mut user, "chaz-registry").await?;
        let central_db = find_or_create_db(&mut user, "chaz-central").await?;
        let (new_session_tx, new_session_rx) = mpsc::channel(64);

        // Watch the registry DB for writes (including remote sync).
        // When a new binding appears (from sync or other sources), notify listeners.
        let sync_tx = new_session_tx.clone();
        registry_db.on_local_write(move |_entry, db, _instance| {
            let sync_tx = sync_tx.clone();
            let db = db.clone();
            Box::pin(async move {
                // Read all bindings and notify for any we haven't seen.
                // This is coarse-grained — the consumer deduplicates.
                if let Ok(txn) = db.new_transaction().await {
                    if let Ok(bindings) = txn.get_store::<Table<SessionBinding>>("bindings").await {
                        if let Ok(results) = bindings.search(|_| true).await {
                            for (_, binding) in results {
                                let _ = sync_tx.try_send(NewSessionEvent {
                                    transport_id: binding.transport_id,
                                    session_db_id: binding.session_db_id,
                                });
                            }
                        }
                    }
                }
                Ok(())
            })
        })?;

        Ok(Self {
            instance,
            user: Arc::new(Mutex::new(user)),
            registry_db,
            central_db,
            agents,
            new_session_tx,
            new_session_rx: Mutex::new(Some(new_session_rx)),
        })
    }

    /// Take the new-session event receiver. Can only be called once; subsequent
    /// calls return None. The consumer (typically the Server) uses this to
    /// auto-detect sessions created by sync, schedules, or other sources.
    pub async fn subscribe_new_sessions(&self) -> Option<mpsc::Receiver<NewSessionEvent>> {
        self.new_session_rx.lock().await.take()
    }

    /// Get the central shared database (for memory tools, secrets, etc.)
    pub fn central_db(&self) -> &Database {
        &self.central_db
    }

    /// Get the eidetica Instance handle
    pub fn instance(&self) -> &eidetica::Instance {
        &self.instance
    }

    /// List all known session bindings.
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<SessionBinding>> {
        let txn = self.registry_db.new_transaction().await?;
        let bindings = txn.get_store::<Table<SessionBinding>>("bindings").await?;
        let results = bindings.search(|_| true).await?;
        Ok(results.into_iter().map(|(_, b)| b).collect())
    }

    /// Open a session database by its eidetica root ID.
    ///
    /// Returns the transport_id (from the registry binding) and the database handle.
    /// Fails if no binding exists for this root ID.
    pub async fn open_session_by_db_id(
        &self,
        db_id: &str,
    ) -> anyhow::Result<(String, ConversationId, Database)> {
        let txn = self.registry_db.new_transaction().await?;
        let bindings = txn.get_store::<Table<SessionBinding>>("bindings").await?;
        let results = bindings.search(|b| b.session_db_id == db_id).await?;

        let (_, binding) = results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No session found for DB ID '{db_id}'"))?;

        let conversation_id = ConversationId(binding.conversation_id);
        let db = {
            let user = self.user.lock().await;
            let root_id = eidetica::entry::ID::parse(&binding.session_db_id).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse session DB ID '{}': {e}",
                    binding.session_db_id
                )
            })?;
            user.open_database(&root_id).await?
        };

        Ok((binding.transport_id, conversation_id, db))
    }

    /// Look up a transport ID and return the session Database, creating one if needed.
    ///
    /// On first call for a transport_id, creates a new eidetica Database and persists
    /// the binding. On subsequent calls (including after restart), opens the existing DB.
    pub async fn get_or_create_session_db(
        &self,
        transport_id: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        // Check registry for existing binding
        let txn = self.registry_db.new_transaction().await?;
        let bindings = txn.get_store::<Table<SessionBinding>>("bindings").await?;

        let existing = bindings.search(|b| b.transport_id == transport_id).await?;

        if let Some((_, binding)) = existing.into_iter().next() {
            // Found existing binding — open the session DB
            let conversation_id = ConversationId(binding.conversation_id);
            let db = {
                let user = self.user.lock().await;
                let root_id = eidetica::entry::ID::parse(&binding.session_db_id).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to parse session DB ID '{}': {e}",
                        binding.session_db_id
                    )
                })?;
                user.open_database(&root_id).await?
            };
            return Ok((conversation_id, db));
        }
        drop(txn); // release before creating DB

        // No binding — create a new session DB
        let conversation_id = ConversationId(transport_id.to_string());
        let db_name = format!("session:{}", transport_id);
        let db = {
            let mut user = self.user.lock().await;
            let mut settings = eidetica::crdt::Doc::new();
            settings.set("name", db_name.as_str());
            let key_id = user.get_default_key()?;
            user.create_database(settings, &key_id).await?
        };

        // Persist the binding
        let txn = self.registry_db.new_transaction().await?;
        let bindings = txn.get_store::<Table<SessionBinding>>("bindings").await?;
        bindings
            .insert(SessionBinding {
                transport_id: transport_id.to_string(),
                conversation_id: conversation_id.0.clone(),
                session_db_id: db.root_id().to_string(),
                agent_name: None,
                name: None,
                model: None,
                role_name: None,
                role_prompt: None,
                backend_name: None,
                backend_url: None,
                backend_key_ref: None,
            })
            .await?;
        txn.commit().await?;

        info!("Created new session DB for {}", transport_id);

        // Notify listeners about the new session
        let _ = self.new_session_tx.try_send(NewSessionEvent {
            transport_id: transport_id.to_string(),
            session_db_id: db.root_id().to_string(),
        });

        Ok((conversation_id, db))
    }

    /// Resolve which agent should handle a conversation.
    /// Priority: explicit override > persisted binding > default agent.
    pub async fn resolve_agent(&self, transport_id: &str, override_name: Option<&str>) -> Agent {
        // Check explicit override first
        if let Some(name) = override_name {
            if let Some(agent) = self.agents.get(name) {
                return agent.clone();
            }
        }

        // Check persisted binding
        if let Ok(txn) = self.registry_db.new_transaction().await {
            if let Ok(bindings) = txn.get_store::<Table<SessionBinding>>("bindings").await {
                if let Ok(results) = bindings.search(|b| b.transport_id == transport_id).await {
                    if let Some((_, binding)) = results.into_iter().next() {
                        if let Some(agent_name) = &binding.agent_name {
                            if let Some(agent) = self.agents.get(agent_name) {
                                return agent.clone();
                            }
                        }
                    }
                }
            }
        }

        self.agents.default_agent().clone()
    }

    /// Look up a session by its human-friendly name.
    ///
    /// Returns the transport_id, ConversationId, and Database handle.
    /// Fails if no session has this name.
    pub async fn open_session_by_name(
        &self,
        name: &str,
    ) -> anyhow::Result<(String, ConversationId, Database)> {
        let txn = self.registry_db.new_transaction().await?;
        let bindings = txn.get_store::<Table<SessionBinding>>("bindings").await?;
        let results = bindings.search(|b| b.name.as_deref() == Some(name)).await?;

        let (_, binding) = results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No session named '{name}'"))?;

        let conversation_id = ConversationId(binding.conversation_id);
        let db = {
            let user = self.user.lock().await;
            let root_id = eidetica::entry::ID::parse(&binding.session_db_id).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse session DB ID '{}': {e}",
                    binding.session_db_id
                )
            })?;
            user.open_database(&root_id).await?
        };

        Ok((binding.transport_id, conversation_id, db))
    }

    /// Set a human-friendly name for a session (persisted).
    ///
    /// Returns an error if the name is already taken by another session.
    pub async fn set_session_name(&self, transport_id: &str, name: String) -> anyhow::Result<()> {
        let txn = self.registry_db.new_transaction().await?;
        let bindings = txn.get_store::<Table<SessionBinding>>("bindings").await?;

        // Check for name collision
        let existing = bindings
            .search(|b| b.name.as_deref() == Some(&name) && b.transport_id != transport_id)
            .await?;
        if !existing.is_empty() {
            anyhow::bail!("Name '{name}' is already used by another session");
        }

        // Update the binding
        let results = bindings.search(|b| b.transport_id == transport_id).await?;
        if let Some((key, mut binding)) = results.into_iter().next() {
            binding.name = Some(name);
            bindings.set(&key, binding).await?;
            txn.commit().await?;
        } else {
            anyhow::bail!("No session found for transport ID '{transport_id}'");
        }

        Ok(())
    }

    /// Clear the name from a session (persisted).
    pub async fn clear_session_name(&self, transport_id: &str) -> anyhow::Result<()> {
        let txn = self.registry_db.new_transaction().await?;
        let bindings = txn.get_store::<Table<SessionBinding>>("bindings").await?;
        let results = bindings.search(|b| b.transport_id == transport_id).await?;
        if let Some((key, mut binding)) = results.into_iter().next() {
            binding.name = None;
            bindings.set(&key, binding).await?;
            txn.commit().await?;
        }
        Ok(())
    }

    /// Resolve a session identifier that could be a name, DB ID, or transport ID.
    ///
    /// Tries in order: name → DB ID → transport ID (creates if needed).
    pub async fn resolve_session(
        &self,
        identifier: &str,
    ) -> anyhow::Result<(String, ConversationId, Database)> {
        // Try name first
        if let Ok(result) = self.open_session_by_name(identifier).await {
            return Ok(result);
        }

        // Try DB ID
        if let Ok(result) = self.open_session_by_db_id(identifier).await {
            return Ok(result);
        }

        // Fall back to transport ID (creates if needed)
        let (conv_id, db) = self.get_or_create_session_db(identifier).await?;
        Ok((identifier.to_string(), conv_id, db))
    }

    /// Bind a conversation to a specific agent (persisted).
    pub async fn set_agent_binding(&self, transport_id: &str, agent_name: String) {
        if let Ok(txn) = self.registry_db.new_transaction().await {
            if let Ok(bindings) = txn.get_store::<Table<SessionBinding>>("bindings").await {
                if let Ok(results) = bindings.search(|b| b.transport_id == transport_id).await {
                    if let Some((key, mut binding)) = results.into_iter().next() {
                        binding.agent_name = Some(agent_name);
                        let _ = bindings.set(&key, binding).await;
                        let _ = txn.commit().await;
                    }
                }
            }
        }
    }

    /// Get the session binding for a transport ID.
    pub async fn get_binding(&self, transport_id: &str) -> Option<SessionBinding> {
        let txn = self.registry_db.new_transaction().await.ok()?;
        let bindings = txn
            .get_store::<Table<SessionBinding>>("bindings")
            .await
            .ok()?;
        let results = bindings
            .search(|b| b.transport_id == transport_id)
            .await
            .ok()?;
        results.into_iter().next().map(|(_, b)| b)
    }

    /// Update a binding field. The `updater` closure mutates the binding in-place.
    pub async fn update_binding(
        &self,
        transport_id: &str,
        updater: impl FnOnce(&mut SessionBinding),
    ) -> anyhow::Result<()> {
        let txn = self.registry_db.new_transaction().await?;
        let bindings = txn.get_store::<Table<SessionBinding>>("bindings").await?;
        let results = bindings.search(|b| b.transport_id == transport_id).await?;
        if let Some((key, mut binding)) = results.into_iter().next() {
            updater(&mut binding);
            bindings.set(&key, binding).await?;
            txn.commit().await?;
            Ok(())
        } else {
            anyhow::bail!("No session found for transport ID '{transport_id}'");
        }
    }
}

/// Find or create a named eidetica database for a user.
async fn find_or_create_db(
    user: &mut eidetica::user::User,
    name: &str,
) -> anyhow::Result<Database> {
    match user.find_database(name).await {
        Ok(existing) if !existing.is_empty() => Ok(existing.into_iter().next().unwrap()),
        _ => {
            let mut settings = eidetica::crdt::Doc::new();
            settings.set("name", name);
            let key_id = user.get_default_key()?;
            Ok(user.create_database(settings, &key_id).await?)
        }
    }
}
