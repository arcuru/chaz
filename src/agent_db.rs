//! Agent DB primitive — Stage 1 of the Living Agents plan.
//!
//! An `AgentDb` is an eidetica `Database` owned by a per-agent `PrivateKey`.
//! Chaz creates one such DB per agent at startup (bootstrapped from yaml).
//! Whoever holds the key hosts the agent.
//!
//! Well-known stores inside each Agent DB:
//! - `config`  (DocStore) — serializable agent definition mirroring `AgentConfig`
//! - `memory`  (Table<MemoryEntry>) — per-agent facts
//! - `meta`    (DocStore) — display name, description, capabilities, avatar
//! - `history` (Table<SessionHistoryEntry>) — sessions this agent participated in
//!
//! Stage 1 materializes the DBs and populates `config`/`meta` from yaml.
//! Session routing and memory migration arrive in Stages 3+.
//!
//! **Config/meta encoding note:** both stores hold a single JSON-serialized
//! blob under key `"value"`. This keeps the schema tractable for nested
//! types (presets, grants, tool lists). Per-field storage may be revisited
//! later if partial CRDT merges become important.

// Stage 1 defines read-side API surface (read_config, read_meta, database
// handle) that is exercised by tests but not yet consumed by runtime. Stages
// 3+ will wire these in; until then the warnings are noise.
#![allow(dead_code)]

use crate::config::{AgentConfig, AgentPreset, Config};
use crate::grants::Grants;
use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::auth::crypto::PublicKey;
use eidetica::crdt::Doc;
use eidetica::entry::ID;
use eidetica::store::{DocStore, Table};
use eidetica::user::User;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

pub const CONFIG_STORE: &str = "config";
pub const MEMORY_STORE: &str = "memory";
pub const META_STORE: &str = "meta";
pub const HISTORY_STORE: &str = "history";
pub const MEMORY_BANKS_STORE: &str = "memory_banks";
pub const SCHEDULES_STORE: &str = "schedules";
pub const SCHEDULE_FIRES_STORE: &str = "schedule_fires";

const BLOB_KEY: &str = "value";

/// Display metadata for an agent. Surfaced in UI; not load-bearing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentMeta {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub capabilities: Option<String>,
    pub avatar: Option<String>,
}

/// Serializable agent definition. Mirrors the runtime-relevant fields of
/// [`AgentConfig`]. What used to live in yaml will live here once yaml is
/// downgraded to bootstrap sugar (Stage 6).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentDbConfig {
    /// Persona definition: file includes + optional inline text. The
    /// resolved string is what becomes the LLM's system message; live
    /// snapshots written into each session DB freeze the resolved text
    /// at attach/bump time so disk edits don't silently mutate ongoing
    /// sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<crate::persona::Persona>,
    /// Deprecated: name of a config-level `roles:` entry. Migrated into
    /// `persona` at runtime when `persona` is unset (Stage transition).
    /// Kept on the schema so older AgentDbs continue to deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub can_spawn: Vec<String>,
    #[serde(default)]
    pub allowed_callers: Vec<String>,
    pub max_iterations: Option<u32>,
    #[serde(default)]
    pub autonomous: bool,
    #[serde(default)]
    pub presets: HashMap<String, AgentPreset>,
    pub tool_profile: Option<String>,
    pub max_context_tokens: Option<usize>,
    #[serde(default)]
    pub grants: HashMap<String, Grants>,
}

impl AgentDbConfig {
    pub fn from_agent_config(cfg: &AgentConfig) -> Self {
        Self {
            persona: cfg.persona.clone(),
            role: cfg.role.clone(),
            model: cfg.model.clone(),
            tools: cfg.tools.clone(),
            can_spawn: cfg.can_spawn.clone().unwrap_or_default(),
            allowed_callers: cfg.allowed_callers.clone().unwrap_or_default(),
            max_iterations: cfg.max_iterations,
            autonomous: cfg.autonomous,
            presets: cfg.presets.clone().unwrap_or_default(),
            tool_profile: cfg.tool_profile.clone(),
            max_context_tokens: cfg.max_context_tokens,
            grants: cfg.grants.clone().unwrap_or_default(),
        }
    }
}

/// A single memory fact. Stage 1 just declares the schema; memory-tool
/// migration into this store happens later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub timestamp: DateTime<Utc>,
    /// Free-form labels for filtering recall. Default empty for back-compat
    /// with entries written before tags existed.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Record that this agent participated in a given session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistoryEntry {
    pub session_db_id: String,
    pub joined_at: DateTime<Utc>,
}

