//! `SessionRegistry` struct definition, constructor, session CRUD,
//! session-name index. Fields accessed by sibling modules are
//! `pub(super)` so `channels`/`agents`/`keys` can reach them.

use crate::agent::AgentRegistry;
use crate::types::ConversationId;

use eidetica::Database;
use eidetica::auth::types::{DelegatedTreeRef, Permission, PermissionBounds, TreeReference};
use eidetica::store::DocStore;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use super::{
    GatewayKind, SessionCatalogEntry, SessionIndex, SessionStatus, find_or_create_db,
    read_meta_from_db, update_meta_on_db,
};

/// Notification emitted when a new session is indexed in the registry.
#[derive(Debug, Clone)]
pub struct NewSessionEvent {
    pub session_db_id: String,
    pub source: Option<String>,
}

/// Registry over chaz's two peer-local bookkeeping databases. Neither syncs;
/// canonical sync-ful state lives in per-session, per-agent, and per-bank DBs.
///
/// `chaz_group` — group-level routing/metadata. Stores:
/// - `sessions`        (DocStore)  — `session_db_id` → `source` (origin tag)
/// - `matrix_channels` (DocStore)  — `room_id` → `session_db_id`
/// - `session_names`   (DocStore)  — `name` → `session_db_id`
///
/// Hosted-agent and hosted-bank lookups live in the in-memory
/// [`crate::hosted_index::HostedIndex`] caches built at startup from
/// eidetica's `user.databases()` — not in this DB.
///
/// `chaz_peer` — peer-runtime state. Tied to this binary on this machine.
/// - `credentials`         (DocStore)  — backend API keys, host-injected
/// - `heartbeat_last_fired`(DocStore)  — peer-local rule timestamps
/// - `schedule_state`      (Table)     — scheduler last_run state
pub struct SessionRegistry {
    pub(super) instance: eidetica::Instance,
    /// User for creating new session databases (behind Mutex since create_database needs &mut)
    pub(super) user: Arc<Mutex<eidetica::user::User>>,
    /// Group-level routing/metadata DB for this chaz instance.
    pub(super) chaz_group: Database,
    /// Peer-runtime state DB (credentials, cron timestamps, schedule state).
    pub(super) chaz_peer: Database,
    pub agents: Arc<AgentRegistry>,
    pub(super) new_session_tx: mpsc::Sender<NewSessionEvent>,
    new_session_rx: Mutex<Option<mpsc::Receiver<NewSessionEvent>>>,
}

pub(super) const STORE_SESSIONS: &str = "sessions";
pub(super) const STORE_MATRIX_CHANNELS: &str = "matrix_channels";
pub(super) const STORE_SESSION_NAMES: &str = "session_names";
/// User-central session catalog. Companion to `STORE_SESSIONS`; the routing
/// index there stays a cheap id→source map, while this store holds rich
/// per-session metadata (gateway, created_at, status) as JSON.
pub(super) const STORE_SESSION_CATALOG: &str = "session_catalog";

impl SessionRegistry {
    pub async fn new(
        instance: eidetica::Instance,
        mut user: eidetica::user::User,
        agents: Arc<AgentRegistry>,
    ) -> anyhow::Result<Self> {
        let chaz_group = find_or_create_db(&mut user, "chaz_group").await?;
        let chaz_peer = find_or_create_db(&mut user, "chaz_peer").await?;
        let (new_session_tx, new_session_rx) = mpsc::channel(64);

        // Watch the chaz_group for writes (including remote sync).
        // On each write, re-scan the sessions index and fire events for each known session.
        // Consumers dedupe via their own `seen` set.
        let sync_tx = new_session_tx.clone();
        chaz_group
            .on_write(move |_event, db| {
                let sync_tx = sync_tx.clone();
                let db = db.clone();
                Box::pin(async move {
                    if let Ok(txn) = db.new_transaction().await
                        && let Ok(store) = txn.get_store::<DocStore>(STORE_SESSIONS).await
                        && let Ok(doc) = store.get_all().await
                    {
                        for (key, value) in doc.iter() {
                            let source: Option<String> = value.try_into().ok();
                            let _ = sync_tx.try_send(NewSessionEvent {
                                session_db_id: key.clone(),
                                source,
                            });
                        }
                    }
                    Ok(())
                })
            })?
            .detach();

        Ok(Self {
            instance,
            user: Arc::new(Mutex::new(user)),
            chaz_group,
            chaz_peer,
            agents,
            new_session_tx,
            new_session_rx: Mutex::new(Some(new_session_rx)),
        })
    }

