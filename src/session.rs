use crate::agent::{Agent, AgentRegistry};
use crate::types::ConversationId;

use chrono::{DateTime, Utc};
use eidetica::store::{DocStore, Table};
use eidetica::Database;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

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

/// A reference to an agent authorized to participate in a session.
///
/// `db_id` is the agent's eidetica Database root ID — its global identity.
/// `display_name` caches the name so listings don't require opening the
/// agent's DB. Name is advisory; the DB id is canonical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub db_id: String,
    pub display_name: String,
}

/// Metadata stored in each session's own eidetica DB (under the "meta" DocStore).
///
/// This is the authoritative source for per-session configuration. It travels
/// with the session via eidetica sync — sharing a session also shares its
/// name, agent, model, role, and backend choices.
///
/// `agents` is the Living-Agents list of participating Agent DBs. The legacy
/// `agent_name` is still read for backward compatibility and as a fallback
/// when `agents` is empty — Stage 3 keeps both; later stages remove
/// `agent_name` once all sessions are migrated.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub name: Option<String>,
    pub agent_name: Option<String>,
    #[serde(default)]
    pub agents: Vec<AgentRef>,
    pub model: Option<String>,
    pub role_name: Option<String>,
    pub role_prompt: Option<String>,
    pub backend_name: Option<String>,
    pub backend_url: Option<String>,
    pub backend_key_ref: Option<String>,
}

/// Registry index entry — exists for every session known to this instance.
#[derive(Debug, Clone)]
pub struct SessionIndex {
    pub session_db_id: String,
    /// Free-form origin tag for debugging ("matrix:!room", "tui", "spawn:uuid").
    pub source: Option<String>,
}

/// Per-conversation state backed by its own eidetica Database.
///
/// Each session owns a dedicated eidetica Database containing:
/// - `entries` (Table<SessionEntry>) — message/directive/tool-call history
/// - `meta` (DocStore) — session configuration (name, agent, model, etc.)
pub struct Session {
    pub conversation_id: ConversationId,
    database: Database,
    entries: Vec<SessionEntry>,
    /// Store name — "entries" for regular, "entries:{id}" for ephemeral
    store_name: String,
}

const META_STORE: &str = "meta";

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

    /// Read session metadata from the session's own DB.
    /// Returns `SessionMeta::default()` if no meta has been written yet.
    pub async fn read_meta(&self) -> SessionMeta {
        read_meta_from_db(&self.database).await
    }

    /// Mutate session metadata in the session's own DB.
    /// The closure receives the current meta (default if unset) and may modify it.
    pub async fn update_meta<F>(&self, mutator: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut SessionMeta),
    {
        update_meta_on_db(&self.database, mutator).await
    }
}

/// Read the meta DocStore of a session DB. Returns default on any error.
pub async fn read_meta_from_db(database: &Database) -> SessionMeta {
    let Ok(txn) = database.new_transaction().await else {
        return SessionMeta::default();
    };
    let Ok(store) = txn.get_store::<DocStore>(META_STORE).await else {
        return SessionMeta::default();
    };

    let agents: Vec<AgentRef> = match store.get_string("agents").await {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
            warn!("Malformed agents list in SessionMeta, ignoring: {e}");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    };

    SessionMeta {
        name: store.get_string("name").await.ok(),
        agent_name: store.get_string("agent_name").await.ok(),
        agents,
        model: store.get_string("model").await.ok(),
        role_name: store.get_string("role_name").await.ok(),
        role_prompt: store.get_string("role_prompt").await.ok(),
        backend_name: store.get_string("backend_name").await.ok(),
        backend_url: store.get_string("backend_url").await.ok(),
        backend_key_ref: store.get_string("backend_key_ref").await.ok(),
    }
}

/// Apply a mutator to the meta DocStore of a session DB and commit.
pub async fn update_meta_on_db<F>(database: &Database, mutator: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut SessionMeta),
{
    let mut current = read_meta_from_db(database).await;
    mutator(&mut current);

    let txn = database.new_transaction().await?;
    let store = txn.get_store::<DocStore>(META_STORE).await?;

    write_field(&store, "name", current.name.as_deref()).await?;
    write_field(&store, "agent_name", current.agent_name.as_deref()).await?;
    if current.agents.is_empty() {
        let _ = store.delete("agents").await;
    } else {
        let json = serde_json::to_string(&current.agents)?;
        store.set_string("agents", json).await?;
    }
    write_field(&store, "model", current.model.as_deref()).await?;
    write_field(&store, "role_name", current.role_name.as_deref()).await?;
    write_field(&store, "role_prompt", current.role_prompt.as_deref()).await?;
    write_field(&store, "backend_name", current.backend_name.as_deref()).await?;
    write_field(&store, "backend_url", current.backend_url.as_deref()).await?;
    write_field(
        &store,
        "backend_key_ref",
        current.backend_key_ref.as_deref(),
    )
    .await?;

    txn.commit().await?;
    Ok(())
}