/// Permission this agent holds on a referenced memory bank.
///
/// Mirrors the relevant axis of eidetica's permission model at the agent
/// level: a cached hint about what this agent can do. Actual authority
/// still lives in the bank's `AuthSettings`; this field is a
/// mirror/display cache written alongside the grant (same pattern as
/// `SessionMeta.agents` mirroring session AuthSettings).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BankPermission {
    Read,
    Write,
}

/// Reference from an Agent DB to an external memory bank DB it has been
/// granted access to. One row per bank in the `memory_banks` store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryBankRef {
    /// Peer-local display name for this bank (how the LLM refers to it).
    pub name: String,
    /// Eidetica DB root ID of the bank (stable global identity).
    pub db_id: String,
    /// What this agent's key can do on the bank.
    pub permission: BankPermission,
}

fn default_true() -> bool {
    true
}
fn default_schedule_max_failures() -> u32 {
    3
}

/// Where a fired [`Schedule`] runs.
///
/// The agent's "home"/default session is **not** a separate concept —
/// it is just a `Pinned` schedule whose `session_db_id` is that session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleTarget {
    /// Fire into this existing session.
    Pinned { session_db_id: String },
    /// Create a fresh session per fire (autonomous recurring task).
    Fresh,
}

/// An agent-owned scheduled wake. Lives in the owning agent's DB
/// `schedules` store, so it syncs and travels with the agent across peers
/// exactly like its persona/config — the agent is the unit of
/// ownership, chaz is the runtime that fires it.
///
/// The `prompt` is invocation-scoped input handed to the agent when the
/// schedule fires; it is *not* written as a broadcast session entry.
/// Failure-tracking fields mirror [`crate::routine::Routine`] so the
/// engine's auto-disable pass can operate on schedules uniformly
/// (`max_failures == 0` opts out).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub trigger: crate::routine::Trigger,
    pub prompt: String,
    pub target: ScheduleTarget,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default = "default_schedule_max_failures")]
    pub max_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Schedule {
    /// Construct a schedule with default resilience settings.
    pub fn new(
        id: impl Into<String>,
        trigger: crate::routine::Trigger,
        prompt: impl Into<String>,
        target: ScheduleTarget,
    ) -> Self {
        Self {
            id: id.into(),
            trigger,
            prompt: prompt.into(),
            target,
            enabled: true,
            consecutive_failures: 0,
            max_failures: 3,
            last_error: None,
        }
    }
}

/// Audit record of one schedule fire, written to the owning agent's
/// `schedule_fires` store. For `Fresh` targets this is how the
/// freshly-created session's address is recoverable ("this schedule
/// fired and the session it created is at X"); for `Pinned` it
/// records the run against the existing session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleFire {
    pub schedule_id: String,
    pub fired_at: DateTime<Utc>,
    /// The session the fire ran in — created (Fresh) or reused (Pinned).
    pub session_db_id: String,
    /// True when this fire created a new session (Fresh target).
    pub fresh: bool,
    /// Token/cost provenance for the turn this fire drove. Attributing
    /// it here keeps autonomous-wake cost on the *agent's* ledger
    /// rather than the session's (session usage stays Message-only by
    /// design). `None` if the turn produced no usable metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::runtime::ResponseMetadata>,
}

/// Handle over the eidetica `Database` that holds an agent's state.
#[derive(Clone, Debug)]
pub struct AgentDb {
    database: Database,
}

impl AgentDb {
    /// Wrap an existing database as an `AgentDb`. Use when the caller already
    /// opened the DB (e.g. via `User::open_database`).
    pub fn from_database(database: Database) -> Self {
        Self { database }
    }