    /// Take the new-session event receiver. Can only be called once.
    pub async fn subscribe_new_sessions(&self) -> Option<mpsc::Receiver<NewSessionEvent>> {
        self.new_session_rx.lock().await.take()
    }

    /// Group-level routing/metadata DB. Holds session/channel/name indices
    /// and the hosted-Agent / hosted-MemoryBank lists. Never syncs.
    pub fn chaz_group(&self) -> &Database {
        &self.chaz_group
    }

    /// Peer-runtime state DB. Holds credentials, heartbeat last-fired
    /// timestamps, and scheduler last-run state. Never syncs — this is
    /// inherently this-binary-on-this-machine state.
    pub fn chaz_peer(&self) -> &Database {
        &self.chaz_peer
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

        crate::db_kind::write_marker(&db, crate::db_kind::KIND_SESSION, source.unwrap_or("new"))
            .await?;

        // Add to sessions index + session catalog in one transaction so the
        // pair stays consistent even if the second write would fail.
        let catalog_entry = SessionCatalogEntry {
            session_db_id: session_db_id.clone(),
            source: source.map(|s| s.to_string()),
            gateway: GatewayKind::from_source(source),
            created_at: chrono::Utc::now(),
            status: SessionStatus::Active,
        };
        {
            let txn = self.chaz_group.new_transaction().await?;
            let sessions = txn.get_store::<DocStore>(STORE_SESSIONS).await?;
            sessions
                .set_string(&session_db_id, source.unwrap_or(""))
                .await?;
            let catalog = txn.get_store::<DocStore>(STORE_SESSION_CATALOG).await?;
            let catalog_json = serde_json::to_string(&catalog_entry)?;
            catalog.set_string(&session_db_id, catalog_json).await?;
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
    ///
    /// Joins the routing index (`sessions`) with the catalog
    /// (`session_catalog`). Sessions created before the catalog existed
    /// surface here with `created_at = None` and gateway derived from their
    /// routing-index source string.
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<SessionIndex>> {
        let txn = self.chaz_group.new_transaction().await?;
        let sessions = txn.get_store::<DocStore>(STORE_SESSIONS).await?;
        let catalog = txn.get_store::<DocStore>(STORE_SESSION_CATALOG).await?;
        let routing_doc = sessions.get_all().await?;
        let catalog_doc = catalog.get_all().await?;

        let mut out: Vec<SessionIndex> = Vec::with_capacity(routing_doc.iter().count());
        for (key, value) in routing_doc.iter() {
            let routing_source: Option<String> =
                value.try_into().ok().filter(|s: &String| !s.is_empty());
            let entry = match catalog_doc.get(key) {
                Some(v) => {
                    let json: Option<String> = v.try_into().ok();
                    json.as_deref().and_then(|s| {
                        serde_json::from_str::<SessionCatalogEntry>(s)
                            .map_err(|e| {
                                warn!(session_db_id = %key, "Malformed catalog entry: {e}");
                                e
                            })
                            .ok()
                    })
                }
                None => None,
            };
            let (source, gateway, created_at, status) = match entry {
                Some(e) => (e.source, e.gateway, Some(e.created_at), e.status),
                None => {
                    // Legacy: synthesize from routing-index source only.
                    let g = GatewayKind::from_source(routing_source.as_deref());
                    (routing_source, g, None, SessionStatus::Active)
                }
            };
            out.push(SessionIndex {
                session_db_id: key.clone(),
                source,
                gateway,
                created_at,
                status,
            });
        }
        Ok(out)
    }

    /// Mark a session as closed in the catalog (no row removal — Patrick's
    /// "find all sessions" design treats history as append-only).
    ///
    /// Stub for the future session-deletion pathway. No-op for sessions that
    /// have no catalog row yet (legacy entries); call after `create_session`
    /// or rely on backfill if needed.
    pub async fn mark_session_closed(&self, session_db_id: &str) -> anyhow::Result<()> {
        let txn = self.chaz_group.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_SESSION_CATALOG).await?;
        let Ok(json) = store.get_string(session_db_id).await else {
            return Ok(());
        };
        let mut entry: SessionCatalogEntry = serde_json::from_str(&json)?;
        if entry.status == SessionStatus::Closed {
            return Ok(());
        }
        entry.status = SessionStatus::Closed;
        store
            .set_string(session_db_id, serde_json::to_string(&entry)?)
            .await?;
        txn.commit().await?;
        Ok(())
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
        let txn = self.chaz_group.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_SESSION_NAMES).await?;
        Ok(store.get_string(name).await.ok())
    }

