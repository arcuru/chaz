//! Peer-local indices of hosted DBs — one row per Agent or Memory Bank DB
//! this peer holds a key for.
//!
//! Why this exists: eidetica has no "DBs where key K has permission P"
//! inverse query, and keys don't carry readable display names outside the
//! process that created them. Routing (pubkey → agent) and `/agent hosted`
//! / `/memory list` need O(1) lookups, so chaz maintains explicit DocStore
//! indices in the chazdb.
//!
//! Agent and bank indices are structurally identical — same row shape,
//! same read/write paths — so a single `DbRegistry` type serves both.
//! The store name (`agents` vs `memory_banks`) and a label for log
//! messages are chosen at construction.
//!
//! Never synced. Peer-local bookkeeping only.

#![allow(dead_code)]

use crate::agent_db::BootstrappedAgent;
use eidetica::auth::crypto::PublicKey;
use eidetica::entry::ID;
use eidetica::store::DocStore;
use eidetica::Database;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info};

/// One row in a hosted-DBs index. Used for both agents and memory banks —
/// same shape in both cases: the DB root ID, a display name, and the
/// pubkey this peer holds for that DB.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DbEntry {
    pub db_id: ID,
    pub display_name: String,
    pub pubkey: PublicKey,
}

/// Cheap handle over a DocStore in the chazdb. Clone-friendly;
/// eidetica handles concurrency under the hood.
#[derive(Clone)]
pub struct DbRegistry {
    chazdb: Database,
    store_name: &'static str,
    /// Used only in log messages (e.g. "agent", "bank").
    label: &'static str,
}

impl DbRegistry {
    /// Construct the index of Agent DBs hosted on this peer
    /// (`agents` store in the chazdb).
    pub fn agents(chazdb: Database) -> Self {
        Self {
            chazdb,
            store_name: "agents",
            label: "agent",
        }
    }

    /// Construct the index of Memory Bank DBs hosted on this peer
    /// (`memory_banks` store in the chazdb).
    pub fn memory_banks(chazdb: Database) -> Self {
        Self {
            chazdb,
            store_name: "memory_banks",
            label: "bank",
        }
    }

    /// Upsert one entry. Uses `db_id.to_string()` as the DocStore key.
    pub async fn register(&self, entry: DbEntry) -> anyhow::Result<()> {
        let json = serde_json::to_string(&entry)?;
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(self.store_name).await?;
        store.set_string(entry.db_id.to_string(), json).await?;
        txn.commit().await?;
        debug!(
            kind = self.label,
            name = %entry.display_name,
            db_id = %entry.db_id,
            "Registered in hosted index"
        );
        Ok(())
    }

    /// Remove an entry by db_id. Missing entries are silently ignored.
    pub async fn unregister(&self, db_id: &ID) -> anyhow::Result<()> {
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(self.store_name).await?;
        let _ = store.delete(db_id.to_string()).await;
        txn.commit().await?;
        debug!(kind = self.label, db_id = %db_id, "Unregistered from hosted index");
        Ok(())
    }

    /// Return every entry in the index.
    pub async fn list(&self) -> anyhow::Result<Vec<DbEntry>> {
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(self.store_name).await?;
        let doc = store.get_all().await?;
        let mut out = Vec::new();
        for (_key, value) in doc.iter() {
            let json: String = match value.try_into() {
                Ok(s) => s,
                Err(_) => continue,
            };
            match serde_json::from_str::<DbEntry>(&json) {
                Ok(entry) => out.push(entry),
                Err(e) => tracing::warn!(
                    kind = self.label,
                    error = %e,
                    "Skipping malformed hosted-index entry"
                ),
            }
        }
        Ok(out)
    }

