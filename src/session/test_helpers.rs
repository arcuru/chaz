//! Shared test fixtures for the `session::*` submodules.
//!
//! Only compiled under `#[cfg(test)]` and used to avoid duplicating the
//! ~30-LOC registry setup across agents/keys/registry test modules.

use super::*;
use crate::agent::AgentRegistry;
use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
use crate::config::{AgentConfig, Config};
use crate::db_registry::{DbEntry, DbRegistry};
use eidetica::Instance;
use eidetica::backend::database::InMemory;
use std::sync::Arc;

/// Fresh in-memory peer with one database ready for SessionMeta round-trip
/// tests. Returns Instance+User so they stay alive while the Database is in
/// use (dropping the Instance closes the backend and invalidates the handle).
pub(crate) async fn test_session_db() -> (Instance, eidetica::user::User, eidetica::Database) {
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

pub(crate) fn blank_config() -> Config {
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
        agents: None,
        security: None,
        schedules: None,
        mcp_servers: None,
        tool_profiles: None,
        mcp_server_dir: None,
        context: None,
    }
}

pub(crate) fn agent_cfg(name: &str) -> AgentConfig {
    AgentConfig {
        name: name.to_string(),
        role: None,
        model: None,
        tools: None,
        can_spawn: None,
        allowed_callers: None,
        max_iterations: None,
        autonomous: false,
        presets: None,
        tool_profile: None,
        max_context_tokens: None,
        grants: None,
    }
}

/// Plain registry with no declared agents — useful for attach/detach tests
/// that don't care about AgentRegistry resolution.
pub(crate) async fn make_registry() -> (Instance, Arc<SessionRegistry>) {
    let backend = InMemory::new();
    let instance = Instance::open(Box::new(backend)).await.unwrap();
    let _ = instance.create_user("test", None).await;
    let user = instance.login_user("test", None).await.unwrap();
    let agents = Arc::new(AgentRegistry::from_config(&blank_config()));
    let registry = SessionRegistry::new(instance.clone(), user, agents)
        .await
        .unwrap();
    (instance, Arc::new(registry))
}

/// Registry with one declared agent `alpha` — routing tests can then resolve
/// display_name → Agent via AgentRegistry.
pub(crate) async fn make_registry_with_alpha_agent() -> (Instance, Arc<SessionRegistry>, DbRegistry)
{
    let backend = InMemory::new();
    let instance = Instance::open(Box::new(backend)).await.unwrap();
    let _ = instance.create_user("test", None).await;
    let user = instance.login_user("test", None).await.unwrap();

    let mut cfg = blank_config();
    cfg.agents = Some(vec![agent_cfg("alpha")]);
    let agents = Arc::new(AgentRegistry::from_config(&cfg));
    let registry = Arc::new(
        SessionRegistry::new(instance.clone(), user, agents)
            .await
            .unwrap(),
    );
    let index = DbRegistry::agents(registry.chazdb().clone());
    (instance, registry, index)
}

/// Registry with two declared agents (alpha, beta).
pub(crate) async fn make_registry_with_two_agents() -> (Instance, Arc<SessionRegistry>, DbRegistry)
{
    let backend = InMemory::new();
    let instance = Instance::open(Box::new(backend)).await.unwrap();
    let _ = instance.create_user("test", None).await;
    let user = instance.login_user("test", None).await.unwrap();

    let mut cfg = blank_config();
    cfg.agents = Some(vec![agent_cfg("alpha"), agent_cfg("beta")]);
    let agents = Arc::new(AgentRegistry::from_config(&cfg));
    let registry = Arc::new(
        SessionRegistry::new(instance.clone(), user, agents)
            .await
            .unwrap(),
    );
    let index = DbRegistry::agents(registry.chazdb().clone());
    (instance, registry, index)
}

/// Create a fresh Agent DB for `name` on the given registry and return a
/// `DbEntry` suitable for `attach_agent_to_session`.
pub(crate) async fn make_agent_entry(registry: &SessionRegistry, name: &str) -> DbEntry {
    let cfg = AgentDbConfig::default();
    let meta = AgentMeta {
        display_name: Some(name.to_string()),
        ..Default::default()
    };
    let mut user = registry.user.lock().await;
    let (db, pubkey) = create_agent_db(&mut user, name, &cfg, &meta).await.unwrap();
    DbEntry {
        db_id: db.id(),
        display_name: name.to_string(),
        pubkey,
    }
}
