//! `SessionRegistry` struct definition, constructor, session CRUD,
//! session-name index. Fields accessed by sibling modules are
//! `pub(super)` so `channels`/`agents`/`keys` can reach them.

use crate::agent::AgentRegistry;
use crate::types::ConversationId;

use eidetica::auth::types::{DelegatedTreeRef, Permission, PermissionBounds, TreeReference};
use eidetica::store::DocStore;
use eidetica::Database;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use super::{find_or_create_db, read_meta_from_db, update_meta_on_db, SessionIndex};

/// Notification emitted when a new session is indexed in the registry.
#[derive(Debug, Clone)]
pub struct NewSessionEvent {
    pub session_db_id: String,
    pub source: Option<String>,
}

/// Registry over the `chazdb` — the peer-local bookkeeping database for one
/// instance of the chaz agent framework. Canonical session config lives in
/// each session's own DB (see [`super::SessionMeta`]); the chazdb holds only
/// indices and other peer-local state that never syncs.
///
/// Stores inside the chazdb:
/// - `sessions`        (DocStore)  — `session_db_id` → `source` (origin tag)
/// - `matrix_channels` (DocStore)  — `room_id` → `session_db_id`
/// - `session_names`   (DocStore)  — `name` → `session_db_id`
/// - `agents`          (DocStore)  — hosted Agent DBs (see `db_registry`)
/// - `memory_banks`    (DocStore)  — hosted Memory Bank DBs (see `db_registry`)
/// - `heartbeat_last_fired` (DocStore) — peer-local rule timestamps
/// - `schedules`       (Table)     — scheduler last_run state
/// - `secrets`         (DocStore)  — backend API keys, host-injected
pub struct SessionRegistry {
    pub(super) instance: eidetica::Instance,
    /// User for creating new session databases (behind Mutex since create_database needs &mut)
    pub(super) user: Arc<Mutex<eidetica::user::User>>,
    /// The single peer-local bookkeeping database for this chaz instance.
    pub(super) chazdb: Database,
    pub agents: Arc<AgentRegistry>,
    pub(super) new_session_tx: mpsc::Sender<NewSessionEvent>,
    new_session_rx: Mutex<Option<mpsc::Receiver<NewSessionEvent>>>,
}

pub(super) const STORE_SESSIONS: &str = "sessions";
pub(super) const STORE_MATRIX_CHANNELS: &str = "matrix_channels";
pub(super) const STORE_SESSION_NAMES: &str = "session_names";

impl SessionRegistry {
    pub async fn new(
        instance: eidetica::Instance,
        mut user: eidetica::user::User,
        agents: Arc<AgentRegistry>,
    ) -> anyhow::Result<Self> {
        let chazdb = find_or_create_db(&mut user, "chazdb").await?;
        let (new_session_tx, new_session_rx) = mpsc::channel(64);

        // Watch the chazdb for writes (including remote sync).
        // On each write, re-scan the sessions index and fire events for each known session.
        // Consumers dedupe via their own `seen` set.
        let sync_tx = new_session_tx.clone();
        chazdb.on_local_write(move |_entry, db, _instance| {
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
            chazdb,
            agents,
            new_session_tx,
            new_session_rx: Mutex::new(Some(new_session_rx)),
        })
    }

    /// Take the new-session event receiver. Can only be called once.
    pub async fn subscribe_new_sessions(&self) -> Option<mpsc::Receiver<NewSessionEvent>> {
        self.new_session_rx.lock().await.take()
    }