    pub async fn find_by_id(&self, db_id: &ID) -> anyhow::Result<Option<DbEntry>> {
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(self.store_name).await?;
        match store.get_string(db_id.to_string()).await {
            Ok(json) => Ok(serde_json::from_str(&json).ok()),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Linear scan — N is small. If this ever grows, maintain a
    /// name → db_id secondary index.
    pub async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<DbEntry>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|e| e.display_name == name))
    }

    /// Linear scan — N is small. Stage 3 routing calls this to resolve
    /// session-participant pubkeys to the agents they represent.
    pub async fn find_by_pubkey(&self, pubkey: &PublicKey) -> anyhow::Result<Option<DbEntry>> {
        Ok(self.list().await?.into_iter().find(|e| &e.pubkey == pubkey))
    }

    /// Mirror a bootstrap result into the index. Idempotent: overwrites
    /// existing entries keyed by the same db_id.
    pub async fn sync_from_bootstrap(
        &self,
        agents: &HashMap<String, BootstrappedAgent>,
    ) -> anyhow::Result<usize> {
        let mut count = 0;
        for (name, agent) in agents {
            let entry = DbEntry {
                db_id: agent.db.id(),
                display_name: name.clone(),
                pubkey: agent.pubkey.clone(),
            };
            self.register(entry).await?;
            count += 1;
        }
        if count > 0 {
            info!(
                kind = self.label,
                count, "Synced hosted index from bootstrap"
            );
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_db::{create_agent_db, AgentDbConfig, AgentMeta, BootstrappedAgent};
    use crate::memory_bank_db::{create_memory_bank, MemoryBankMeta};
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;
    use eidetica::user::User;
    use eidetica::Instance;

    async fn test_peer_user() -> User {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        instance.login_user("test", None).await.unwrap()
    }

    async fn test_chazdb(user: &mut User) -> Database {
        let key = user.get_default_key().unwrap();
        let mut settings = Doc::new();
        settings.set("name", "test-chazdb");
        user.create_database(settings, &key).await.unwrap()
    }

    async fn make_agent(user: &mut User, name: &str) -> BootstrappedAgent {
        let cfg = AgentDbConfig {
            role: Some(name.to_string()),
            ..Default::default()
        };
        let meta = AgentMeta {
            display_name: Some(name.to_string()),
            ..Default::default()
        };
        let (db, pubkey) = create_agent_db(user, name, &cfg, &meta).await.unwrap();
        BootstrappedAgent { db, pubkey }
    }

    async fn make_bank_entry(user: &mut User, name: &str) -> DbEntry {
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
        DbEntry {
            db_id: bank.id(),
            display_name: name.to_string(),
            pubkey,
        }
    }

    #[tokio::test]
    async fn agents_register_and_find_round_trip() {
        let mut user = test_peer_user().await;
        let chazdb = test_chazdb(&mut user).await;
        let index = DbRegistry::agents(chazdb);

        let agent = make_agent(&mut user, "alpha").await;
        let entry = DbEntry {
            db_id: agent.db.id(),
            display_name: "alpha".to_string(),
            pubkey: agent.pubkey.clone(),
        };
        index.register(entry.clone()).await.unwrap();

        assert_eq!(
            index.find_by_id(&agent.db.id()).await.unwrap(),
            Some(entry.clone())
        );
        assert_eq!(
            index.find_by_name("alpha").await.unwrap(),
            Some(entry.clone())
        );
        assert_eq!(
            index.find_by_pubkey(&agent.pubkey).await.unwrap(),
            Some(entry)
        );
    }

    #[tokio::test]
    async fn list_returns_all_entries() {
        let mut user = test_peer_user().await;
        let chazdb = test_chazdb(&mut user).await;
        let index = DbRegistry::agents(chazdb);

        for name in ["alpha", "beta", "gamma"] {
            let agent = make_agent(&mut user, name).await;
            index
                .register(DbEntry {
                    db_id: agent.db.id(),
                    display_name: name.to_string(),
                    pubkey: agent.pubkey,
                })
                .await
                .unwrap();
        }

        let mut names: Vec<_> = index
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.display_name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn unregister_removes_entry() {
        let mut user = test_peer_user().await;
        let chazdb = test_chazdb(&mut user).await;
        let index = DbRegistry::agents(chazdb);

        let agent = make_agent(&mut user, "alpha").await;
        let id = agent.db.id();
        index
            .register(DbEntry {
                db_id: id.clone(),
                display_name: "alpha".to_string(),
                pubkey: agent.pubkey,
            })
            .await
            .unwrap();
        assert!(index.find_by_id(&id).await.unwrap().is_some());

        index.unregister(&id).await.unwrap();
        assert!(index.find_by_id(&id).await.unwrap().is_none());
        assert!(index.find_by_name("alpha").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unregister_missing_is_ok() {
        let mut user = test_peer_user().await;
        let chazdb = test_chazdb(&mut user).await;
        let index = DbRegistry::agents(chazdb);
        let agent = make_agent(&mut user, "alpha").await;

        index.unregister(&agent.db.id()).await.unwrap();
    }

    #[tokio::test]
    async fn sync_from_bootstrap_registers_all() {
        let mut user = test_peer_user().await;
        let chazdb = test_chazdb(&mut user).await;
        let index = DbRegistry::agents(chazdb);

        let mut agents = HashMap::new();
        agents.insert("alpha".to_string(), make_agent(&mut user, "alpha").await);
        agents.insert("beta".to_string(), make_agent(&mut user, "beta").await);

        let n = index.sync_from_bootstrap(&agents).await.unwrap();
        assert_eq!(n, 2);

        assert!(index.find_by_name("alpha").await.unwrap().is_some());
        assert!(index.find_by_name("beta").await.unwrap().is_some());
        assert_eq!(index.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sync_is_idempotent_and_overwrites() {
        let mut user = test_peer_user().await;
        let chazdb = test_chazdb(&mut user).await;
        let index = DbRegistry::agents(chazdb);

        let mut agents = HashMap::new();
        agents.insert("alpha".to_string(), make_agent(&mut user, "alpha").await);

        index.sync_from_bootstrap(&agents).await.unwrap();
        index.sync_from_bootstrap(&agents).await.unwrap();

        assert_eq!(index.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn banks_register_and_find_round_trip() {
        let mut user = test_peer_user().await;
        let chazdb = test_chazdb(&mut user).await;
        let index = DbRegistry::memory_banks(chazdb);

        let entry = make_bank_entry(&mut user, "patrick").await;
        index.register(entry.clone()).await.unwrap();

        assert_eq!(
            index.find_by_id(&entry.db_id).await.unwrap(),
            Some(entry.clone())
        );
        assert_eq!(
            index.find_by_name("patrick").await.unwrap(),
            Some(entry.clone())
        );
        assert_eq!(index.list().await.unwrap(), vec![entry]);
        assert!(index.find_by_name("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn agents_and_banks_share_chazdb_without_collision() {
        let mut user = test_peer_user().await;
        let chazdb = test_chazdb(&mut user).await;
        let agents = DbRegistry::agents(chazdb.clone());
        let banks = DbRegistry::memory_banks(chazdb);

        let a = make_agent(&mut user, "alpha").await;
        agents
            .register(DbEntry {
                db_id: a.db.id(),
                display_name: "alpha".to_string(),
                pubkey: a.pubkey,
            })
            .await
            .unwrap();

        let bank = make_bank_entry(&mut user, "alpha").await;
        banks.register(bank.clone()).await.unwrap();

        // Same display name, different stores — no cross-contamination.
        assert_eq!(agents.list().await.unwrap().len(), 1);
        assert_eq!(banks.list().await.unwrap().len(), 1);
        assert_eq!(
            banks.find_by_name("alpha").await.unwrap().unwrap().db_id,
            bank.db_id
        );
    }
}