async fn write_field(store: &DocStore, key: &str, value: Option<&str>) -> anyhow::Result<()> {
    match value {
        Some(v) => {
            store.set_string(key, v).await?;
        }
        None => {
            // Ignore KeyNotFound on delete — just means it wasn't set.
            let _ = store.delete(key).await;
        }
    }
    Ok(())
}

/// Notification emitted when a new session is indexed in the registry.
#[derive(Debug, Clone)]
pub struct NewSessionEvent {
    pub session_db_id: String,
    pub source: Option<String>,
}

/// Central registry. Holds *indices* into session databases — nothing load-bearing
/// about sessions themselves lives here. Canonical session config lives in each
/// session's own DB (see [`SessionMeta`]).
///
/// Stores (all DocStore in a single `chaz-registry` database):
/// - `sessions`        — `session_db_id` → `source` (origin tag)
/// - `matrix_channels` — `room_id` → `session_db_id`
/// - `session_names`   — `name` → `session_db_id`
///
/// Also owns the `chaz-central` DB for shared, cross-session data (memory tools,
/// secrets, schedule state).
pub struct SessionRegistry {
    instance: eidetica::Instance,
    /// User for creating new session databases (behind Mutex since create_database needs &mut)
    user: Arc<Mutex<eidetica::user::User>>,
    /// Index DB — holds `sessions`, `matrix_channels`, `session_names` DocStores.
    registry_db: Database,
    /// Central shared database (memory tools, secrets, schedules)
    central_db: Database,
    pub agents: Arc<AgentRegistry>,
    new_session_tx: mpsc::Sender<NewSessionEvent>,
    new_session_rx: Mutex<Option<mpsc::Receiver<NewSessionEvent>>>,
}

const STORE_SESSIONS: &str = "sessions";
const STORE_MATRIX_CHANNELS: &str = "matrix_channels";
const STORE_SESSION_NAMES: &str = "session_names";

