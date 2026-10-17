//! `SessionRegistry` struct definition, constructor, session CRUD,
//! session-name index. Fields accessed by sibling modules are
//! `pub(super)` so `transport`/`agents`/`keys` can reach them.

use crate::agent::AgentRegistry;
use crate::types::ConversationId;
use anyhow::Context as _;
use chrono::{DateTime, Utc};

use eidetica::Database;
use eidetica::auth::types::{DelegatedTreeRef, Permission, PermissionBounds, TreeReference};
use eidetica::crdt::{
    Doc,
    doc::{List, Value},
};
use eidetica::store::{DocStore, Table};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use super::roots::{self, RootSetup};
use super::{
    BridgeKind, SessionCatalogEntry, SessionIndex, SessionStatus, read_meta_from_db,
    update_meta_on_db,
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
/// - `sessions`          (DocStore)  — `session_db_id` → `source` (origin tag)
/// - `session_names`     (DocStore)  — `name` → `session_db_id`
///
/// Transport channel bindings are **not** here — they live in each session's
/// own DB (see [`super::transport`]) so they sync with the session.
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
    pub(super) user: Arc<Mutex<eidetica::AccountSession>>,
    /// Group-level routing/metadata DB for this chaz instance.
    pub(super) chaz_group: Database,
    /// Peer-runtime state DB (credentials, cron timestamps, schedule state).
    pub(super) chaz_peer: Database,
    pub agents: Arc<AgentRegistry>,
    pub(super) new_session_tx: mpsc::Sender<NewSessionEvent>,
    new_session_rx: Mutex<Option<mpsc::Receiver<NewSessionEvent>>>,
    /// Serialize local explicit renames with extension fill-if-unnamed writes.
    name_lock: Mutex<()>,
    /// Coalesced read-side invalidation, independent of the resident's single
    /// new-session receiver. Observers never consume execution notifications.
    catalog_changed: tokio::sync::watch::Sender<()>,
    /// Sessions created by a local service client may predate the daemon's
    /// in-memory per-DB key mapping. Keep the already-keyed daemon handle so
    /// later opens use the same signing identity.
    pub(super) local_client_sessions: Mutex<std::collections::HashMap<String, Database>>,
    /// Sessions created through this registry already have an explicit owner
    /// path (frontend watch, schedule, child registration, or test setup).
    /// The generic resident watcher only adopts sessions created through a
    /// different service client.
    locally_created_sessions: Mutex<std::collections::HashSet<String>>,
    /// Per-`(transport, login_id, channel)` locks serializing get-or-create in
    /// [`super::transport`], so two concurrent inbound messages on a brand-new
    /// channel can't both miss the registry walk and both create a session.
    /// Keyed by `transport::channel_key`. Grows by distinct channel seen
    /// (bounded; never pruned — entries are tiny).
    pub(super) channel_create_locks: Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
}

pub(super) const STORE_SESSIONS: &str = "sessions";
pub(super) const STORE_SESSION_NAMES: &str = "session_names";
/// User-central session catalog. Companion to `STORE_SESSIONS`; the routing
/// index there stays a cheap id→source map, while this store holds rich
/// per-session metadata as a versioned, atomic Doc.
pub(super) const STORE_SESSION_CATALOG: &str = "session_catalog";
const CATALOG_VERSION: i64 = 1;

impl SessionCatalogEntry {
    fn to_native(&self) -> Doc {
        // Replacements must not merge a source, bridge or status from another row revision.
        let mut row = Doc::atomic();
        row.set("version", CATALOG_VERSION);
        row.set("session_db_id", self.session_db_id.as_str());
        row.set(
            "source",
            self.source.clone().map_or(Value::Null, Value::Text),
        );
        row.set("bridge", self.bridge.as_str());
        row.set("created_at", self.created_at.to_rfc3339());
        row.set(
            "status",
            match self.status {
                SessionStatus::Active => "active",
                SessionStatus::Closed => "closed",
            },
        );
        row
    }

    /// Decode one row without treating malformed data as a missing row. The
    /// caller chooses whether to degrade the result for a display-only read.
    pub(crate) fn from_catalog(catalog: &Doc, id: &str) -> anyhow::Result<Option<Self>> {
        catalog
            .get(id)
            .map(|value| {
                let row = Self::decode_stored(value)
                    .with_context(|| format!("unreadable session catalog row {id}"))?;
                anyhow::ensure!(
                    row.session_db_id == id,
                    "session catalog row ID differs from its key: {id}"
                );
                Ok(row)
            })
            .transpose()
    }

    fn decode_stored(value: &Value) -> anyhow::Result<Self> {
        let doc = match value {
            // Retained rows use the `gateway` field and may omit `status`.
            Value::Text(json) => return Ok(serde_json::from_str(json)?),
            Value::Doc(doc) => doc,
            _ => anyhow::bail!("invalid session catalog value type"),
        };
        anyhow::ensure!(
            doc.get("version").and_then(Value::as_int) == Some(CATALOG_VERSION),
            "unsupported session catalog version"
        );
        anyhow::ensure!(
            doc.is_atomic() && doc.len() == 6,
            "invalid native session catalog record"
        );
        let text = |key| {
            doc.get(key)
                .and_then(Value::as_text)
                .with_context(|| format!("invalid or missing catalog field {key}"))
        };
        let bridge = text("bridge")?;
        Ok(Self {
            session_db_id: text("session_db_id")?.to_owned(),
            source: match doc.get("source") {
                Some(Value::Text(source)) => Some(source.clone()),
                Some(Value::Null) => None,
                _ => anyhow::bail!("invalid or missing catalog source"),
            },
            bridge: BridgeKind::from_filter_str(bridge)
                .filter(|kind| kind.as_str() == bridge)
                .context("invalid catalog bridge")?,
            created_at: text("created_at")?
                .parse()
                .context("invalid catalog timestamp")?,
            status: match text("status")? {
                "active" => SessionStatus::Active,
                "closed" => SessionStatus::Closed,
                _ => anyhow::bail!("invalid catalog status"),
            },
        })
    }
}

const JOB_REQUEST_STORE: &str = "job_request";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct JobRequest {
    pub parent_id: String,
    pub target: String,
    pub task: String,
}

/// Only this completed setup handle can publish the child to the catalog.
#[allow(dead_code)]
pub(crate) struct PreparedAgentJob {
    pub(crate) db: Database,
    source: String,
}

/// Peer-runtime overrides edited by Peer Settings, kept in `chaz_peer`.
const STORE_PEER_DEFAULTS: &str = "peer_defaults";
const KEY_DEFAULT_AGENTS: &str = "default_agents";
const DEFAULT_AGENTS_VERSION: i64 = 1;

async fn read_peer_default_agents(store: &DocStore) -> anyhow::Result<Option<Vec<String>>> {
    let doc = store.get_all().await?;
    let names = match doc.get(KEY_DEFAULT_AGENTS) {
        None => return Ok(None),
        Some(Value::Text(json)) => {
            serde_json::from_str(json).context("invalid retained peer default agents")?
        }
        Some(Value::Doc(value)) => {
            anyhow::ensure!(
                value.get("version").and_then(Value::as_int) == Some(DEFAULT_AGENTS_VERSION),
                "unsupported peer default agents version"
            );
            anyhow::ensure!(
                value.is_atomic() && value.len() == 2,
                "invalid native peer default agents record"
            );
            value
                .get("names")
                .and_then(Value::as_list)
                .context("invalid peer default agents list")?
                .iter()
                .map(|value| {
                    value
                        .as_text()
                        .map(str::to_owned)
                        .context("peer default agent name must be text")
                })
                .collect::<anyhow::Result<_>>()?
        }
        _ => anyhow::bail!("invalid peer default agents value type"),
    };
    Ok(Some(names))
}

