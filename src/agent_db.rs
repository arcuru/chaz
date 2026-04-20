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
use eidetica::crdt::Doc;
use eidetica::entry::ID;
use eidetica::store::{DocStore, Table};
use eidetica::user::User;
use eidetica::Database;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

pub const CONFIG_STORE: &str = "config";
pub const MEMORY_STORE: &str = "memory";
pub const META_STORE: &str = "meta";
pub const HISTORY_STORE: &str = "history";

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
}

/// Record that this agent participated in a given session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistoryEntry {
    pub session_db_id: String,
    pub joined_at: DateTime<Utc>,
}

/// Handle over the eidetica `Database` that holds an agent's state.
#[derive(Clone)]
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
        txn.commit().await?;
        Ok(())
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
/// populates the config/meta stores from `agent_cfg` and `meta`.
pub async fn create_agent_db(
    user: &mut User,
    display_name: &str,
    agent_cfg: &AgentDbConfig,
    meta: &AgentMeta,
) -> anyhow::Result<AgentDb> {
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
    Ok(agent_db)
}

/// Look up an existing Agent DB by display name. Returns `None` if none found.
pub async fn find_agent_db(user: &User, display_name: &str) -> Option<AgentDb> {
    let name = agent_db_name(display_name);
    // `find_database` returns `Err(DatabaseNotFoundByName)` when no matches —
    // collapse any lookup error to None; real errors surface on subsequent
    // operations.
    let matches = user.find_database(&name).await.ok()?;
    matches.into_iter().next().map(AgentDb::from_database)
}

/// Stage 1 bootstrap: materialize an Agent DB for each yaml agent entry.
/// Idempotent — re-runs reuse the existing DB (matched by `agent:<name>`
/// settings.name) and refresh config/meta to mirror the yaml.
pub async fn bootstrap_from_config(
    user: &mut User,
    config: &Config,
) -> anyhow::Result<HashMap<String, AgentDb>> {
    let mut out = HashMap::new();
    let Some(agent_configs) = config.agents.as_ref() else {
        return Ok(out);
    };

    for ac in agent_configs {
        let agent_cfg = AgentDbConfig::from_agent_config(ac);
        let meta = AgentMeta {
            display_name: Some(ac.name.clone()),
            ..Default::default()
        };

        let agent_db = match find_agent_db(user, &ac.name).await {
            Some(db) => {
                info!(agent = %ac.name, db_id = %db.id(), "Reusing existing Agent DB");
                db.ensure_stores().await?;
                db.write_config(&agent_cfg).await?;
                db.write_meta(&meta).await?;
                db
            }
            None => create_agent_db(user, &ac.name, &agent_cfg, &meta).await?,
        };

        out.insert(ac.name.clone(), agent_db);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config};
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;

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
            homeserver_url: String::new(),
            username: String::new(),
            password: None,
            allow_list: None,
            message_limit: None,
            room_size_limit: None,
            state_dir: None,
            chat_summary_model: None,
            role: None,
            roles: None,
            backends: None,
            agents: Some(agents),
            security: None,
            schedules: None,
            mcp_servers: None,
            tool_profiles: None,
            mcp_server_dir: None,
            context: None,
        }
    }

    fn agent_cfg(name: &str) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
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

        let db = create_agent_db(&mut user, "researcher", &cfg, &meta)
            .await
            .unwrap();

        assert_eq!(db.read_config().await.unwrap(), cfg);
        assert_eq!(db.read_meta().await.unwrap(), meta);
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
        let db = create_agent_db(&mut user, "r", &cfg, &meta).await.unwrap();
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
        let alpha_id_1 = first["alpha"].id();
        let beta_id_1 = first["beta"].id();

        // Same yaml → same DBs (no new ones created).
        let second = bootstrap_from_config(&mut user, &config).await.unwrap();
        assert_eq!(second["alpha"].id(), alpha_id_1);
        assert_eq!(second["beta"].id(), beta_id_1);

        // And no extra keys were generated on the second pass.
        // (Creator key on create_database, one key per agent created on first pass.)
        let keys = user.list_keys().unwrap();
        // 1 default login key + 2 agent keys from first bootstrap.
        assert_eq!(keys.len(), 3);
    }

    #[tokio::test]
    async fn bootstrap_refreshes_config_from_yaml() {
        let mut user = test_peer_user().await;
        let mut cfg = agent_cfg("chatter");
        cfg.max_iterations = Some(5);
        let config = empty_config_with_agents(vec![cfg]);
        let dbs = bootstrap_from_config(&mut user, &config).await.unwrap();
        let id = dbs["chatter"].id();

        // Bump max_iterations in yaml, re-bootstrap.
        let mut cfg2 = agent_cfg("chatter");
        cfg2.max_iterations = Some(99);
        let config2 = empty_config_with_agents(vec![cfg2]);
        let dbs2 = bootstrap_from_config(&mut user, &config2).await.unwrap();

        // Same DB, refreshed config.
        assert_eq!(dbs2["chatter"].id(), id);
        assert_eq!(
            dbs2["chatter"].read_config().await.unwrap().max_iterations,
            Some(99)
        );
    }
}