impl SessionRegistry {
    pub async fn new(
        instance: eidetica::Instance,
        mut user: eidetica::user::User,
        agents: Arc<AgentRegistry>,
    ) -> anyhow::Result<Self> {
        let registry_db = find_or_create_db(&mut user, "chaz-registry").await?;
        let central_db = find_or_create_db(&mut user, "chaz-central").await?;
        let (new_session_tx, new_session_rx) = mpsc::channel(64);

        // Watch the registry DB for writes (including remote sync).
        // On each write, re-scan the sessions index and fire events for each known session.
        // Consumers dedupe via their own `seen` set.
        let sync_tx = new_session_tx.clone();
        registry_db.on_local_write(move |_entry, db, _instance| {
            let sync_tx = sync_tx.clone();
            let db = db.clone();
            Box::pin(async move {
                if let Ok(txn) = db.new_transaction().await {
                    if let Ok(store) = txn.get_store::<DocStore>(STORE_SESSIONS).await {
                        if let Ok(doc) = store.get_all().await {
                            for (key, value) in doc.iter() {
                                let source: Option<String> = value.try_into().ok();
                                let _ = sync_tx.try_send(NewSessionEvent {
                                    session_db_id: key.clone(),
                                    source,
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

    /// Take the new-session event receiver. Can only be called once.
    pub async fn subscribe_new_sessions(&self) -> Option<mpsc::Receiver<NewSessionEvent>> {
        self.new_session_rx.lock().await.take()
    }

    pub fn central_db(&self) -> &Database {
        &self.central_db
    }

    pub fn instance(&self) -> &eidetica::Instance {
        &self.instance
    }

    // -------------------------------------------------------------------------
    // Session creation & opening
    // -------------------------------------------------------------------------

    /// Create a new session database and register it in the sessions index.
    /// `source` is an optional free-form tag used for listing/debugging only.
    pub async fn create_session(
        &self,
        source: Option<&str>,
    ) -> anyhow::Result<(ConversationId, Database)> {
        let db = {
            let mut user = self.user.lock().await;
            let mut settings = eidetica::crdt::Doc::new();
            // Best-effort display name for the DB itself
            let display_name = format!("session:{}", source.unwrap_or("new"));
            settings.set("name", display_name.as_str());
            let key_id = user.get_default_key()?;
            user.create_database(settings, &key_id).await?
        };

        let session_db_id = db.root_id().to_string();
        let conv_id = ConversationId(session_db_id.clone());

        // Add to sessions index
        {
            let txn = self.registry_db.new_transaction().await?;
            let store = txn.get_store::<DocStore>(STORE_SESSIONS).await?;
            store
                .set_string(&session_db_id, source.unwrap_or(""))
                .await?;
            txn.commit().await?;
        }

        info!(
            session_db_id = %session_db_id,
            source = ?source,
            "Created new session"
        );

        let _ = self.new_session_tx.try_send(NewSessionEvent {
            session_db_id: session_db_id.clone(),
            source: source.map(|s| s.to_string()),
        });

        Ok((conv_id, db))
    }

    /// Open an existing session database by its eidetica root ID.
    pub async fn open_session(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        let root_id = eidetica::entry::ID::parse(session_db_id)
            .map_err(|e| anyhow::anyhow!("Invalid session DB ID '{session_db_id}': {e}"))?;
        let user = self.user.lock().await;
        let db = user.open_database(&root_id).await?;
        Ok((ConversationId(session_db_id.to_string()), db))
    }

    // -------------------------------------------------------------------------
    // Session listing
    // -------------------------------------------------------------------------

    /// List every session known to the registry.
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<SessionIndex>> {
        let txn = self.registry_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_SESSIONS).await?;
        let doc = store.get_all().await?;
        Ok(doc
            .iter()
            .map(|(key, value)| {
                let source: Option<String> =
                    value.try_into().ok().filter(|s: &String| !s.is_empty());
                SessionIndex {
                    session_db_id: key.clone(),
                    source,
                }
            })
            .collect())
    }

    // -------------------------------------------------------------------------
    // Resolution
    // -------------------------------------------------------------------------

    /// Resolve an identifier (session name or session DB ID) to an open session.
    pub async fn resolve_session(
        &self,
        identifier: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        if let Some(id) = self.find_by_name(identifier).await? {
            return self.open_session(&id).await;
        }
        // Assume it's a session DB ID
        self.open_session(identifier).await
    }

    // -------------------------------------------------------------------------
    // Name index
    // -------------------------------------------------------------------------

    pub async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<String>> {
        let txn = self.registry_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_SESSION_NAMES).await?;
        Ok(store.get_string(name).await.ok())
    }

    /// Associate a human-friendly name with a session. Fails if the name is taken
    /// by a different session.
    pub async fn set_session_name(&self, session_db_id: &str, name: String) -> anyhow::Result<()> {
        {
            let txn = self.registry_db.new_transaction().await?;
            let store = txn.get_store::<DocStore>(STORE_SESSION_NAMES).await?;
            if let Ok(existing) = store.get_string(&name).await {
                if existing != session_db_id {
                    anyhow::bail!("Name '{name}' is already used by another session");
                }
            }
            store.set_string(&name, session_db_id).await?;
            txn.commit().await?;
        }

        // Mirror into the session's own meta
        let (_conv_id, db) = self.open_session(session_db_id).await?;
        update_meta_on_db(&db, |m| m.name = Some(name.clone())).await?;

        Ok(())
    }

    pub async fn clear_session_name(&self, session_db_id: &str) -> anyhow::Result<()> {
        // Fetch current name from meta so we can find the index entry
        let (_conv_id, db) = self.open_session(session_db_id).await?;
        let current = read_meta_from_db(&db).await;
        if let Some(name) = current.name.as_deref() {
            let txn = self.registry_db.new_transaction().await?;
            let store = txn.get_store::<DocStore>(STORE_SESSION_NAMES).await?;
            let _ = store.delete(name).await;
            txn.commit().await?;
        }
        update_meta_on_db(&db, |m| m.name = None).await?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Matrix channels
    // -------------------------------------------------------------------------

    /// Return the session bound to a Matrix room, if any.
    pub async fn matrix_channel_for_room(&self, room_id: &str) -> anyhow::Result<Option<String>> {
        let txn = self.registry_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_MATRIX_CHANNELS).await?;
        Ok(store.get_string(room_id).await.ok())
    }

    /// Attach a Matrix room to a session. Overwrites any existing binding for this room.
    pub async fn attach_matrix_room(
        &self,
        room_id: &str,
        session_db_id: &str,
    ) -> anyhow::Result<()> {
        let txn = self.registry_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_MATRIX_CHANNELS).await?;
        store.set_string(room_id, session_db_id).await?;
        txn.commit().await?;
        info!(room_id, session_db_id, "Matrix room attached to session");
        Ok(())
    }

    pub async fn detach_matrix_room(&self, room_id: &str) -> anyhow::Result<()> {
        let txn = self.registry_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_MATRIX_CHANNELS).await?;
        let _ = store.delete(room_id).await;
        txn.commit().await?;
        Ok(())
    }

    /// List every (room_id, session_db_id) pair.
    pub async fn list_matrix_channels(&self) -> anyhow::Result<Vec<(String, String)>> {
        let txn = self.registry_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_MATRIX_CHANNELS).await?;
        let doc = store.get_all().await?;
        Ok(doc
            .iter()
            .filter_map(|(k, v)| {
                let session_db_id: String = v.try_into().ok()?;
                Some((k.clone(), session_db_id))
            })
            .collect())
    }

    /// List all Matrix rooms currently attached to a session.
    pub async fn matrix_channels_for_session(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        Ok(self
            .list_matrix_channels()
            .await?
            .into_iter()
            .filter_map(|(room, sid)| {
                if sid == session_db_id {
                    Some(room)
                } else {
                    None
                }
            })
            .collect())
    }

    /// Convenience for the Matrix gateway: get (or create) the session bound to a room.
    ///
    /// If no binding exists, creates a fresh session, attaches the room to it, and
    /// returns it.
    pub async fn get_or_create_matrix_session(
        &self,
        room_id: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        if let Some(session_db_id) = self.matrix_channel_for_room(room_id).await? {
            match self.open_session(&session_db_id).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    warn!(
                        room_id,
                        session_db_id,
                        "Dangling matrix channel — session unreadable, recreating: {e}"
                    );
                    let _ = self.detach_matrix_room(room_id).await;
                }
            }
        }
        let source = format!("matrix:{room_id}");
        let (conv_id, db) = self.create_session(Some(&source)).await?;
        let session_db_id = db.root_id().to_string();
        self.attach_matrix_room(room_id, &session_db_id).await?;
        Ok((conv_id, db))
    }

    // -------------------------------------------------------------------------
    // Agent resolution
    // -------------------------------------------------------------------------

    /// Resolve which agent should handle a session.
    /// Priority: explicit override > session meta > default agent.
    pub async fn resolve_agent(&self, session_db_id: &str, override_name: Option<&str>) -> Agent {
        if let Some(name) = override_name {
            if let Some(agent) = self.agents.get(name) {
                return agent.clone();
            }
        }

        if let Ok((_conv_id, db)) = self.open_session(session_db_id).await {
            let meta = read_meta_from_db(&db).await;
            if let Some(agent_name) = meta.agent_name.as_deref() {
                if let Some(agent) = self.agents.get(agent_name) {
                    return agent.clone();
                }
            }
        }

        self.agents.default_agent().clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;

    /// Test-only fixture: fresh in-memory peer with one database ready for
    /// SessionMeta round-trip tests. Returns the Instance+User so they stay
    /// alive while the Database is in use (dropping the Instance closes the
    /// backend and invalidates the Database handle).
    async fn test_session_db() -> (Instance, eidetica::user::User, Database) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut settings = eidetica::crdt::Doc::new();
        settings.set("name", "test-session");
        let db = user.create_database(settings, &key).await.unwrap();
        (instance, user, db)
    }

    #[tokio::test]
    async fn session_meta_agents_round_trip() {
        let (_instance, _user, db) = test_session_db().await;

        let agents = vec![
            AgentRef {
                db_id: "sha256:abc".to_string(),
                display_name: "alpha".to_string(),
            },
            AgentRef {
                db_id: "sha256:def".to_string(),
                display_name: "beta".to_string(),
            },
        ];

        let expected = agents.clone();
        update_meta_on_db(&db, |m| m.agents = agents).await.unwrap();

        let read_back = read_meta_from_db(&db).await;
        assert_eq!(read_back.agents, expected);
    }

    #[tokio::test]
    async fn session_meta_empty_agents_clears_field() {
        let (_instance, _user, db) = test_session_db().await;

        // Populate then clear.
        update_meta_on_db(&db, |m| {
            m.agents.push(AgentRef {
                db_id: "sha256:x".to_string(),
                display_name: "alpha".to_string(),
            });
        })
        .await
        .unwrap();

        update_meta_on_db(&db, |m| m.agents.clear()).await.unwrap();

        let read_back = read_meta_from_db(&db).await;
        assert!(read_back.agents.is_empty());
    }

    #[tokio::test]
    async fn session_meta_coexists_with_agent_name() {
        let (_instance, _user, db) = test_session_db().await;
        update_meta_on_db(&db, |m| {
            m.agent_name = Some("legacy".to_string());
            m.agents.push(AgentRef {
                db_id: "sha256:a".to_string(),
                display_name: "modern".to_string(),
            });
        })
        .await
        .unwrap();

        let meta = read_meta_from_db(&db).await;
        assert_eq!(meta.agent_name.as_deref(), Some("legacy"));
        assert_eq!(meta.agents.len(), 1);
        assert_eq!(meta.agents[0].display_name, "modern");
    }
}
