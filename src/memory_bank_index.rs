//! Peer-local index of Memory Bank DBs this peer hosts — Stage 9.D.1.
//!
//! Same shape and rationale as `agent_index`: eidetica offers no
//! list-my-DBs-by-name query, and keys don't carry readable display
//! names outside the process that created them. So chaz maintains an
//! explicit `memory_banks_hosted` DocStore in the central DB with one
//! row per bank this peer hosts.
//!
//! Never synced. Peer-local bookkeeping only. A bank DB might be
//! tracked on multiple peers (everyone who's been granted access),
//! but each peer maintains its own index; no cross-peer coordination.

#![allow(dead_code)]

use eidetica::auth::crypto::PublicKey;
use eidetica::entry::ID;
use eidetica::store::DocStore;
use eidetica::Database;
use serde::{Deserialize, Serialize};
use tracing::debug;

const STORE_NAME: &str = "memory_banks_hosted";

/// One row in the memory-banks-I-host index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryBankIndexEntry {
    pub db_id: ID,
    pub display_name: String,
    pub pubkey: PublicKey,
}

/// Cheap handle over the central DB's `memory_banks_hosted` store.
#[derive(Clone)]
pub struct MemoryBankIndex {
    central_db: Database,
}

impl MemoryBankIndex {
    pub fn new(central_db: Database) -> Self {
        Self { central_db }
    }

    pub async fn register(&self, entry: MemoryBankIndexEntry) -> anyhow::Result<()> {
        let json = serde_json::to_string(&entry)?;
        let txn = self.central_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_NAME).await?;
        store.set_string(entry.db_id.to_string(), json).await?;
        txn.commit().await?;
        debug!(
            bank = %entry.display_name,
            db_id = %entry.db_id,
            "Registered bank in memory-banks-hosted index"
        );
        Ok(())
    }

    pub async fn unregister(&self, db_id: &ID) -> anyhow::Result<()> {
        let txn = self.central_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_NAME).await?;
        let _ = store.delete(db_id.to_string()).await;
        txn.commit().await?;
        debug!(db_id = %db_id, "Unregistered bank from memory-banks-hosted index");
        Ok(())
    }

    pub async fn list(&self) -> anyhow::Result<Vec<MemoryBankIndexEntry>> {
        let txn = self.central_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_NAME).await?;
        let doc = store.get_all().await?;
        let mut out = Vec::new();
        for (_key, value) in doc.iter() {
            let json: String = match value.try_into() {
                Ok(s) => s,
                Err(_) => continue,
            };
            match serde_json::from_str::<MemoryBankIndexEntry>(&json) {
                Ok(entry) => out.push(entry),
                Err(e) => tracing::warn!(error = %e, "Skipping malformed hosted-banks entry"),
            }
        }
        Ok(out)
    }

    pub async fn find_by_id(&self, db_id: &ID) -> anyhow::Result<Option<MemoryBankIndexEntry>> {
        let txn = self.central_db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_NAME).await?;
        match store.get_string(db_id.to_string()).await {
            Ok(json) => Ok(serde_json::from_str(&json).ok()),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<MemoryBankIndexEntry>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|e| e.display_name == name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_bank_db::{create_memory_bank, MemoryBankMeta};
    use eidetica::backend::database::InMemory;
    use eidetica::user::User;
    use eidetica::Instance;

    async fn peer_with_central_db() -> (User, Database) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.add_private_key(Some("central")).await.unwrap();
        let mut settings = eidetica::crdt::Doc::new();
        settings.set("name", "test-central");
        let db = user.create_database(settings, &key).await.unwrap();
        (user, db)
    }

    async fn provision_entry(user: &mut User, name: &str) -> MemoryBankIndexEntry {
        let (bank, pubkey) = create_memory_bank(
            user,
            name,
            &MemoryBankMeta {
                display_name: Some(name.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        MemoryBankIndexEntry {
            db_id: bank.id(),
            display_name: name.to_string(),
            pubkey,
        }
    }

    #[tokio::test]
    async fn register_find_and_list() {
        let (mut user, central) = peer_with_central_db().await;
        let index = MemoryBankIndex::new(central);
        let e = provision_entry(&mut user, "patrick").await;
        index.register(e.clone()).await.unwrap();

        assert_eq!(index.find_by_id(&e.db_id).await.unwrap(), Some(e.clone()));
        assert_eq!(
            index.find_by_name("patrick").await.unwrap(),
            Some(e.clone())
        );
        assert_eq!(index.list().await.unwrap(), vec![e]);
        assert!(index.find_by_name("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unregister_removes_entry() {
        let (mut user, central) = peer_with_central_db().await;
        let index = MemoryBankIndex::new(central);
        let e = provision_entry(&mut user, "patrick").await;
        index.register(e.clone()).await.unwrap();
        index.unregister(&e.db_id).await.unwrap();
        assert!(index.list().await.unwrap().is_empty());
        // Second unregister is a no-op.
        index.unregister(&e.db_id).await.unwrap();
    }
}