impl SessionRegistry {
    /// Open the registry, initializing absent bookkeeping roots only when
    /// this connection is a direct owner of its storage. A service client
    /// requires roots its executor already initialized; use
    /// [`Self::open`] to state the setup role explicitly.
    pub async fn new(
        instance: eidetica::Instance,
        user: eidetica::AccountSession,
        agents: Arc<AgentRegistry>,
    ) -> anyhow::Result<Self> {
        let setup = RootSetup::for_connection(&instance, false);
        Self::open(instance, user, agents, setup).await
    }

    /// Open the registry's `chaz_group` and `chaz_peer` by their Account
    /// references (see [`super::roots`]). Only `RootSetup::Initialize`
    /// creates absent roots; no error path creates a replacement.
    pub async fn open(
        instance: eidetica::Instance,
        mut user: eidetica::AccountSession,
        agents: Arc<AgentRegistry>,
        setup: RootSetup,
    ) -> anyhow::Result<Self> {
        let started = Instant::now();
        let roots::Roots {
            group: chaz_group,
            peer: chaz_peer,
        } = roots::open(&mut user, setup).await?;
        info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            chaz_group = %chaz_group.root_id(),
            chaz_peer = %chaz_peer.root_id(),
            "Session registry roots opened by reference"
        );
        let (new_session_tx, new_session_rx) = mpsc::channel(64);
        let (catalog_changed, _) = tokio::sync::watch::channel(());
        let catalog_notify = catalog_changed.clone();

        // Watch the chaz_group for writes (including remote sync).
        // On each write, re-scan the sessions index and fire events for each known session.
        // Consumers dedupe via their own `seen` set.
        let sync_tx = new_session_tx.clone();
        let stage = Instant::now();
        chaz_group
            .on_write(move |event, db| {
                catalog_notify.send_replace(());
                tracing::debug!(
                    source = ?event.source(),
                    "chaz_group write; rescanning the sessions index"
                );
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
            })
            .await?
            .detach();
        info!(
            elapsed_ms = stage.elapsed().as_millis() as u64,
            total_elapsed_ms = started.elapsed().as_millis() as u64,
            "Session registry callback installed"
        );

