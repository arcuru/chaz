//! Stage 4 of the Database Layout Refactor: explicit `kind` + `display_name`
//! markers on every entity DB's `meta` store so the in-memory cache (Stage 3)
//! can classify a tracked database without inferring from `_settings.name`
//! or deserializing entity-specific JSON blobs.
//!
//! Written once at DB creation (`create_agent_db`, `create_memory_bank`,
//! `create_session`). Read by the cache builder when walking
//! `user.databases()` to decide which map (agents / banks / skip) the entry
//! belongs in and what name to file it under.
//!
//! Stored as top-level keys in the `meta` DocStore — distinct from the
//! JSON-blob `value` key the rest of `meta` uses, so adding them doesn't
//! force a schema change on `AgentMeta` / `MemoryBankMeta` / `SessionMeta`.

use eidetica::Database;
use eidetica::auth::crypto::PublicKey;
use eidetica::store::DocStore;

pub const META_STORE: &str = "meta";
pub const KIND_KEY: &str = "kind";
pub const DISPLAY_NAME_KEY: &str = "display_name";

/// Top-level key on an agent DB's `meta` store naming the peer that should
/// run agent-owned Fresh timer fires (where no session yet exists to carry
/// a per-session `home_pubkey`). For interactive turns and Pinned timer
/// fires the gate uses `AgentRef.home_pubkey` on the session instead.
pub const HOME_PUBKEY_KEY: &str = "home_pubkey";

pub const KIND_AGENT: &str = "agent";
/// Memory bank — peer-hosted, granted to agents for shared remember/recall.
pub const KIND_BANK: &str = "bank";
/// Skill bank — peer-hosted, granted to agents for shared skill prompts.
pub const KIND_SKILL_BANK: &str = "skill_bank";
pub const KIND_SESSION: &str = "session";

/// Write `kind` and `display_name` into the database's `meta` store in one
/// transaction. Idempotent — overwrites any prior values.
pub async fn write_marker(
    database: &Database,
    kind: &str,
    display_name: &str,
) -> anyhow::Result<()> {
    let txn = database.new_transaction().await?;
    let store = txn.get_store::<DocStore>(META_STORE).await?;
    store.set_string(KIND_KEY, kind).await?;
    store.set_string(DISPLAY_NAME_KEY, display_name).await?;
    txn.commit().await?;
    Ok(())
}

/// Read the (`kind`, `display_name`) marker pair from a database's `meta`
/// store. Returns `None` if either field is missing — i.e. the database was
/// created before Stage 4 or isn't a chaz entity DB (chaz_group / chaz_peer).
pub async fn read_marker(database: &Database) -> Option<(String, String)> {
    let txn = database.new_transaction().await.ok()?;
    let store = txn.get_store::<DocStore>(META_STORE).await.ok()?;
    let kind = store.get_string(KIND_KEY).await.ok()?;
    let display_name = store.get_string(DISPLAY_NAME_KEY).await.ok()?;
    Some((kind, display_name))
}

/// Write the agent-level `home_pubkey` into an agent DB's `meta` store.
/// Names the peer that should run `TimerTarget::Fresh` timer fires for
/// this agent. Idempotent — overwrites any prior value.
pub async fn write_agent_home_pubkey(database: &Database, pk: &PublicKey) -> anyhow::Result<()> {
    let txn = database.new_transaction().await?;
    let store = txn.get_store::<DocStore>(META_STORE).await?;
    store.set_string(HOME_PUBKEY_KEY, pk.to_string()).await?;
    txn.commit().await?;
    Ok(())
}

/// Remove the agent-level `home_pubkey` from an agent DB's `meta` store,
/// restoring the legacy "any keyholder runs Fresh fires" default. Operator
/// escape hatch for the rare case where a stuck home pubkey needs clearing.
pub async fn clear_agent_home_pubkey(database: &Database) -> anyhow::Result<()> {
    let txn = database.new_transaction().await?;
    let store = txn.get_store::<DocStore>(META_STORE).await?;
    let _ = store.delete(HOME_PUBKEY_KEY).await;
    txn.commit().await?;
    Ok(())
}

/// Read the agent-level `home_pubkey` from an agent DB's `meta` store.
/// Returns `None` if unset OR if the stored value fails to parse — in the
/// latter case the gate falls back to legacy "any keyholder runs" behavior
/// (safer than going silent on corruption).
pub async fn read_agent_home_pubkey(database: &Database) -> Option<PublicKey> {
    let txn = database.new_transaction().await.ok()?;
    let store = txn.get_store::<DocStore>(META_STORE).await.ok()?;
    let raw = store.get_string(HOME_PUBKEY_KEY).await.ok()?;
    PublicKey::from_prefixed_string(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};

    async fn fresh_db() -> (Instance, eidetica::user::User, Database) {
        let (instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("t"))
                .await
                .unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = eidetica::crdt::Doc::new();
        s.set("name", "x");
        let db = user.create_database(s, &key).await.unwrap();
        (instance, user, db)
    }

    #[tokio::test]
    async fn write_then_read_returns_marker() {
        let (_inst, _user, db) = fresh_db().await;
        write_marker(&db, KIND_AGENT, "alpha").await.unwrap();
        assert_eq!(
            read_marker(&db).await,
            Some((KIND_AGENT.to_string(), "alpha".to_string()))
        );
    }

    #[tokio::test]
    async fn read_on_db_without_marker_returns_none() {
        let (_inst, _user, db) = fresh_db().await;
        assert!(read_marker(&db).await.is_none());
    }

    #[tokio::test]
    async fn write_then_read_home_pubkey_round_trips() {
        let (_inst, user, db) = fresh_db().await;
        let pk = user.get_default_key().unwrap();
        write_agent_home_pubkey(&db, &pk).await.unwrap();
        assert_eq!(read_agent_home_pubkey(&db).await.as_ref(), Some(&pk));
    }

    #[tokio::test]
    async fn clear_home_pubkey_restores_none() {
        let (_inst, user, db) = fresh_db().await;
        let pk = user.get_default_key().unwrap();
        write_agent_home_pubkey(&db, &pk).await.unwrap();
        clear_agent_home_pubkey(&db).await.unwrap();
        assert!(read_agent_home_pubkey(&db).await.is_none());
    }

    #[tokio::test]
    async fn read_home_pubkey_on_unset_db_returns_none() {
        let (_inst, _user, db) = fresh_db().await;
        assert!(read_agent_home_pubkey(&db).await.is_none());
    }
}