    /// The peer-local bookkeeping database for this chaz instance. Holds
    /// every index, hosted-DB list, schedule state, and secret. Nothing here
    /// syncs — sync-ful state lives in per-session / per-agent / per-bank DBs.
    pub fn chazdb(&self) -> &Database {
        &self.chazdb
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
            let txn = self.chazdb.new_transaction().await?;
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

    /// Create a child session and wire a `DelegatedTreeRef { max: Admin(0) }`
    /// from child→parent into the child's auth settings. Any key with Admin on
    /// the parent session inherits Admin on the child transparently via
    /// eidetica's delegation resolver — no key copying, no refresh needed (the
    /// validator reads the parent's live tips at validation time).
    ///
    /// Used by Living Agents Stage 5 `spawn_agent` / `spawn_task`: supervisor
    /// authority on the invoking session carries into the spawned child.
    pub async fn create_child_session(
        &self,
        parent_session_db_id: &str,
        source: Option<&str>,
    ) -> anyhow::Result<(ConversationId, Database)> {
        let (conv_id, child_db) = self.create_session(source).await?;

        // Open the parent to snapshot a TreeReference (tips at delegation-write
        // time; delegation resolver re-reads live tips at validation time).
        let parent_db = {
            let user = self.user.lock().await;
            let parent_root = eidetica::entry::ID::parse(parent_session_db_id).map_err(|e| {
                anyhow::anyhow!("Invalid parent session ID '{parent_session_db_id}': {e}")
            })?;
            user.open_database(&parent_root).await?
        };
        let parent_tips = parent_db.get_tips().await?;

        let tree_ref = DelegatedTreeRef {
            permission_bounds: PermissionBounds {
                max: Permission::Admin(0),
                min: None,
            },
            tree: TreeReference {
                root: parent_db.root_id().clone(),
                tips: parent_tips,
            },
        };

        {
            let txn = child_db.new_transaction().await?;
            let settings = txn.get_settings()?;
            settings.add_delegated_tree(tree_ref).await?;
            txn.commit().await?;
        }

        info!(
            parent_session_db_id,
            child_session_db_id = %child_db.root_id(),
            "Wired parent→child delegation (Admin(0))"
        );

        Ok((conv_id, child_db))
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
        let txn = self.chazdb.new_transaction().await?;
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
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_SESSION_NAMES).await?;
        Ok(store.get_string(name).await.ok())
    }

    /// Associate a human-friendly name with a session. Fails if the name is taken
    /// by a different session.
    pub async fn set_session_name(&self, session_db_id: &str, name: String) -> anyhow::Result<()> {
        {
            let txn = self.chazdb.new_transaction().await?;
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
            let txn = self.chazdb.new_transaction().await?;
            let store = txn.get_store::<DocStore>(STORE_SESSION_NAMES).await?;
            let _ = store.delete(name).await;
            txn.commit().await?;
        }
        update_meta_on_db(&db, |m| m.name = None).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;

    /// Read the delegation entry for a given parent tree ID from the child
    /// session's auth settings. Returns the DelegatedTreeRef if present.
    async fn read_delegation(
        child_db: &Database,
        parent_root: &eidetica::entry::ID,
    ) -> Option<DelegatedTreeRef> {
        let settings_doc = child_db
            .get_settings()
            .await
            .unwrap()
            .get_auth_doc_for_validation()
            .await
            .unwrap();
        let key = format!("delegations.{parent_root}");
        match settings_doc.get(&key) {
            Some(eidetica::crdt::doc::Value::Doc(d)) => DelegatedTreeRef::try_from(d).ok(),
            _ => None,
        }
    }

    #[tokio::test]
    async fn create_child_session_wires_parent_delegation() {
        let (_instance, registry) = make_registry().await;
        let (_parent_conv, parent_db) = registry.create_session(Some("parent")).await.unwrap();
        let parent_id = parent_db.root_id().to_string();

        let (_child_conv, child_db) = registry
            .create_child_session(&parent_id, Some("child"))
            .await
            .unwrap();

        // Delegation from parent_root to child should be in child's auth
        // settings with max = Admin(0).
        let delegation = read_delegation(&child_db, parent_db.root_id()).await;
        assert!(
            delegation.is_some(),
            "Expected delegation entry for parent tree {parent_id}"
        );
        let d = delegation.unwrap();
        assert_eq!(d.permission_bounds.max, Permission::Admin(0));
        assert_eq!(&d.tree.root, parent_db.root_id());
    }
}