        Ok(Self {
            instance,
            user: Arc::new(Mutex::new(user)),
            chaz_group,
            chaz_peer,
            agents,
            new_session_tx,
            new_session_rx: Mutex::new(Some(new_session_rx)),
            name_lock: Mutex::new(()),
            catalog_changed,
            local_client_sessions: Mutex::new(std::collections::HashMap::new()),
            locally_created_sessions: Mutex::new(std::collections::HashSet::new()),
            channel_create_locks: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Get (or create) the per-channel get-or-create lock for `key`
    /// (`transport::channel_key`). Held across the find→create critical section
    /// so concurrent inbound messages on a new channel serialize instead of
    /// both creating a session.
    pub(super) async fn channel_create_lock(&self, key: &str) -> Arc<Mutex<()>> {
        self.channel_create_locks
            .lock()
            .await
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    /// Take the new-session event receiver. Can only be called once.
    pub async fn subscribe_new_sessions(&self) -> Option<mpsc::Receiver<NewSessionEvent>> {
        self.new_session_rx.lock().await.take()
    }

    /// Subscribe without taking the resident executor's notification receiver.
    pub fn catalog_changes(&self) -> tokio::sync::watch::Receiver<()> {
        self.catalog_changed.subscribe()
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

    /// Called only after an executor has recorded participation in an accepted job.
    pub(crate) async fn ensure_executor_db(
        &self,
        agent_db_id: &eidetica::entry::ID,
        agent_key: &eidetica::auth::crypto::PublicKey,
    ) -> anyhow::Result<(Database, eidetica::auth::crypto::PublicKey)> {
        let mut user = self.user.lock().await;
        let agent = crate::agent_db::AgentDb::from_database(
            user.open_database_with_key(agent_db_id, agent_key).await?,
        );
        crate::executor_db::ensure(&mut user, &agent, agent_key).await
    }

    /// Read through the Agent's reference using that Agent's key; no creation.
    pub async fn agent_executor_queues(
        &self,
        agent_db_id: &eidetica::entry::ID,
        agent_key: &eidetica::auth::crypto::PublicKey,
    ) -> anyhow::Result<Vec<crate::executor_db::ExecutorQueue>> {
        let mut user = self.user.lock().await;
        let agent = crate::agent_db::AgentDb::from_database(
            user.open_database_with_key(agent_db_id, agent_key).await?,
        );
        let agent_permission = super::jobs::require_observer_write(agent.database()).await?;
        let mut queues = Vec::new();
        for reference in agent.list_executors().await? {
            let rows = async {
                let db =
                    crate::executor_db::open_for_agent(&mut user, &agent, &reference, agent_key)
                        .await?;
                // This path is the existing single-hop Agent delegation.
                // current_permission on a delegated service handle requires a
                // local backend; use native bounds on the proven source grant.
                // The service still validates the identity on every operation.
                let delegation = db.get_settings().await?.auth_snapshot().await?
                    .get_delegated_tree(agent.database().root_id())?;
                anyhow::ensure!(
                    agent_permission.clamp_to_bounds(&delegation.permission_bounds).can_write(),
                    "insufficient permission: local-v1 job observation requires existing Write authority (Admin allowed) on the Executor DB"
                );
                crate::executor_db::list_jobs(&db).await
            }
            .await;
            let (jobs, unavailable) = match rows {
                Ok(jobs) => (jobs, None),
                Err(error) => (Vec::new(), Some(error.to_string())),
            };
            queues.push(crate::executor_db::ExecutorQueue {
                reference,
                jobs,
                unavailable,
            });
        }
        Ok(queues)
    }

    // -------------------------------------------------------------------------
    // Session creation & opening
    // Unindexed job setup and publication.

    /// Prepare a delegated child without adding it to either discoverable index.
    pub(crate) async fn prepare_agent_job(
        &self,
        parent_id: &str,
        target: &str,
        task: &str,
    ) -> anyhow::Result<PreparedAgentJob> {
        anyhow::ensure!(
            !target.is_empty() && !task.is_empty(),
            "job target and task are required"
        );
        let parent_root = eidetica::entry::ID::parse(parent_id)?;
        // An adopted same-service parent may postdate this AccountSession's key-map
        // snapshot. Reuse its registry-bound, service-validated identity.
        let (_, parent) = self.open_session(parent_id).await?;
        let db = {
            let mut user = self.user.lock().await;
            let mut settings = eidetica::crdt::Doc::new();
            settings.set("name", "session:job-stage:submitter");
            let key = user.get_default_key()?;
            user.create_database(settings, &key).await?
        };
        let source = format!("job-stage:submitter:{}", db.root_id());
        // Eidetica commits separately per DB; none of these writes makes the
        // child discoverable through the watched catalog.
        super::Session::initialize_turn_schema(&db).await?;
        crate::db_kind::write_marker(&db, crate::db_kind::KIND_SESSION, &source).await?;
        let txn = db.new_transaction().await?;
        txn.get_settings()?
            .add_delegated_tree(DelegatedTreeRef {
                permission_bounds: PermissionBounds {
                    max: Permission::Admin(0),
                    min: None,
                },
                tree: TreeReference {
                    root: parent_root,
                    tips: parent.snapshot().await?.into_tips(),
                },
            })
            .await?;
        // Keep the request one whole value, staged with delegation and Directive.
        let mut request = Doc::atomic();
        request.set("parent_id", parent_id);
        request.set("target", target);
        request.set("task", task);
        txn.get_store::<DocStore>(JOB_REQUEST_STORE)
            .await?
            .set_node("v1", request)
            .await?;
        txn.get_store::<Table<super::SessionEntry>>("entries")
            .await?
            .insert(super::SessionEntry {
                sender: "submitter".into(),
                content: task.to_string(),
                timestamp: Utc::now(),
                entry_type: super::EntryType::Directive,
                metadata: None,
                routing: None,
            })
            .await?;
        txn.commit().await?;
        Ok(PreparedAgentJob { db, source })
    }

    /// Read the request as untrusted data. Admission checks the catalog, parent,
    /// delegation and the one prewritten Directive before using its fields.
    pub(crate) async fn read_job_request(db: &Database) -> anyhow::Result<JobRequest> {
        let txn = db.new_transaction().await?;
        let doc = txn
            .get_store::<DocStore>(JOB_REQUEST_STORE)
            .await?
            .get_all()
            .await?;
        anyhow::ensure!(
            doc.len() == 1,
            "invalid published job: requires exactly one request record"
        );
        let value = doc
            .get("v1")
            .ok_or_else(|| anyhow::anyhow!("invalid published job: request is missing"))?;
        match value {
            Value::Text(json) => serde_json::from_str(json).map_err(|error| {
                anyhow::anyhow!("invalid published job: malformed request: {error}")
            }),
            Value::Doc(record) => {
                let (
                    Some(Value::Text(parent_id)),
                    Some(Value::Text(target)),
                    Some(Value::Text(task)),
                ) = (
                    record.get("parent_id"),
                    record.get("target"),
                    record.get("task"),
                )
                else {
                    anyhow::bail!("invalid published job: malformed native request");
                };
                anyhow::ensure!(
                    record.is_atomic() && record.len() == 3,
                    "invalid published job: malformed native request"
                );
                Ok(JobRequest {
                    parent_id: parent_id.clone(),
                    target: target.clone(),
                    task: task.clone(),
                })
            }
            _ => anyhow::bail!("unsupported job request representation"),
        }
    }

    /// Publish only a fully prepared handle to both existing catalog stores.
    pub(crate) async fn publish_agent_job(
        &self,
        prepared: PreparedAgentJob,
    ) -> anyhow::Result<ConversationId> {
        self.publish_prepared_session(&prepared.db, &prepared.source)
            .await
    }

    /// Shared catalog publication; preparation in the source DB must finish first.
    pub(crate) async fn publish_prepared_session(
        &self,
        db: &Database,
        source: &str,
    ) -> anyhow::Result<ConversationId> {
        let id = db.root_id().to_string();
        let source = source.to_string();
        let txn = self.chaz_group.new_transaction().await?;
        let catalog = txn.get_store::<DocStore>(STORE_SESSION_CATALOG).await?;
        SessionCatalogEntry::from_catalog(&catalog.get_all().await?, &id)?;
        txn.get_store::<DocStore>(STORE_SESSIONS)
            .await?
            .set_string(&id, &source)
            .await?;
        catalog
            .set_node(
                &id,
                SessionCatalogEntry {
                    session_db_id: id.clone(),
                    source: Some(source.clone()),
                    bridge: BridgeKind::from_source(Some(&source)),
                    created_at: Utc::now(),
                    status: SessionStatus::Active,
                }
                .to_native(),
            )
            .await?;
        txn.commit().await?;
        self.locally_created_sessions
            .lock()
            .await
            .insert(id.clone());
        let _ = self.new_session_tx.try_send(NewSessionEvent {
            session_db_id: id.clone(),
            source: Some(source),
        });
        Ok(ConversationId(id))
    }

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
        self.locally_created_sessions
            .lock()
            .await
            .insert(session_db_id.clone());

        super::Session::initialize_turn_schema(&db).await?;
        crate::db_kind::write_marker(&db, crate::db_kind::KIND_SESSION, source.unwrap_or("new"))
            .await?;

        // Add to sessions index + session catalog in one transaction so the
        // pair stays consistent even if the second write would fail.
        let catalog_entry = SessionCatalogEntry {
            session_db_id: session_db_id.clone(),
            source: source.map(|s| s.to_string()),
            bridge: BridgeKind::from_source(source),
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
            catalog
                .set_node(&session_db_id, catalog_entry.to_native())
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

    /// Session DBs this peer tracks whose initial settings were created by
    /// [`Self::create_session`] with exactly `source`. The display name is
    /// written in the same commit that creates the DB, so a caller that put a
    /// unique provisioning marker in `source` can recover a DB whose ID was
    /// never recorded (interrupted after creation). Callers decide what more
    /// than one match means; this never picks one.
    pub async fn find_sessions_by_source(&self, source: &str) -> anyhow::Result<Vec<String>> {
        let name = format!("session:{source}");
        let user = self.user.lock().await;
        // AccountSession::find_database skips unreadable tracked databases.
        // Provisioning must not interpret a failed read as marker absence.
        let mut found = Vec::new();
        for tracked in user.databases().await? {
            let db = user.open_database(&tracked.database_id).await?;
            if db.get_name().await? == name {
                found.push(db.root_id().to_string());
            }
        }
        Ok(found)
    }

    /// Whether this registry itself created the session during this process.
    pub async fn created_session_locally(&self, session_db_id: &str) -> bool {
        self.locally_created_sessions
            .lock()
            .await
            .contains(session_db_id)
    }

    /// Create a child session and wire a `DelegatedTreeRef { max: Admin(0) }`
    /// from child→parent into the child's auth settings. Any key with Admin on
    /// the parent session inherits Admin on the child transparently via
    /// eidetica's delegation resolver — no key copying, no refresh needed (the
    /// validator reads the parent's live tips at validation time).
    ///
    /// Used by `spawn_agent` / `spawn_worker`: supervisor
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
        let parent_tips = parent_db.snapshot().await?.into_tips();

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

    /// Allocate an unindexed Worker using the actual parent's bound handle.
    /// Catalog publication and runtime installation are separate later effects.
    pub(crate) async fn prepare_worker_session(
        &self,
        parent: &Database,
        source: &str,
    ) -> anyhow::Result<Database> {
        let db = {
            let mut user = self.user.lock().await;
            let mut settings = eidetica::crdt::Doc::new();
            settings.set("name", format!("session:{source}"));
            let key = user.get_default_key()?;
            user.create_database(settings, &key).await?
        };
        let setup = async {
            super::Session::initialize_turn_schema(&db).await?;
            crate::db_kind::write_marker(&db, crate::db_kind::KIND_SESSION, source).await?;
            let txn = db.new_transaction().await?;
            txn.get_settings()?
                .add_delegated_tree(DelegatedTreeRef {
                    permission_bounds: PermissionBounds {
                        max: Permission::Admin(0),
                        min: None,
                    },
                    tree: TreeReference {
                        root: parent.root_id().clone(),
                        tips: parent.snapshot().await?.into_tips(),
                    },
                })
                .await?;
            txn.commit().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        setup.map_err(|error| {
            anyhow::anyhow!(
                "Unindexed Worker child {} preparation failed (preserved): {error}",
                db.root_id()
            )
        })?;
        Ok(db)
    }

    /// Open an existing session database by its eidetica root ID.
    pub async fn open_session(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        let root_id = eidetica::entry::ID::parse(session_db_id)
            .map_err(|e| anyhow::anyhow!("Invalid session DB ID '{session_db_id}': {e}"))?;
        if let Some(db) = self
            .local_client_sessions
            .lock()
            .await
            .get(session_db_id)
            .cloned()
        {
            return Ok((ConversationId(session_db_id.to_string()), db));
        }
        let user = self.user.lock().await;
        let key_id = user
            .find_key(&root_id)?
            .ok_or_else(|| anyhow::anyhow!("no session signing key"))?;
        let mut db = user.open_database_with_key(&root_id, &key_id).await?;
        // Single-hop delegated sessions share an inherited snapshot floor across
        // interactive turns, tools and pinned schedules. Refresh every keyed
        // embedded open, including read-only opens, without changing the selected key.
        // Service discovery cannot resolve delegation: preserve its bound identity
        // and explicit authority gaps rather than selecting a different path.
        // Direct and nested handles retain their existing native semantics.
        if self.instance.backend().local_engine().is_some()
            && matches!(db.auth_identity(), Some(eidetica::auth::types::SigKey::Delegation { path, .. }) if path.len() == 1)
        {
            let (identity, _) = Database::find_sigkeys(&self.instance, &root_id, &key_id)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("session signer has no current identity"))?;
            db = db.with_key(eidetica::database::DatabaseKey::with_identity(
                user.get_signing_key(&key_id)?,
                identity,
            ));
        }
        Ok((ConversationId(session_db_id.to_string()), db))
    }

    /// Capture the actual selected signer while opening an embedded session.
    /// Cached local-client handles retain their existing exact identity; this
    /// path cannot recover their private signer and must not guess a substitute.
    pub(crate) async fn open_session_with_signer(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<(
        ConversationId,
        Database,
        Option<eidetica::auth::crypto::PrivateKey>,
    )> {
        if let Some(db) = self
            .local_client_sessions
            .lock()
            .await
            .get(session_db_id)
            .cloned()
        {
            return Ok((ConversationId(session_db_id.to_string()), db, None));
        }
        let root = eidetica::entry::ID::parse(session_db_id)?;
        let user = self.user.lock().await;
        let key_id = user
            .find_key(&root)?
            .ok_or_else(|| anyhow::anyhow!("no session signing key"))?;
        let db = user.open_database_with_key(&root, &key_id).await?;
        if self.instance.backend().local_engine().is_none()
            || matches!(db.auth_identity(), Some(eidetica::auth::types::SigKey::Delegation { path, .. }) if path.len() > 1)
        {
            // Native discovery cannot enumerate nested paths. Keep a bound
            // nested writer just as we keep a service-bound writer.
            return Ok((ConversationId(session_db_id.to_string()), db, None));
        }
        let key = user.get_signing_key(&key_id)?;
        let db = super::current_session_writer(&db, Some(&key)).await?;
        Ok((ConversationId(session_db_id.to_string()), db, Some(key)))
    }

    /// Open a session created through this daemon's local Eidetica service.
    ///
    /// The client records the new per-database key mapping in its own `AccountSession`
    /// handle, so the daemon's older handle cannot use `AccountSession::open_database`.
    /// Local client sessions use the shared user's default key; resolve that
    /// key against the session's auth settings and bind the proven identity.
    /// This refuses bridge-owned sessions whose trees do not authorize that
    /// key, leaving them on the existing bridge-adoption path.
    pub async fn open_local_client_session(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        let root_id = eidetica::entry::ID::parse(session_db_id)
            .map_err(|e| anyhow::anyhow!("Invalid session DB ID '{session_db_id}': {e}"))?;
        let user = self.user.lock().await;
        let default_key = user.get_default_key()?;
        let (identity, permission) = Database::find_sigkeys(&self.instance, &root_id, &default_key)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Session {session_db_id} does not authorize this peer's default key"
                )
            })?;
        if !permission.can_write() {
            anyhow::bail!(
                "Session {session_db_id} grants this peer's default key {permission:?}, which cannot run a turn"
            );
        }
        let signing_key = user.get_signing_key(&default_key)?;
        let db = Database::open(&self.instance, &root_id).await?.with_key(
            eidetica::database::DatabaseKey::with_identity(signing_key, identity),
        );
        self.local_client_sessions
            .lock()
            .await
            .insert(session_db_id.to_string(), db.clone());
        Ok((ConversationId(session_db_id.to_string()), db))
    }

    // -------------------------------------------------------------------------
    // Session listing
    // -------------------------------------------------------------------------

    /// List every session known to the registry.
    ///
    /// Joins the routing index (`sessions`) with the catalog
    /// (`session_catalog`). Sessions created before the catalog existed
    /// surface here with `created_at = None` and bridge derived from their
    /// routing-index source string.
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<SessionIndex>> {
        let txn = self.chaz_group.new_transaction().await?;
        let sessions = txn.get_store::<DocStore>(STORE_SESSIONS).await?;
        let catalog = txn.get_store::<DocStore>(STORE_SESSION_CATALOG).await?;
        let names = txn.get_store::<DocStore>(STORE_SESSION_NAMES).await?;
        let routing_doc = sessions.get_all().await?;
        let catalog_doc = catalog.get_all().await?;
        let names_doc = names.get_all().await?;

        let names_by_id: std::collections::HashMap<String, String> = names_doc
            .iter()
            .filter_map(|(name, value)| {
                let session_db_id: String = value.try_into().ok()?;
                Some((session_db_id, name.clone()))
            })
            .collect();

        let mut out: Vec<SessionIndex> = Vec::with_capacity(routing_doc.iter().count());
        for (key, value) in routing_doc.iter() {
            let routing_source: Option<String> =
                value.try_into().ok().filter(|s: &String| !s.is_empty());
            let entry = match SessionCatalogEntry::from_catalog(&catalog_doc, key) {
                Ok(entry) => entry,
                Err(error) => {
                    warn!(session_db_id = %key, "Malformed catalog entry: {error:#}");
                    None
                }
            };
            let (source, bridge, created_at, status) = match entry {
                Some(e) => (e.source, e.bridge, Some(e.created_at), e.status),
                None => {
                    // Display only: synthesize from routing-index source only.
                    let g = BridgeKind::from_source(routing_source.as_deref());
                    (routing_source, g, None, SessionStatus::Active)
                }
            };
            out.push(SessionIndex {
                session_db_id: key.clone(),
                source,
                bridge,
                created_at,
                status,
                name: names_by_id.get(key).cloned(),
            });
        }
        Ok(out)
    }

    /// Upsert a session into the local catalog (both `STORE_SESSIONS` routing
    /// index and `STORE_SESSION_CATALOG` entry). Used when the daemon adopts a
    /// session created by another peer (e.g., a bridge-exposed session
    /// discovered via the agent DB registry) so it appears in the local TUI
    /// `/sessions` list.
    ///
    /// Idempotent, and a *merge* rather than a blind overwrite: adoption runs
    /// again on every rescan and every daemon restart, so anything derived
    /// from "now" or from stores that may not have synced yet would be
    /// rewritten each pass. Re-adopting a session must not change what the
    /// catalog says about it.
    ///
    /// `started_at` is the session's real first-entry timestamp where the
    /// caller could read one (see [`session_origin`]). It wins over a stored
    /// value, so rows previously stamped with an adoption time heal on the
    /// next pass; without it an existing `created_at` is preserved, and only
    /// a genuinely new row falls back to now. Likewise a `None` source never
    /// downgrades a known [`BridgeKind`] to `Other`, and an existing
    /// `status` is carried forward so adoption can't resurrect a closed
    /// session.
    ///
    /// The catalog is peer-local (never syncs), so this doesn't conflict with
    /// the bridge's own catalog entry.
    pub async fn upsert_session_catalog(
        &self,
        session_db_id: &str,
        source: Option<&str>,
        started_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        let txn = self.chaz_group.new_transaction().await?;
        let catalog = txn.get_store::<DocStore>(STORE_SESSION_CATALOG).await?;
        let existing = SessionCatalogEntry::from_catalog(&catalog.get_all().await?, session_db_id)?;
        let source = source
            .map(|s| s.to_string())
            .or_else(|| existing.as_ref().and_then(|e| e.source.clone()));
        let catalog_entry = SessionCatalogEntry {
            session_db_id: session_db_id.to_string(),
            bridge: BridgeKind::from_source(source.as_deref()),
            created_at: started_at
                .or_else(|| existing.as_ref().map(|e| e.created_at))
                .unwrap_or_else(chrono::Utc::now),
            status: existing
                .as_ref()
                .map_or(SessionStatus::Active, |e| e.status),
            source,
        };
        let sessions = txn.get_store::<DocStore>(STORE_SESSIONS).await?;
        sessions
            .set_string(session_db_id, catalog_entry.source.as_deref().unwrap_or(""))
            .await?;
        catalog
            .set_node(session_db_id, catalog_entry.to_native())
            .await?;
        txn.commit().await?;

        info!(
            session_db_id,
            source = ?catalog_entry.source,
            created_at = %catalog_entry.created_at,
            "Upserted session into local catalog (adopted from peer)"
        );
        Ok(())
    }

    /// Mark a session as closed in the catalog (no row removal — Patrick's
    /// "find all sessions" design treats history as append-only).
    ///
    /// Also cleans up any tree-sync relationships that were registered during
    /// session pull (via [`crate::server::build::pull_and_register_trees`]).
    /// Sync cleanup runs even when the session is already closed (remove_tree_sync
    /// on a non-existent relationship is a no-op).
    pub async fn mark_session_closed(&self, session_db_id: &str) -> anyhow::Result<()> {
        let txn = self.chaz_group.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_SESSION_CATALOG).await?;
        let existing = SessionCatalogEntry::from_catalog(&store.get_all().await?, session_db_id)?;
        if let Some(mut entry) = existing
            && entry.status != SessionStatus::Closed
        {
            entry.status = SessionStatus::Closed;
            store.set_node(session_db_id, entry.to_native()).await?;
            txn.commit().await?;
        }

        // Tree-sync cleanup: unregister the (peer, session) pairs so dead
        // relationships don't accumulate in the sync engine over time.
        // Scope fence: tree half only — no peer-identity reaping here.
        if let Some(sync) = self.instance.sync() {
            if let Ok(session_id) = eidetica::entry::ID::parse(session_db_id) {
                match sync.get_tree_peers(&session_id).await {
                    Ok(peers) => {
                        for peer in &peers {
                            match sync.remove_tree_sync(peer.public_key(), &session_id).await {
                                Ok(()) => {
                                    tracing::info!(
                                        session_db_id,
                                        peer = %peer,
                                        "Removed tree sync relationship for closed session"
                                    );
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        session_db_id,
                                        peer = %peer,
                                        "Failed to remove tree sync for closed session: {e}"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            session_db_id,
                            "Failed to get tree peers for closed session: {e}"
                        );
                    }
                }
            } else {
                tracing::debug!(
                    session_db_id,
                    "Invalid session id, cannot clean up tree sync"
                );
            }
        }
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
        let _guard = self.name_lock.lock().await;
        let (_conv_id, db) = self.open_session(session_db_id).await?;
        self.write_session_name(session_db_id, &db, name).await
    }

    /// Fill an unnamed session only. Rechecks after a model call so an explicit
    /// local rename made while that call was in flight always wins.
    pub async fn set_session_name_if_unnamed(
        &self,
        session_db_id: &str,
        name: String,
    ) -> anyhow::Result<bool> {
        let _guard = self.name_lock.lock().await;
        let (_conv_id, db) = self.open_session(session_db_id).await?;
        if super::read_session_name_from_db(&db).await?.is_some() {
            return Ok(false);
        }
        self.write_session_name(session_db_id, &db, name).await?;
        Ok(true)
    }

    async fn write_session_name(
        &self,
        session_db_id: &str,
        db: &Database,
        name: String,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            db.current_permission().await?.can_write(),
            "Session {session_db_id} is read-only"
        );
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
        update_meta_on_db(db, |m| m.name = Some(name.clone())).await?;

        Ok(())
    }

    pub async fn clear_session_name(&self, session_db_id: &str) -> anyhow::Result<()> {
        let _guard = self.name_lock.lock().await;
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

    // -------------------------------------------------------------------------
    // Peer-runtime overrides (chaz_peer)
    // -------------------------------------------------------------------------

    /// Read the persisted peer-level `default_agents` list, if one has
    /// ever been written. `None` means no override exists and callers
    /// should fall back to the yaml `default_agents:` block. An empty
    /// vec is an explicit "no defaults" override.
    pub async fn load_peer_default_agents(&self) -> Option<Vec<String>> {
        let result = async {
            let txn = self.chaz_peer.new_transaction().await?;
            let store = txn.get_store::<DocStore>(STORE_PEER_DEFAULTS).await?;
            read_peer_default_agents(&store).await
        }
        .await;
        match result {
            Ok(names) => names,
            Err(error) => {
                warn!("Unreadable peer default agents; using config fallback: {error:#}");
                None
            }
        }
    }

    /// Persist a new peer-level `default_agents` list, overwriting any
    /// previous value. The empty slice is allowed and means "no
    /// defaults — use the legacy single-agent fallback even if yaml
    /// has entries."
    pub async fn save_peer_default_agents(&self, names: &[String]) -> anyhow::Result<()> {
        let txn = self.chaz_peer.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_PEER_DEFAULTS).await?;
        read_peer_default_agents(&store).await?;
        // An ordered list is replaced as a unit, not merged by positions.
        let mut value = Doc::atomic();
        value.set("version", DEFAULT_AGENTS_VERSION);
        value.set(
            "names",
            names.iter().cloned().map(Value::Text).collect::<List>(),
        );
        store.set_node(KEY_DEFAULT_AGENTS, value).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Forget the persisted peer-level `default_agents`, falling back
    /// to yaml on next startup. UI affordance for reverting.
    pub async fn clear_peer_default_agents(&self) -> anyhow::Result<()> {
        let txn = self.chaz_peer.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_PEER_DEFAULTS).await?;
        if read_peer_default_agents(&store).await?.is_some() {
            store.delete(KEY_DEFAULT_AGENTS).await?;
            txn.commit().await?;
        }
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

        let by_id: std::collections::HashMap<&str, &SessionIndex> =
            list.iter().map(|s| (s.session_db_id.as_str(), s)).collect();

        let cli = by_id[db_cli.root_id().to_string().as_str()];
        assert_eq!(cli.bridge, BridgeKind::Cli);
        assert_eq!(cli.source.as_deref(), Some("cli"));
        assert_eq!(cli.status, SessionStatus::Active);
        assert_eq!(cli.name, None);
        let cli_created = cli.created_at.expect("cli session should have created_at");
        assert!(cli_created >= before && cli_created <= after);

        let matrix = by_id[db_matrix.root_id().to_string().as_str()];
        assert_eq!(matrix.bridge, BridgeKind::Matrix);
        assert!(matrix.source.as_deref().unwrap().starts_with("matrix:"));

        let bare = by_id[db_bare.root_id().to_string().as_str()];
        assert_eq!(bare.bridge, BridgeKind::Other);
        assert_eq!(bare.source, None);
        assert!(bare.created_at.is_some());
    }

    #[tokio::test]
    async fn retained_catalog_rows_stay_readable_and_convert_only_on_write() {
        let (_instance, registry) = make_registry().await;
        let (id, session) = registry
            .create_session(Some("matrix:!room:example.com"))
            .await
            .unwrap();
        let txn = registry.chaz_group.new_transaction().await.unwrap();
        let store = txn
            .get_store::<DocStore>(STORE_SESSION_CATALOG)
            .await
            .unwrap();
        assert!(store.delete(&id.0).await.unwrap());
        txn.commit().await.unwrap();

        // A pre-catalog routing row is still listable without opening the session.
        let fallback = registry.list_sessions().await.unwrap().pop().unwrap();
        assert_eq!(fallback.bridge, BridgeKind::Matrix);
        assert_eq!(fallback.created_at, None);

        let txn = registry.chaz_group.new_transaction().await.unwrap();
        let store = txn
            .get_store::<DocStore>(STORE_SESSION_CATALOG)
            .await
            .unwrap();
        let stamp = "2025-01-02T03:04:05.123456789Z";
        let legacy = format!(
            r#"{{"session_db_id":"{id}","source":"matrix:!room:example.com","gateway":"Matrix","created_at":"{stamp}"}}"#
        );
        store.set_string(&id.0, &legacy).await.unwrap();
        txn.commit().await.unwrap();
        let historical = registry.chaz_group.snapshot().await.unwrap();
        let session_before = session.snapshot().await.unwrap();

        let listed = registry.list_sessions().await.unwrap().pop().unwrap();
        assert_eq!(listed.created_at, Some(stamp.parse().unwrap()));
        assert_eq!(listed.status, SessionStatus::Active);
        assert_eq!(registry.chaz_group.snapshot().await.unwrap(), historical);

        registry.mark_session_closed(&id.0).await.unwrap();
        let txn = registry.chaz_group.new_transaction().await.unwrap();
        let store = txn
            .get_store::<DocStore>(STORE_SESSION_CATALOG)
            .await
            .unwrap();
        let native = store.get_node(&id.0).await.unwrap();
        assert!(native.is_atomic());
        assert_eq!(native.get("version"), Some(&Value::Int(1)));
        assert_eq!(native.get("status"), Some(&Value::Text("closed".into())));
        assert!(
            store.get_string(&id.0).await.is_err(),
            "pre-cutover readers expected text"
        );
        let closed = registry.list_sessions().await.unwrap().pop().unwrap();
        assert_eq!(closed.status, SessionStatus::Closed);
        assert_eq!(closed.created_at, listed.created_at);
        assert_eq!(closed.source, listed.source);
        assert_eq!(closed.bridge, listed.bridge);

        let txn = registry
            .chaz_group
            .new_transaction_at(&historical)
            .await
            .unwrap();
        let store = txn
            .get_store::<DocStore>(STORE_SESSION_CATALOG)
            .await
            .unwrap();
        assert_eq!(store.get_string(&id.0).await.unwrap(), legacy);
        assert_eq!(session.snapshot().await.unwrap(), session_before);
    }

    #[tokio::test]
    async fn malformed_catalog_can_list_but_cannot_be_overwritten() {
        let (_instance, registry) = make_registry().await;
        let (id, session) = registry.create_session(Some("cli")).await.unwrap();
        let txn = registry.chaz_group.new_transaction().await.unwrap();
        let store = txn
            .get_store::<DocStore>(STORE_SESSION_CATALOG)
            .await
            .unwrap();
        let mut row = store.get_node(&id.0).await.unwrap();
        row.set("version", 2_i64);
        store.set_node(&id.0, row).await.unwrap();
        txn.commit().await.unwrap();
        let before = registry.chaz_group.snapshot().await.unwrap();

        let listed = registry.list_sessions().await.unwrap().pop().unwrap();
        assert_eq!(listed.source.as_deref(), Some("cli"));
        assert_eq!(listed.created_at, None);
        for result in [
            registry
                .upsert_session_catalog(&id.0, Some("tui"), None)
                .await,
            registry.mark_session_closed(&id.0).await,
            registry
                .publish_prepared_session(&session, "spawn:replacement")
                .await
                .map(|_| ()),
        ] {
            assert!(
                format!("{:#}", result.unwrap_err())
                    .contains("unsupported session catalog version")
            );
        }
        assert_eq!(registry.chaz_group.snapshot().await.unwrap(), before);
    }

    #[tokio::test]
    async fn peer_defaults_round_trip_and_replace_as_one_native_value() {
        let (_instance, registry) = make_registry().await;
        let txn = registry.chaz_peer.new_transaction().await.unwrap();
        let store = txn
            .get_store::<DocStore>(STORE_PEER_DEFAULTS)
            .await
            .unwrap();
        let legacy = r#"["z","a.本","z",""]"#;
        store.set_string(KEY_DEFAULT_AGENTS, legacy).await.unwrap();
        txn.commit().await.unwrap();
        let names = registry.load_peer_default_agents().await.unwrap();
        assert_eq!(names, ["z", "a.本", "z", ""]);

        registry.save_peer_default_agents(&names).await.unwrap();
        let txn = registry.chaz_peer.new_transaction().await.unwrap();
        let store = txn
            .get_store::<DocStore>(STORE_PEER_DEFAULTS)
            .await
            .unwrap();
        let doc = store.get_node(KEY_DEFAULT_AGENTS).await.unwrap();
        assert!(doc.is_atomic());
        assert_eq!(doc.get("version"), Some(&Value::Int(1)));
        assert_eq!(
            doc.get("names").unwrap().as_list().unwrap().to_vec(),
            names.iter().cloned().map(Value::Text).collect::<Vec<_>>()
        );
        assert_eq!(registry.load_peer_default_agents().await, Some(names));

        registry.save_peer_default_agents(&[]).await.unwrap();
        assert_eq!(registry.load_peer_default_agents().await, Some(vec![]));
        registry.clear_peer_default_agents().await.unwrap();
        assert_eq!(registry.load_peer_default_agents().await, None);
    }

    #[tokio::test]
    async fn corrupt_peer_defaults_refuse_save_and_clear() {
        let (_instance, registry) = make_registry().await;
        let txn = registry.chaz_peer.new_transaction().await.unwrap();
        let store = txn
            .get_store::<DocStore>(STORE_PEER_DEFAULTS)
            .await
            .unwrap();
        let mut doc = Doc::atomic();
        doc.set("version", DEFAULT_AGENTS_VERSION);
        doc.set("names", [Value::Int(42)].into_iter().collect::<List>());
        store.set_node(KEY_DEFAULT_AGENTS, doc).await.unwrap();
        txn.commit().await.unwrap();
        let before = registry.chaz_peer.snapshot().await.unwrap();

        assert_eq!(registry.load_peer_default_agents().await, None);
        for result in [
            registry.save_peer_default_agents(&[]).await,
            registry.clear_peer_default_agents().await,
        ] {
            assert_eq!(
                result.unwrap_err().to_string(),
                "peer default agent name must be text"
            );
        }
        assert_eq!(registry.chaz_peer.snapshot().await.unwrap(), before);
    }

    #[tokio::test]
    async fn list_sessions_joins_names_without_opening_session_databases() {
        let (instance, registry) = make_registry().await;
        let (conv, db) = registry.create_session(Some("cli")).await.unwrap();
        let id = db.root_id().to_string();
        registry
            .set_session_name(&id, "named-session".into())
            .await
            .unwrap();

        let mut session = super::super::Session::new(conv, db).await;
        for n in 0..32 {
            session
                .add_entry(super::super::SessionEntry {
                    sender: "user".into(),
                    content: format!("message {n}"),
                    timestamp: chrono::Utc::now(),
                    entry_type: super::super::EntryType::Message,
                    metadata: None,
                    routing: None,
                })
                .await
                .unwrap();
        }

        let engine = instance.backend().local_engine().unwrap();
        let memory = engine
            .as_any()
            .downcast_ref::<eidetica::backend::database::InMemory>()
            .unwrap();
        instance
            .backend()
            .clear_derived_store_state()
            .await
            .unwrap();
        let root = eidetica::entry::ID::parse(&id).unwrap();
        assert_eq!(memory.store_state_record_count(&root, "entries"), 0);
        let rows = registry.list_sessions().await.unwrap();

        let row = rows.iter().find(|row| row.session_db_id == id).unwrap();
        assert_eq!(row.name.as_deref(), Some("named-session"));
        assert_eq!(memory.store_state_record_count(&root, "entries"), 0);
    }

    /// Adoption re-runs on every rescan and every daemon restart. It used to
    /// rebuild the row from scratch each time, so a Matrix conversation's age
    /// was really "time since this daemon last started" and the whole list
    /// collapsed onto the same age — which also scrambled the ordering, since
    /// the picker sorts on `created_at`.
    #[tokio::test]
    async fn re_adopting_a_session_preserves_its_catalog_row() {
        let (_instance, registry) = make_registry().await;
        let (_conv, db) = registry.create_session(None).await.unwrap();
        let id = db.root_id().to_string();

        let started_at = chrono::Utc::now() - chrono::Duration::days(9);
        registry
            .upsert_session_catalog(&id, Some("matrix:!room1:example.com"), Some(started_at))
            .await
            .unwrap();

        // A later pass with no evidence — bindings not yet synced, no entries
        // to read — must not overwrite what the first pass established.
        registry
            .upsert_session_catalog(&id, None, None)
            .await
            .unwrap();

        let row = registry
            .list_sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.session_db_id == id)
            .expect("adopted session must be listed");
        assert_eq!(
            row.created_at,
            Some(started_at),
            "created_at was re-stamped"
        );
        assert_eq!(row.bridge, BridgeKind::Matrix, "bridge kind downgraded");
        assert_eq!(row.source.as_deref(), Some("matrix:!room1:example.com"));
    }

    /// Rows already written with an adoption timestamp have to heal, not just
    /// stop drifting — the fix is worthless if the existing catalog keeps
    /// showing every Matrix session as minutes old forever.
    #[tokio::test]
    async fn real_start_time_corrects_a_row_stamped_at_adoption() {
        let (_instance, registry) = make_registry().await;
        let (_conv, db) = registry.create_session(None).await.unwrap();
        let id = db.root_id().to_string();

        // The old behavior: stamped with "now" because nothing better was known.
        registry
            .upsert_session_catalog(&id, Some("matrix:!r:example.com"), None)
            .await
            .unwrap();
        let stamped = registry
            .list_sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.session_db_id == id)
            .and_then(|s| s.created_at)
            .expect("row should exist");

        let started_at = chrono::Utc::now() - chrono::Duration::days(3);
        registry
            .upsert_session_catalog(&id, None, Some(started_at))
            .await
            .unwrap();

        let healed = registry
            .list_sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.session_db_id == id)
            .and_then(|s| s.created_at)
            .expect("row should exist");
        assert_ne!(healed, stamped);
        assert_eq!(healed, started_at);
    }

