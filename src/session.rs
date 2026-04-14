use crate::agent::{Agent, AgentRegistry};
use crate::backends::{ChatContext, Message};
use crate::role::RoleDetails;
use crate::types::ConversationId;

use chrono::{DateTime, Utc};
use eidetica::store::Table;
use eidetica::Database;
use openai_api_rs::v1::chat_completion::MessageRole;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};

/// Maximum messages to include in context sent to the LLM.
/// Older messages are dropped to stay within token limits.
const MAX_CONTEXT_MESSAGES: usize = 50;

/// Type of session entry. Participants (users and agents alike) write entries
/// to a session. There is no user/agent distinction at the protocol level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntryType {
    /// A chat message (from any participant)
    Message,
    /// Acknowledgement that work is in progress
    Ack,
    /// An error that occurred during processing
    Error,
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
}

/// Per-conversation state backed by its own eidetica Database.
///
/// Each session owns a dedicated eidetica Database containing a single
/// `Table<SessionEntry>` store. Entries are loaded from the DB on
/// creation and kept in memory for context building.
///
/// Regular sessions use store name "entries". Ephemeral sessions (spawned
/// agents) use "entries:{conversation_id}" to avoid collisions when sharing
/// a parent's database.
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

    /// Create a new session without loading existing entries.
    /// Used by spawn_agent to create fresh sessions in a parent's database.
    /// Uses a unique store name to avoid collisions with the parent's entries.
    pub async fn new_ephemeral(conversation_id: ConversationId, database: Database) -> Self {
        Session {
            store_name: format!("entries:{}", conversation_id.0),
            conversation_id,
            database,
            entries: Vec::new(),
        }
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
                    if let Ok(store) = txn.get_store::<Table<SessionEntry>>(&self.store_name).await {
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

    /// Build a ChatContext from session history with truncation.
    ///
    /// The `agent_name` parameter determines perspective: entries from this sender
    /// become assistant messages, all others become user messages. Only `Message`
    /// entries are included (Ack, Error are excluded from LLM context).
    pub fn build_context(
        &self,
        agent_name: &str,
        role: Option<RoleDetails>,
        model: Option<String>,
    ) -> ChatContext {
        let start = self.entries.len().saturating_sub(MAX_CONTEXT_MESSAGES);
        let messages = self.entries[start..]
            .iter()
            .filter(|e| e.entry_type == EntryType::Message)
            .map(|e| {
                let msg_role = if e.sender == agent_name {
                    MessageRole::assistant
                } else {
                    MessageRole::user
                };
                Message::new(msg_role, e.content.clone())
            })
            .collect();

        ChatContext {
            messages,
            model,
            role,
        }
    }

    /// Number of entries currently in the session
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Get the most recent entry, if any
    pub fn latest_entry(&self) -> Option<&SessionEntry> {
        self.entries.last()
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

        Ok(Self {
            instance,
            user: Arc::new(Mutex::new(user)),
            registry_db,
            central_db,
            agents,
        })
    }

    /// Get the central shared database (for memory tools, secrets, etc.)
    pub fn central_db(&self) -> &Database {
        &self.central_db
    }

    /// Get the eidetica Instance handle
    pub fn instance(&self) -> &eidetica::Instance {
        &self.instance
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

        let existing = bindings
            .search(|b| b.transport_id == transport_id)
            .await?;

        if let Some((_, binding)) = existing.into_iter().next() {
            // Found existing binding — open the session DB
            let conversation_id = ConversationId(binding.conversation_id);
            let db = {
                let user = self.user.lock().await;
                let root_id = eidetica::entry::ID::parse(&binding.session_db_id).map_err(|e| {
                    anyhow::anyhow!("Failed to parse session DB ID '{}': {e}", binding.session_db_id)
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
            })
            .await?;
        txn.commit().await?;

        info!("Created new session DB for {}", transport_id);
        Ok((conversation_id, db))
    }

    /// Resolve which agent should handle a conversation.
    /// Priority: explicit override > persisted binding > default agent.
    pub async fn resolve_agent(
        &self,
        transport_id: &str,
        override_name: Option<&str>,
    ) -> Agent {
        // Check explicit override first
        if let Some(name) = override_name {
            if let Some(agent) = self.agents.get(name) {
                return agent.clone();
            }
        }

        // Check persisted binding
        if let Ok(txn) = self.registry_db.new_transaction().await {
            if let Ok(bindings) = txn.get_store::<Table<SessionBinding>>("bindings").await {
                if let Ok(results) = bindings
                    .search(|b| b.transport_id == transport_id)
                    .await
                {
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

    /// Bind a conversation to a specific agent (persisted).
    pub async fn set_agent_binding(&self, transport_id: &str, agent_name: String) {
        if let Ok(txn) = self.registry_db.new_transaction().await {
            if let Ok(bindings) = txn.get_store::<Table<SessionBinding>>("bindings").await {
                if let Ok(results) = bindings
                    .search(|b| b.transport_id == transport_id)
                    .await
                {
                    if let Some((key, mut binding)) = results.into_iter().next() {
                        binding.agent_name = Some(agent_name);
                        let _ = bindings.set(&key, binding).await;
                        let _ = txn.commit().await;
                    }
                }
            }
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