    /// Eidetica root ID of this agent's DB. Stable — this is the agent's
    /// global identity.
    pub fn id(&self) -> ID {
        self.database.root_id().clone()
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub async fn read_config(&self) -> anyhow::Result<AgentDbConfig> {
        read_blob(&self.database, CONFIG_STORE).await
    }

    pub async fn write_config(&self, cfg: &AgentDbConfig) -> anyhow::Result<()> {
        write_blob(&self.database, CONFIG_STORE, cfg).await
    }

    pub async fn read_meta(&self) -> anyhow::Result<AgentMeta> {
        read_blob(&self.database, META_STORE).await
    }

    pub async fn write_meta(&self, meta: &AgentMeta) -> anyhow::Result<()> {
        write_blob(&self.database, META_STORE, meta).await
    }

    /// Touch every well-known store so it exists in the DB. Safe to call
    /// repeatedly; commits in one transaction.
    pub async fn ensure_stores(&self) -> anyhow::Result<()> {
        let txn = self.database.new_transaction().await?;
        txn.get_store::<DocStore>(CONFIG_STORE).await?;
        txn.get_store::<Table<MemoryEntry>>(MEMORY_STORE).await?;
        txn.get_store::<DocStore>(META_STORE).await?;
        txn.get_store::<Table<SessionHistoryEntry>>(HISTORY_STORE)
            .await?;
        txn.get_store::<Table<MemoryBankRef>>(MEMORY_BANKS_STORE)
            .await?;
        txn.get_store::<Table<Schedule>>(SCHEDULES_STORE).await?;
        txn.get_store::<Table<ScheduleFire>>(SCHEDULE_FIRES_STORE)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Agent-owned schedules (Agent-Owned Schedules — Stage 1)
    // -----------------------------------------------------------------

    /// List every schedule owned by this agent.
    pub async fn list_schedules(&self) -> anyhow::Result<Vec<Schedule>> {
        let txn = self.database.new_transaction().await?;
        let store = txn.get_store::<Table<Schedule>>(SCHEDULES_STORE).await?;
        let rows = store.search(|_: &Schedule| true).await?;
        Ok(rows.into_iter().map(|(_, t)| t).collect())
    }

    /// Insert a schedule, or replace the existing one with the same `id`.
    /// Dedup-by-id keeps edits and failure-state updates idempotent
    /// (same pattern as `attach_memory_bank`'s dedup-by-name).
    pub async fn upsert_schedule(&self, schedule: Schedule) -> anyhow::Result<()> {
        let txn = self.database.new_transaction().await?;
        let store = txn.get_store::<Table<Schedule>>(SCHEDULES_STORE).await?;
        let existing = store.search(|t: &Schedule| t.id == schedule.id).await?;
        for (row_id, _) in existing {
            store.delete(&row_id).await?;
        }
        store.insert(schedule).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Remove the schedule with the given `id`. Returns true if a row was
    /// removed; no-op (false) on an unknown id.
    pub async fn remove_schedule(&self, id: &str) -> anyhow::Result<bool> {
        let txn = self.database.new_transaction().await?;
        let store = txn.get_store::<Table<Schedule>>(SCHEDULES_STORE).await?;
        let existing = store.search(|t: &Schedule| t.id == id).await?;
        let removed = !existing.is_empty();
        for (row_id, _) in existing {
            store.delete(&row_id).await?;
        }
        txn.commit().await?;
        Ok(removed)
    }

    /// Find a single schedule by `id`.
    pub async fn find_schedule(&self, id: &str) -> anyhow::Result<Option<Schedule>> {
        let txn = self.database.new_transaction().await?;
        let store = txn.get_store::<Table<Schedule>>(SCHEDULES_STORE).await?;
        let mut rows = store.search(|t: &Schedule| t.id == id).await?;
        Ok(rows.pop().map(|(_, t)| t))
    }

    /// Append a fire-audit record. Append-only — one row per fire.
    pub async fn record_schedule_fire(&self, fire: ScheduleFire) -> anyhow::Result<()> {
        let txn = self.database.new_transaction().await?;
        let store = txn
            .get_store::<Table<ScheduleFire>>(SCHEDULE_FIRES_STORE)
            .await?;
        store.insert(fire).await?;
        txn.commit().await?;
        Ok(())
    }

    /// All fire-audit records, newest-last (insertion order).
    pub async fn list_schedule_fires(&self) -> anyhow::Result<Vec<ScheduleFire>> {
        let txn = self.database.new_transaction().await?;
        let store = txn
            .get_store::<Table<ScheduleFire>>(SCHEDULE_FIRES_STORE)
            .await?;
        let mut rows = store.search(|_: &ScheduleFire| true).await?;
        rows.sort_by(|a, b| a.1.fired_at.cmp(&b.1.fired_at));
        Ok(rows.into_iter().map(|(_, f)| f).collect())
    }

    // -----------------------------------------------------------------
    // Memory bank references (Memory Banks Stage 9.B)
    // -----------------------------------------------------------------

    /// List every external memory bank this agent has been granted access
    /// to. The agent's own DB (with its self `memory` subtree) is *not*
    /// included — self memory is implicit.
    pub async fn list_memory_banks(&self) -> anyhow::Result<Vec<MemoryBankRef>> {
        let txn = self.database.new_transaction().await?;
        let store = txn
            .get_store::<Table<MemoryBankRef>>(MEMORY_BANKS_STORE)
            .await?;
        let rows = store.search(|_: &MemoryBankRef| true).await?;
        Ok(rows.into_iter().map(|(_, r)| r).collect())
    }

    /// Add or update a memory bank reference. If a row with the same
    /// `name` exists, it's replaced so the permission / db_id reflect the
    /// latest grant. `name` is the caller-supplied peer-local alias.
    pub async fn attach_memory_bank(&self, bank_ref: MemoryBankRef) -> anyhow::Result<()> {
        let txn = self.database.new_transaction().await?;
        let store = txn
            .get_store::<Table<MemoryBankRef>>(MEMORY_BANKS_STORE)
            .await?;
        let existing = store
            .search(|r: &MemoryBankRef| r.name == bank_ref.name)
            .await?;
        for (id, _) in existing {
            store.delete(&id).await?;
        }
        store.insert(bank_ref).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Remove the reference with the given peer-local name. Returns true
    /// if a row was removed. No-op on an unknown name.
    pub async fn detach_memory_bank(&self, name: &str) -> anyhow::Result<bool> {
        let txn = self.database.new_transaction().await?;
        let store = txn
            .get_store::<Table<MemoryBankRef>>(MEMORY_BANKS_STORE)
            .await?;
        let existing = store.search(|r: &MemoryBankRef| r.name == name).await?;
        let removed = !existing.is_empty();
        for (id, _) in existing {
            store.delete(&id).await?;
        }
        txn.commit().await?;
        Ok(removed)
    }

    /// Find a single bank reference by its peer-local name. Used by the
    /// recall/remember tool path to resolve `bank` parameter → `db_id`.
    pub async fn find_memory_bank(&self, name: &str) -> anyhow::Result<Option<MemoryBankRef>> {
        let txn = self.database.new_transaction().await?;
        let store = txn
            .get_store::<Table<MemoryBankRef>>(MEMORY_BANKS_STORE)
            .await?;
        let mut rows = store.search(|r: &MemoryBankRef| r.name == name).await?;
        Ok(rows.pop().map(|(_, r)| r))
    }
}

/// Generic read of a JSON blob from a DocStore. Missing blob → `Default::default()`.
async fn read_blob<T>(database: &Database, store_name: &str) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    let txn = database.new_transaction().await?;
    let store = txn.get_store::<DocStore>(store_name).await?;
    match store.get_string(BLOB_KEY).await {
        Ok(json) => Ok(serde_json::from_str(&json)?),
        Err(e) if e.is_not_found() => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

async fn write_blob<T>(database: &Database, store_name: &str, value: &T) -> anyhow::Result<()>
where
    T: serde::Serialize,
{
    let json = serde_json::to_string(value)?;
    let txn = database.new_transaction().await?;
    let store = txn.get_store::<DocStore>(store_name).await?;
    store.set_string(BLOB_KEY, json).await?;
    txn.commit().await?;
    Ok(())
}

/// DB name used in eidetica settings — `find_database` idempotency key in Stage 1.
pub fn agent_db_name(display_name: &str) -> String {
    format!("agent:{display_name}")
}

/// Create a new Agent DB. Generates a fresh key on `user`, creates the DB
/// signed by that key with `name: agent:<display_name>` in settings, then
/// populates the config/meta stores from `agent_cfg` and `meta`. Returns
/// the DB handle alongside the fresh pubkey so the caller can register it
/// in the agents-I-host index.
pub async fn create_agent_db(
    user: &mut User,
    display_name: &str,
    agent_cfg: &AgentDbConfig,
    meta: &AgentMeta,
) -> anyhow::Result<(AgentDb, PublicKey)> {
    let key = user
        .add_private_key(Some(&format!("agent:{display_name}")))
        .await?;
    let mut settings = Doc::new();
    settings.set("name", agent_db_name(display_name).as_str());
    let database = user.create_database(settings, &key).await?;
    info!(
        agent = display_name,
        db_id = %database.root_id(),
        key = %key,
        "Created Agent DB"
    );

    let agent_db = AgentDb::from_database(database);
    agent_db.ensure_stores().await?;
    agent_db.write_config(agent_cfg).await?;
    agent_db.write_meta(meta).await?;
    crate::db_kind::write_marker(
        agent_db.database(),
        crate::db_kind::KIND_AGENT,
        display_name,
    )
    .await?;
    Ok((agent_db, key))
}

/// Look up an existing Agent DB by display name. Returns `(AgentDb, pubkey)`
/// where pubkey is the key this user holds for the DB. Returns `None` if no
/// DB with the given display name is tracked by this user.
pub async fn find_agent_db(user: &User, display_name: &str) -> Option<(AgentDb, PublicKey)> {
    let name = agent_db_name(display_name);
    // `find_database` returns `Err(DatabaseNotFoundByName)` when no matches —
    // collapse any lookup error to None; real errors surface on subsequent
    // operations.
    let database = user.find_database(&name).await.ok()?.into_iter().next()?;
    let pubkey = user.find_key(database.root_id()).ok().flatten()?;
    Some((AgentDb::from_database(database), pubkey))
}

/// One bootstrapped agent: the opened DB plus the pubkey this peer holds for it.
#[derive(Clone)]
pub struct BootstrappedAgent {
    pub db: AgentDb,
    pub pubkey: PublicKey,
}

/// Ensure the given agent has an AgentDb on this peer. Creates one with
/// default config/meta if it doesn't exist. Idempotent.
///
/// Stage 7 (memory migration) relies on every `AgentRegistry` entry having a
/// matching DB so the `remember`/`recall` tools can write to
/// `AgentDb::memory`. The default `chaz` agent isn't in yaml `agents:`, so
/// `bootstrap_from_config` alone doesn't cover it — `main.rs` calls this
/// for every registry entry after bootstrap.
pub async fn ensure_agent_db(
    user: &mut User,
    display_name: &str,
) -> anyhow::Result<BootstrappedAgent> {
    if let Some((db, pubkey)) = find_agent_db(user, display_name).await {
        db.ensure_stores().await?;
        return Ok(BootstrappedAgent { db, pubkey });
    }
    let meta = AgentMeta {
        display_name: Some(display_name.to_string()),
        ..Default::default()
    };
    let (db, pubkey) =
        create_agent_db(user, display_name, &AgentDbConfig::default(), &meta).await?;
    Ok(BootstrappedAgent { db, pubkey })
}

/// Bootstrap: materialize an Agent DB for each yaml agent entry.
///
/// yaml `agents:` is a **template** — it seeds new DBs but does not
/// overwrite existing ones. On a re-run with a pre-existing DB, this
/// function reuses the DB as-is; any `/agent set` edits or synced config
/// from other peers survive restarts. AgentDb is the source of truth
/// post-bootstrap (Stage 8 hydration reads live config per-message).
pub async fn bootstrap_from_config(
    user: &mut User,
    config: &Config,
) -> anyhow::Result<HashMap<String, BootstrappedAgent>> {
    let mut out = HashMap::new();
    let Some(agent_configs) = config.agents.as_ref() else {
        return Ok(out);
    };

    for ac in agent_configs {
        let agent = match find_agent_db(user, &ac.name).await {
            Some((db, pubkey)) => {
                info!(agent = %ac.name, db_id = %db.id(), "Reusing existing Agent DB (yaml is template; DB config preserved)");
                db.ensure_stores().await?;
                BootstrappedAgent { db, pubkey }
            }
            None => {
                let agent_cfg = AgentDbConfig::from_agent_config(ac);
                let meta = AgentMeta {
                    display_name: Some(ac.name.clone()),
                    ..Default::default()
                };
                let (db, pubkey) = create_agent_db(user, &ac.name, &agent_cfg, &meta).await?;
                BootstrappedAgent { db, pubkey }
            }
        };

        out.insert(ac.name.clone(), agent);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config};
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;

    /// Test-only fixture: build a fresh in-memory `Instance` and return a
    /// logged-in `User` session against it. Each test gets an isolated peer.
    async fn test_peer_user() -> User {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        instance.login_user("test", None).await.unwrap()
    }

    fn empty_config_with_agents(agents: Vec<AgentConfig>) -> Config {
        Config {
            agents: Some(agents),
            ..Config::default()
        }
    }

    fn agent_cfg(name: &str) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            persona: None,
            role: Some("default".to_string()),
            model: Some("sonnet".to_string()),
            tools: Some(vec!["get_time".into(), "calculate".into()]),
            can_spawn: None,
            allowed_callers: None,
            max_iterations: Some(15),
            autonomous: false,
            presets: None,
            tool_profile: None,
            max_context_tokens: None,
            grants: None,
        }
    }

    #[tokio::test]
    async fn config_round_trip() {
        let mut user = test_peer_user().await;
        let cfg = AgentDbConfig {
            persona: None,
            role: Some("researcher".to_string()),
            model: Some("opus".to_string()),
            tools: Some(vec!["web_fetch".into()]),
            can_spawn: vec!["writer".into()],
            allowed_callers: vec![],
            max_iterations: Some(40),
            autonomous: true,
            presets: HashMap::new(),
            tool_profile: Some("deep".to_string()),
            max_context_tokens: Some(200_000),
            grants: HashMap::new(),
        };
        let meta = AgentMeta {
            display_name: Some("researcher".to_string()),
            description: Some("digs into sources".to_string()),
            capabilities: None,
            avatar: None,
        };

        let (db, pubkey) = create_agent_db(&mut user, "researcher", &cfg, &meta)
            .await
            .unwrap();

        assert_eq!(db.read_config().await.unwrap(), cfg);
        assert_eq!(db.read_meta().await.unwrap(), meta);
        // Returned pubkey is an actual key the user holds for this DB.
        assert_eq!(user.find_key(&db.id()).unwrap(), Some(pubkey));
    }

    #[tokio::test]
    async fn reopen_by_id() {
        let mut user = test_peer_user().await;
        let cfg = AgentDbConfig {
            role: Some("r".to_string()),
            ..Default::default()
        };
        let meta = AgentMeta {
            display_name: Some("r".to_string()),
            ..Default::default()
        };
        let (db, _) = create_agent_db(&mut user, "r", &cfg, &meta).await.unwrap();
        let id = db.id();

        let reopened = user.open_database(&id).await.unwrap();
        let agent_db = AgentDb::from_database(reopened);
        assert_eq!(agent_db.read_config().await.unwrap(), cfg);
        assert_eq!(agent_db.read_meta().await.unwrap(), meta);
    }

    #[tokio::test]
    async fn bootstrap_is_idempotent() {
        let mut user = test_peer_user().await;
        let config = empty_config_with_agents(vec![agent_cfg("alpha"), agent_cfg("beta")]);

        let first = bootstrap_from_config(&mut user, &config).await.unwrap();
        assert_eq!(first.len(), 2);
        let alpha_id_1 = first["alpha"].db.id();
        let beta_id_1 = first["beta"].db.id();
        let alpha_key_1 = first["alpha"].pubkey.clone();

        // Same yaml → same DBs and same pubkeys (no new ones created).
        let second = bootstrap_from_config(&mut user, &config).await.unwrap();
        assert_eq!(second["alpha"].db.id(), alpha_id_1);
        assert_eq!(second["beta"].db.id(), beta_id_1);
        assert_eq!(second["alpha"].pubkey, alpha_key_1);

        // And no extra keys were generated on the second pass.
        // (Creator key on create_database, one key per agent created on first pass.)
        let keys = user.list_keys().unwrap();
        // 1 default login key + 2 agent keys from first bootstrap.
        assert_eq!(keys.len(), 3);
    }

    #[tokio::test]
    async fn bootstrap_preserves_db_config_on_rerun() {
        // yaml is a template: once a DB exists, subsequent boots must not
        // overwrite its config — `/agent set` edits and synced DB state
        // need to survive restarts.
        let mut user = test_peer_user().await;
        let mut cfg = agent_cfg("chatter");
        cfg.max_iterations = Some(5);
        let config = empty_config_with_agents(vec![cfg]);
        let dbs = bootstrap_from_config(&mut user, &config).await.unwrap();
        let id = dbs["chatter"].db.id();
        assert_eq!(
            dbs["chatter"]
                .db
                .read_config()
                .await
                .unwrap()
                .max_iterations,
            Some(5)
        );

        // Simulate a `/agent set` edit that bumps max_iterations past yaml.
        dbs["chatter"]
            .db
            .write_config(&AgentDbConfig {
                max_iterations: Some(77),
                ..dbs["chatter"].db.read_config().await.unwrap()
            })
            .await
            .unwrap();

        // yaml changes to something else entirely; re-bootstrap must not clobber.
        let mut cfg2 = agent_cfg("chatter");
        cfg2.max_iterations = Some(99);
        let config2 = empty_config_with_agents(vec![cfg2]);
        let dbs2 = bootstrap_from_config(&mut user, &config2).await.unwrap();

        assert_eq!(dbs2["chatter"].db.id(), id);
        assert_eq!(
            dbs2["chatter"]
                .db
                .read_config()
                .await
                .unwrap()
                .max_iterations,
            Some(77),
            "yaml should not overwrite existing DB config"
        );
    }

    // -------------------------------------------------------------------------
    // Memory bank references (Stage 9.B)
    // -------------------------------------------------------------------------

    /// Helper: build a fresh peer + create one AgentDb. Returns `(user,
    /// db)` — the `user` must outlive the `db` or eidetica's Instance
    /// drops and subsequent reads fail with "Instance has been dropped".
    async fn peer_with_agent_db() -> (User, AgentDb) {
        let mut user = test_peer_user().await;
        let (db, _) = create_agent_db(
            &mut user,
            "alpha",
            &AgentDbConfig::default(),
            &AgentMeta {
                display_name: Some("alpha".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        (user, db)
    }

    #[tokio::test]
    async fn memory_banks_empty_by_default() {
        let (_user, db) = peer_with_agent_db().await;
        assert!(db.list_memory_banks().await.unwrap().is_empty());
        assert!(db.find_memory_bank("anything").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn attach_and_list_memory_banks() {
        let (_user, db) = peer_with_agent_db().await;
        let ref1 = MemoryBankRef {
            name: "patrick".to_string(),
            db_id: "sha256:aaaa".to_string(),
            permission: BankPermission::Read,
        };
        let ref2 = MemoryBankRef {
            name: "projects".to_string(),
            db_id: "sha256:bbbb".to_string(),
            permission: BankPermission::Write,
        };
        db.attach_memory_bank(ref1.clone()).await.unwrap();
        db.attach_memory_bank(ref2.clone()).await.unwrap();

        let mut banks = db.list_memory_banks().await.unwrap();
        banks.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(banks, vec![ref1, ref2]);
    }

    #[tokio::test]
    async fn attach_replaces_existing_by_name() {
        // Re-attaching under the same name updates db_id + permission
        // rather than leaving two rows.
        let (_user, db) = peer_with_agent_db().await;
        db.attach_memory_bank(MemoryBankRef {
            name: "patrick".to_string(),
            db_id: "sha256:aaaa".to_string(),
            permission: BankPermission::Read,
        })
        .await
        .unwrap();
        db.attach_memory_bank(MemoryBankRef {
            name: "patrick".to_string(),
            db_id: "sha256:cccc".to_string(),
            permission: BankPermission::Write,
        })
        .await
        .unwrap();

        let banks = db.list_memory_banks().await.unwrap();
        assert_eq!(banks.len(), 1);
        assert_eq!(banks[0].db_id, "sha256:cccc");
        assert_eq!(banks[0].permission, BankPermission::Write);
    }

    #[tokio::test]
    async fn detach_removes_and_reports_absence() {
        let (_user, db) = peer_with_agent_db().await;
        db.attach_memory_bank(MemoryBankRef {
            name: "patrick".to_string(),
            db_id: "sha256:aaaa".to_string(),
            permission: BankPermission::Read,
        })
        .await
        .unwrap();
        assert!(db.detach_memory_bank("patrick").await.unwrap());
        assert!(db.list_memory_banks().await.unwrap().is_empty());
        // Second detach is a no-op; returns false.
        assert!(!db.detach_memory_bank("patrick").await.unwrap());
    }

    #[tokio::test]
    async fn find_by_name_resolves_ref() {
        let (_user, db) = peer_with_agent_db().await;
        db.attach_memory_bank(MemoryBankRef {
            name: "patrick".to_string(),
            db_id: "sha256:aaaa".to_string(),
            permission: BankPermission::Write,
        })
        .await
        .unwrap();

        let found = db
            .find_memory_bank("patrick")
            .await
            .unwrap()
            .expect("found");
        assert_eq!(found.db_id, "sha256:aaaa");
        assert_eq!(found.permission, BankPermission::Write);

        assert!(db.find_memory_bank("missing").await.unwrap().is_none());
    }

    // -------------------------------------------------------------------------
    // Agent-owned schedules (Stage 1)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn schedules_empty_by_default() {
        let (_user, db) = peer_with_agent_db().await;
        assert!(db.list_schedules().await.unwrap().is_empty());
        assert!(db.find_schedule("nope").await.unwrap().is_none());
        assert!(!db.remove_schedule("nope").await.unwrap());
    }

    #[tokio::test]
    async fn schedule_crud_round_trips_pinned_and_fresh() {
        use crate::routine::Trigger;
        let (_user, db) = peer_with_agent_db().await;

        let pinned = Schedule::new(
            "daily-brief",
            Trigger::Cron {
                expr: "0 0 9 * * *".into(),
            },
            "summarize overnight activity",
            ScheduleTarget::Pinned {
                session_db_id: "sha256:sess".into(),
            },
        );
        let fresh = Schedule::new(
            "nightly-task",
            Trigger::OneShot {
                fire_at: Utc::now() + chrono::Duration::hours(1),
            },
            "run the nightly sweep",
            ScheduleTarget::Fresh,
        );
        db.upsert_schedule(pinned.clone()).await.unwrap();
        db.upsert_schedule(fresh.clone()).await.unwrap();

        let mut got = db.list_schedules().await.unwrap();
        got.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(got, vec![pinned.clone(), fresh.clone()]);
        assert_eq!(db.find_schedule("daily-brief").await.unwrap(), Some(pinned));

        // Defaults applied by Schedule::new.
        let f = db.find_schedule("nightly-task").await.unwrap().unwrap();
        assert!(f.enabled);
        assert_eq!(f.max_failures, 3);
        assert_eq!(f.consecutive_failures, 0);
        assert!(matches!(f.target, ScheduleTarget::Fresh));
    }

    #[tokio::test]
    async fn upsert_replaces_by_id_not_appends() {
        use crate::routine::Trigger;
        let (_user, db) = peer_with_agent_db().await;
        let mut t = Schedule::new(
            "t1",
            Trigger::Cron {
                expr: "0 * * * * *".into(),
            },
            "first",
            ScheduleTarget::Fresh,
        );
        db.upsert_schedule(t.clone()).await.unwrap();

        // Same id, mutated state (e.g. a failure-tracking update).
        t.prompt = "second".into();
        t.consecutive_failures = 2;
        t.enabled = false;
        db.upsert_schedule(t.clone()).await.unwrap();

        let all = db.list_schedules().await.unwrap();
        assert_eq!(all.len(), 1, "upsert must replace, not append");
        assert_eq!(all[0].prompt, "second");
        assert_eq!(all[0].consecutive_failures, 2);
        assert!(!all[0].enabled);

        assert!(db.remove_schedule("t1").await.unwrap());
        assert!(db.list_schedules().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn schedule_fires_audit_log_appends_in_order() {
        let (_user, db) = peer_with_agent_db().await;
        assert!(db.list_schedule_fires().await.unwrap().is_empty());

        let f1 = ScheduleFire {
            schedule_id: "nightly".into(),
            fired_at: Utc::now(),
            session_db_id: "sha256:fresh1".into(),
            fresh: true,
            usage: None,
        };
        let f2 = ScheduleFire {
            schedule_id: "daily".into(),
            fired_at: Utc::now() + chrono::Duration::seconds(5),
            session_db_id: "sha256:pinned".into(),
            fresh: false,
            usage: None,
        };
        db.record_schedule_fire(f2.clone()).await.unwrap();
        db.record_schedule_fire(f1.clone()).await.unwrap();

        // Sorted by fired_at regardless of insertion order.
        let fires = db.list_schedule_fires().await.unwrap();
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].schedule_id, "nightly");
        assert!(fires[0].fresh);
        assert_eq!(fires[0].session_db_id, "sha256:fresh1");
        assert_eq!(fires[1].schedule_id, "daily");
        assert!(!fires[1].fresh);
    }

    #[tokio::test]
    async fn schedule_serde_tags_target_kind() {
        let p = serde_json::to_string(&ScheduleTarget::Pinned {
            session_db_id: "sha256:x".into(),
        })
        .unwrap();
        assert!(p.contains("\"kind\":\"pinned\""), "got: {p}");
        let f = serde_json::to_string(&ScheduleTarget::Fresh).unwrap();
        assert!(f.contains("\"kind\":\"fresh\""), "got: {f}");
        assert_eq!(
            serde_json::from_str::<ScheduleTarget>(&f).unwrap(),
            ScheduleTarget::Fresh
        );
    }
}
