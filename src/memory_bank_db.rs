//! Memory Bank DB primitive — Stage 9.A of the Memory Banks plan.
//!
//! A `MemoryBankDb` is a standalone eidetica `Database` owned by a
//! per-bank `PrivateKey`, holding a single `memory` Table store. Agents
//! gain access to a bank by having their pubkey added to the bank's
//! `AuthSettings` with `Read` or `Write`; access shows up as a reference
//! entry in the agent's own DB's `memory_banks` subtree (Stage 9.B).
//!
//! Shape is deliberately minimal vs. [`AgentDb`]:
//! - `memory` (Table<MemoryEntry>) — the bank's entries. Same subtree
//!   name and schema as an agent's own memory, so read/write code paths
//!   are uniform (Stage 9.C).
//! - `meta`   (DocStore) — display name + description. Kept small.
//!
//! Because an Agent DB *also* carries a `memory` subtree with the same
//! shape, any Agent DB is usable as a bank by anyone with Read on it —
//! no separate "bank" type is required for introspection.

#![allow(dead_code)]

use crate::agent_db::MemoryEntry;
use eidetica::auth::crypto::PublicKey;
use eidetica::crdt::Doc;
use eidetica::entry::ID;
use eidetica::store::{DocStore, Table};
use eidetica::user::User;
use eidetica::Database;
use serde::{Deserialize, Serialize};
use tracing::info;

pub const MEMORY_STORE: &str = "memory";
pub const META_STORE: &str = "meta";

const BLOB_KEY: &str = "value";

/// Display metadata for a memory bank. Read by `/memory list` and by the
/// dynamic tool descriptor that tells the LLM which banks it can query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryBankMeta {
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// Handle over the eidetica `Database` that holds a memory bank.
#[derive(Clone)]
pub struct MemoryBankDb {
    database: Database,
}

impl MemoryBankDb {
    pub fn from_database(database: Database) -> Self {
        Self { database }
    }

    pub fn id(&self) -> ID {
        self.database.root_id().clone()
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub async fn read_meta(&self) -> anyhow::Result<MemoryBankMeta> {
        read_blob(&self.database, META_STORE).await
    }

    pub async fn write_meta(&self, meta: &MemoryBankMeta) -> anyhow::Result<()> {
        write_blob(&self.database, META_STORE, meta).await
    }

    /// Touch every well-known store so it exists in the DB. Safe to call
    /// repeatedly; commits in one transaction.
    pub async fn ensure_stores(&self) -> anyhow::Result<()> {
        let txn = self.database.new_transaction().await?;
        txn.get_store::<Table<MemoryEntry>>(MEMORY_STORE).await?;
        txn.get_store::<DocStore>(META_STORE).await?;
        txn.commit().await?;
        Ok(())
    }
}

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

/// DB name used in eidetica settings — the idempotency key for
/// `find_memory_bank` (parallel to `agent:<display_name>`).
pub fn memory_bank_db_name(display_name: &str) -> String {
    format!("memory:{display_name}")
}

/// Create a new Memory Bank DB. Generates a fresh key on `user`, creates
/// the DB signed by that key with `name: memory:<display_name>` in
/// settings, populates `meta`, and initializes the `memory` store.
pub async fn create_memory_bank(
    user: &mut User,
    display_name: &str,
    meta: &MemoryBankMeta,
) -> anyhow::Result<(MemoryBankDb, PublicKey)> {
    let key = user
        .add_private_key(Some(&format!("memory:{display_name}")))
        .await?;
    let mut settings = Doc::new();
    settings.set("name", memory_bank_db_name(display_name).as_str());
    let database = user.create_database(settings, &key).await?;
    info!(
        bank = display_name,
        db_id = %database.root_id(),
        key = %key,
        "Created Memory Bank DB"
    );

    let bank = MemoryBankDb::from_database(database);
    bank.ensure_stores().await?;
    bank.write_meta(meta).await?;
    Ok((bank, key))
}

/// Look up an existing Memory Bank DB by display name on this peer's
/// `User`. Returns `(MemoryBankDb, pubkey)` where pubkey is the key this
/// user holds for the DB. `None` if no bank with that name is tracked.
pub async fn find_memory_bank(
    user: &User,
    display_name: &str,
) -> Option<(MemoryBankDb, PublicKey)> {
    let name = memory_bank_db_name(display_name);
    let database = user.find_database(&name).await.ok()?.into_iter().next()?;
    let pubkey = user.find_key(database.root_id()).ok().flatten()?;
    Some((MemoryBankDb::from_database(database), pubkey))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;

    async fn test_user() -> User {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        instance.login_user("test", None).await.unwrap()
    }

    #[tokio::test]
    async fn create_and_read_meta() {
        let mut user = test_user().await;
        let meta = MemoryBankMeta {
            display_name: Some("patrick".to_string()),
            description: Some("notes about Patrick".to_string()),
        };
        let (bank, pubkey) = create_memory_bank(&mut user, "patrick", &meta)
            .await
            .unwrap();

        assert_eq!(bank.read_meta().await.unwrap(), meta);
        // Returned pubkey is really the one held by the user for this DB.
        assert_eq!(user.find_key(&bank.id()).unwrap(), Some(pubkey));
    }

    #[tokio::test]
    async fn find_by_display_name() {
        let mut user = test_user().await;
        let meta = MemoryBankMeta {
            display_name: Some("projects".to_string()),
            ..Default::default()
        };
        let (created, _) = create_memory_bank(&mut user, "projects", &meta)
            .await
            .unwrap();
        let created_id = created.id();

        let (found, _) = find_memory_bank(&user, "projects").await.expect("found");
        assert_eq!(found.id(), created_id);
        assert!(find_memory_bank(&user, "nope").await.is_none());
    }

    #[tokio::test]
    async fn memory_store_schema_matches_agent_db() {
        // Bank's `memory` subtree must be Table<MemoryEntry> with the same
        // `MEMORY_STORE` name as an AgentDb so the tool code can read/write
        // either one uniformly. Verify by writing + reading an entry.
        let mut user = test_user().await;
        let (bank, _) = create_memory_bank(
            &mut user,
            "shared",
            &MemoryBankMeta {
                display_name: Some("shared".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let txn = bank.database().new_transaction().await.unwrap();
        let store = txn
            .get_store::<Table<MemoryEntry>>(MEMORY_STORE)
            .await
            .unwrap();
        store
            .insert(MemoryEntry {
                key: "hello".to_string(),
                value: "world".to_string(),
                timestamp: Utc::now(),
            })
            .await
            .unwrap();
        txn.commit().await.unwrap();

        // Read it back via a fresh transaction + same shape.
        let txn = bank.database().new_transaction().await.unwrap();
        let store = txn
            .get_store::<Table<MemoryEntry>>(MEMORY_STORE)
            .await
            .unwrap();
        let all = store.search(|_: &MemoryEntry| true).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.key, "hello");
        assert_eq!(all[0].1.value, "world");
    }

    #[tokio::test]
    async fn crate_assert_store_name_matches_agent_db() {
        // Guard the invariant: both constants resolve to the same string.
        // If someone ever divergedly renames one, this fails loudly.
        assert_eq!(MEMORY_STORE, crate::agent_db::MEMORY_STORE);
    }
}