    /// Associate a human-friendly name with a session. Fails if the name is taken
    /// by a different session.
    pub async fn set_session_name(&self, session_db_id: &str, name: String) -> anyhow::Result<()> {
        {
            let txn = self.chaz_group.new_transaction().await?;
            let store = txn.get_store::<DocStore>(STORE_SESSION_NAMES).await?;
            if let Ok(existing) = store.get_string(&name).await
                && existing != session_db_id
            {
                anyhow::bail!("Name '{name}' is already used by another session");
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
            let txn = self.chaz_group.new_transaction().await?;
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
    async fn list_sessions_populates_catalog_metadata() {
        let (_instance, registry) = make_registry().await;

        let before = chrono::Utc::now();
        let (_conv_cli, db_cli) = registry.create_session(Some("cli")).await.unwrap();
        let (_conv_matrix, db_matrix) = registry
            .create_session(Some("matrix:!room1:example.com"))
            .await
            .unwrap();
        let (_conv_bare, db_bare) = registry.create_session(None).await.unwrap();
        let after = chrono::Utc::now();

        let mut list = registry.list_sessions().await.unwrap();
        list.sort_by(|a, b| a.session_db_id.cmp(&b.session_db_id));
        assert_eq!(list.len(), 3, "expected three sessions in the catalog");

        let by_id: std::collections::HashMap<&str, &SessionIndex> = list
            .iter()
            .map(|s| (s.session_db_id.as_str(), s))
            .collect();

        let cli = by_id[db_cli.root_id().to_string().as_str()];
        assert_eq!(cli.gateway, GatewayKind::Cli);
        assert_eq!(cli.source.as_deref(), Some("cli"));
        assert_eq!(cli.status, SessionStatus::Active);
        let cli_created = cli.created_at.expect("cli session should have created_at");
        assert!(cli_created >= before && cli_created <= after);

        let matrix = by_id[db_matrix.root_id().to_string().as_str()];
        assert_eq!(matrix.gateway, GatewayKind::Matrix);
        assert!(matrix.source.as_deref().unwrap().starts_with("matrix:"));

        let bare = by_id[db_bare.root_id().to_string().as_str()];
        assert_eq!(bare.gateway, GatewayKind::Other);
        assert_eq!(bare.source, None);
        assert!(bare.created_at.is_some());
    }

    #[tokio::test]
    async fn mark_session_closed_flips_catalog_status() {
        let (_instance, registry) = make_registry().await;
        let (_conv, db) = registry.create_session(Some("cli")).await.unwrap();
        let id = db.root_id().to_string();

        registry.mark_session_closed(&id).await.unwrap();
        let list = registry.list_sessions().await.unwrap();
        let entry = list
            .iter()
            .find(|s| s.session_db_id == id)
            .expect("session must still be listed after close");
        assert_eq!(entry.status, SessionStatus::Closed);

        // Idempotent: second call is a no-op, still Closed.
        registry.mark_session_closed(&id).await.unwrap();
        let list = registry.list_sessions().await.unwrap();
        assert_eq!(
            list.iter()
                .find(|s| s.session_db_id == id)
                .unwrap()
                .status,
            SessionStatus::Closed
        );
    }

    #[tokio::test]
    async fn mark_session_closed_is_noop_for_unknown_id() {
        let (_instance, registry) = make_registry().await;
        // Use a syntactically-valid but never-registered id. The fn should
        // silently succeed (no catalog row to update).
        registry
            .mark_session_closed("sha256:deadbeefcafe")
            .await
            .unwrap();
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