    /// Adoption must not resurrect a session the user closed.
    #[tokio::test]
    async fn re_adopting_a_closed_session_keeps_it_closed() {
        let (_instance, registry) = make_registry().await;
        let (_conv, db) = registry.create_session(Some("cli")).await.unwrap();
        let id = db.root_id().to_string();
        registry.mark_session_closed(&id).await.unwrap();

        registry
            .upsert_session_catalog(&id, Some("cli"), None)
            .await
            .unwrap();

        let list = registry.list_sessions().await.unwrap();
        assert_eq!(
            list.iter().find(|s| s.session_db_id == id).unwrap().status,
            SessionStatus::Closed
        );
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
            list.iter().find(|s| s.session_db_id == id).unwrap().status,
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
    async fn mark_session_closed_removes_tree_sync_relationships() {
        let (_instance, registry) = make_registry_with_sync().await;
        let (_conv, db) = registry.create_session(Some("test")).await.unwrap();
        let id = db.root_id().to_string();
        let session_eid = eidetica::entry::ID::parse(&id).unwrap();

        // Verify sync is enabled
        let sync = registry.instance().sync().expect("sync must be enabled");

        // Register a fake tree sync relationship to simulate what happens
        // during session pull (the build.rs registration loop).
        // add_tree_sync requires the peer to exist first.
        let (_privkey, pubkey) = eidetica::auth::crypto::generate_keypair();

        sync.register_peer(&pubkey, Some("test-peer"))
            .await
            .unwrap();
        sync.add_tree_sync(&pubkey, &session_eid).await.unwrap();

        // Verify the relationship exists before close
        let peers_before = sync.get_tree_peers(&session_eid).await.unwrap();
        assert!(
            !peers_before.is_empty(),
            "peer should be registered before close"
        );

        // Close the session
        registry.mark_session_closed(&id).await.unwrap();

        // Verify the relationship is gone
        let peers_after = sync.get_tree_peers(&session_eid).await.unwrap();
        assert!(
            peers_after.is_empty(),
            "tree sync relationships should be removed after close, got {peers_after:?}"
        );
    }

    #[tokio::test]
    async fn client_prepares_then_publishes_one_inert_child_on_shared_service() {
        use eidetica::backend::database::InMemory;
        use eidetica::instance::{Instance, NewAccount};
        use eidetica::service::ServiceServer;
        use tokio::sync::watch;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("job.sock");
        let (owner, mut owner_user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::with_password("job-test", "test-password"),
        )
        .await
        .unwrap();
        crate::test_support::provision_roots(&mut owner_user).await;
        let service = ServiceServer::bind(owner, &socket).await.unwrap();
        let (shutdown, receiver) = watch::channel(());
        let task = tokio::spawn(service.run(receiver));
        let settings = crate::config::EideticaConfig {
            connection: format!("unix://{}", socket.display()),
            login: crate::config::EideticaLoginConfig {
                username: "job-test".into(),
                password: Some("test-password".into()),
            },
            sync: None,
        };
        let agents = Arc::new(AgentRegistry::with_default_agent());
        let client = crate::instance::connect_with(&settings, crate::config::ExecutionRole::Client)
            .await
            .unwrap();
        let submitter = SessionRegistry::new(client.instance, client.user, agents.clone())
            .await
            .unwrap();
        let (_, parent) = submitter.create_session(Some("cli")).await.unwrap();
        let parent_id = parent.root_id().to_string();
        let orphan = submitter
            .prepare_agent_job(&parent_id, "default", "orphan task")
            .await
            .unwrap();
        let orphan_id = orphan.db.root_id().to_string();
        let prepared = submitter
            .prepare_agent_job(&parent_id, "default", "requested task")
            .await
            .unwrap();
        let id = prepared.db.root_id().to_string();
        let resident =
            crate::instance::connect_with(&settings, crate::config::ExecutionRole::Executor)
                .await
                .unwrap();
        let observer = SessionRegistry::new(resident.instance, resident.user, agents)
            .await
            .unwrap();
        let rows = observer.list_sessions().await.unwrap();
        assert!(
            !rows
                .iter()
                .any(|row| row.session_db_id == id || row.session_db_id == orphan_id)
        );
        assert!(observer.open_job_session(&id).await.is_err());
        assert!(observer.open_job_session(&orphan_id).await.is_err());
        drop(orphan);

        assert_eq!(submitter.publish_agent_job(prepared).await.unwrap().0, id);
        let rows = observer.list_sessions().await.unwrap();
        assert_eq!(rows.iter().filter(|row| row.session_db_id == id).count(), 1);
        assert!(!rows.iter().any(|row| row.session_db_id == orphan_id));
        assert!(
            rows.iter()
                .find(|row| row.session_db_id == id)
                .unwrap()
                .source
                .as_deref()
                .unwrap()
                .starts_with("job-stage:")
        );
        let txn = observer.chaz_group.new_transaction().await.unwrap();
        for store in [STORE_SESSIONS, STORE_SESSION_CATALOG] {
            assert!(
                txn.get_store::<DocStore>(store)
                    .await
                    .unwrap()
                    .get_all()
                    .await
                    .unwrap()
                    .get(&id)
                    .is_some()
            );
        }
        // This is the delegated-job open path, not find_sigkeys on a remote service.
        let (_, child) = observer.open_job_session(&id).await.unwrap();
        assert!(read_delegation(&child, parent.root_id()).await.is_some());
        assert!(crate::session::jobs::is_job_session(&child).await);
        assert!(
            crate::session::jobs::read_accepted_job(&child)
                .await
                .unwrap()
                .is_none()
        );
        let txn = child.new_transaction().await.unwrap();
        let doc = txn
            .get_store::<DocStore>(JOB_REQUEST_STORE)
            .await
            .unwrap()
            .get_all()
            .await
            .unwrap();
        assert!(doc.get("v1").unwrap().as_doc().unwrap().is_atomic());
        assert_eq!(
            SessionRegistry::read_job_request(&child).await.unwrap(),
            JobRequest {
                parent_id,
                target: "default".into(),
                task: "requested task".into(),
            }
        );
        let entries = txn
            .get_store::<Table<super::super::SessionEntry>>("entries")
            .await
            .unwrap()
            .search(|_| true)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.entry_type, super::super::EntryType::Directive);
        assert_eq!(entries[0].1.content, "requested task");
        assert!(observer.open_job_session(&orphan_id).await.is_err());
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn keyed_open_preserves_a_bound_nested_writer() {
        use eidetica::auth::types::{DelegationStep, SigKey};
        let (instance, registry) = make_registry().await;
        let mut user = registry.user.lock().await;
        let key_id = user.get_default_key().unwrap();
        let key = user.get_signing_key(&key_id).unwrap();
        let authority = user
            .create_database(eidetica::crdt::Doc::new(), &key_id)
            .await
            .unwrap();
        let mut path = Vec::new();
        let mut parent = authority;
        for _ in 0..2 {
            path.insert(
                0,
                DelegationStep {
                    tree: parent.root_id().clone(),
                    tips: parent.snapshot().await.unwrap().into_tips(),
                },
            );
            let child = Database::create(
                &instance,
                eidetica::auth::crypto::PrivateKey::generate(),
                eidetica::crdt::Doc::new(),
            )
            .await
            .unwrap();
            let txn = child.new_transaction().await.unwrap();
            txn.get_settings()
                .unwrap()
                .add_delegated_tree(DelegatedTreeRef {
                    permission_bounds: PermissionBounds {
                        max: Permission::Admin(0),
                        min: None,
                    },
                    tree: TreeReference {
                        root: parent.root_id().clone(),
                        tips: parent.snapshot().await.unwrap().into_tips(),
                    },
                })
                .await
                .unwrap();
            txn.commit().await.unwrap();
            parent = child;
        }
        let identity = SigKey::Delegation {
            path,
            hint: SigKey::from_pubkey(&key_id).hint().clone(),
        };
        user.map_key(&key_id, parent.root_id(), identity.clone())
            .await
            .unwrap();
        let bound = parent.with_key(eidetica::database::DatabaseKey::with_identity(
            key,
            identity.clone(),
        ));
        // Native commits can validate the explicitly bound nested path even
        // though find_sigkeys only discovers single-hop paths.
        let txn = bound.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_name("before open")
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let id = bound.root_id().to_string();
        drop(user);
        let (_, reopened, captured) = registry.open_session_with_signer(&id).await.unwrap();
        let mut session = super::super::Session::new(ConversationId(id), reopened)
            .await
            .with_signing_key(captured);
        session.refresh_writer().await.unwrap();
        assert_eq!(session.database().auth_identity(), Some(&identity));
        let txn = session.database().new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_name("after open")
            .await
            .unwrap();
        txn.commit().await.unwrap();
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
