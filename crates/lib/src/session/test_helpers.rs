//! Shared test fixtures for the `session::*` submodules.
//!
//! Only compiled under `#[cfg(test)]` and used to avoid duplicating the
//! ~30-LOC registry setup across agents/keys/registry test modules.

use super::*;
use crate::agent::AgentRegistry;
use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
use crate::config::{AgentConfig, Config};
use crate::hosted_index::{DbEntry, HostedIndex};
use eidetica::backend::database::InMemory;
use eidetica::{Instance, NewUser};
use std::sync::Arc;

/// Fresh in-memory peer with one database ready for SessionMeta round-trip
/// tests. Returns Instance+User so they stay alive while the Database is in
/// use (dropping the Instance closes the backend and invalidates the handle).
pub(crate) async fn test_session_db() -> (Instance, eidetica::user::User, eidetica::Database) {
    let backend = InMemory::new();
    let (instance, mut user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();
    let key = user.get_default_key().unwrap();
    let mut settings = eidetica::crdt::Doc::new();
    settings.set("name", "test-session");
    let db = user.create_database(settings, &key).await.unwrap();
    (instance, user, db)
}

pub(crate) fn agent_cfg(name: &str) -> AgentConfig {
    AgentConfig {
        name: name.to_string(),
        system_prompt: Some("You are a test agent.".to_string()),
        system_prompt_files: None,
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
        default_memory_banks: None,
        default_skill_banks: None,
    }
}

/// Registry containing only the bare-bones `"default"` agent — useful
/// for attach/detach tests that don't care about AgentRegistry
/// resolution but still need `default_agent()` to succeed if hit.
pub(crate) async fn make_registry() -> (Instance, Arc<SessionRegistry>) {
    let backend = InMemory::new();
    let (instance, user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();
    let agents = Arc::new(AgentRegistry::with_default_agent());
    let registry = SessionRegistry::new(instance.clone(), user, agents)
        .await
        .unwrap();
    (instance, Arc::new(registry))
}

/// Same as [`make_registry`] but with eidetica's sync subsystem enabled.
/// Required for any helper that talks to the bootstrap-request store
/// (`pending_bootstrap_requests`, `approve_bootstrap_request`, etc.) —
/// those error out with "Sync not enabled" otherwise.
pub(crate) async fn make_registry_with_sync() -> (Instance, Arc<SessionRegistry>) {
    let backend = InMemory::new();
    let (instance, user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();
    instance.enable_sync().await.unwrap();
    let agents = Arc::new(AgentRegistry::with_default_agent());
    let registry = SessionRegistry::new(instance.clone(), user, agents)
        .await
        .unwrap();
    (instance, Arc::new(registry))
}

/// Registry with one declared agent `alpha` — routing tests can then resolve
/// display_name → Agent via AgentRegistry.
pub(crate) async fn make_registry_with_alpha_agent() -> (Instance, Arc<SessionRegistry>, HostedIndex)
{
    let backend = InMemory::new();
    let (instance, user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();

    let cfg = Config {
        agents: Some(vec![agent_cfg("alpha")]),
        ..Config::default()
    };
    let agents = Arc::new(AgentRegistry::from_config(&cfg));
    let registry = Arc::new(
        SessionRegistry::new(instance.clone(), user, agents)
            .await
            .unwrap(),
    );
    let index = HostedIndex::empty("agent");
    (instance, registry, index)
}

/// Registry with two declared agents (alpha, beta).
pub(crate) async fn make_registry_with_two_agents() -> (Instance, Arc<SessionRegistry>, HostedIndex)
{
    let backend = InMemory::new();
    let (instance, user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();

    let cfg = Config {
        agents: Some(vec![agent_cfg("alpha"), agent_cfg("beta")]),
        ..Config::default()
    };
    let agents = Arc::new(AgentRegistry::from_config(&cfg));
    let registry = Arc::new(
        SessionRegistry::new(instance.clone(), user, agents)
            .await
            .unwrap(),
    );
    let index = HostedIndex::empty("agent");
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
