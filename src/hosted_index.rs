//! In-memory peer-local indices of hosted Agent and Memory Bank DBs.
//!
//! Stage 3 of the Database Layout Refactor. Replaces the persistent
//! `db_registry::DbRegistry` (a chaz_group DocStore mirror) with a derived
//! in-memory cache built once at startup by walking
//! `eidetica::user::User::databases()` — which is eidetica's authoritative
//! list of every DB the user holds keys for. Each entry's `meta.kind`
//! marker (Stage 4 — see [`crate::db_kind`]) tells the walker which bucket
//! the entry belongs in.
//!
//! Why in-memory: routing reads hit this on every session entry, and
//! "ownership = key possession" means the source of truth is already
//! eidetica's key store. Mirroring it into the group DB's `agents` /
//! `memory_banks` DocStores added a second source that could drift; an
//! in-memory cache built from eidetica's list at boot can't.
//!
//! The cache is mutable at runtime: `/agent new`, `/agent delete`,
//! `/memory new`, `/memory delete`, `/agent import`, `/memory import` all
//! call `register` / `unregister` to keep the cache in sync without
//! restarting. Cross-peer sync of *new* keys mid-runtime is not handled
//! here — same gap as the prior `DbRegistry` mirror.

use eidetica::auth::crypto::PublicKey;
use eidetica::entry::ID;
use eidetica::user::User;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, info, warn};

/// One row in a hosted-DBs index. Same shape for agents and memory banks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DbEntry {
    pub db_id: ID,
    pub display_name: String,
    pub pubkey: PublicKey,
}

/// Peer-local in-memory index of hosted entity DBs of one kind (agents OR
/// memory banks). Cheap to clone (`Arc` over the inner state).
#[derive(Clone)]
pub struct HostedIndex {
    /// Used only in log messages (e.g. "agent", "bank").
    label: &'static str,
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    by_id: HashMap<ID, DbEntry>,
    by_name: HashMap<String, DbEntry>,
    by_pubkey: HashMap<PublicKey, DbEntry>,
}

impl HostedIndex {
    pub fn empty(label: &'static str) -> Self {
        Self {
            label,
            inner: Arc::new(RwLock::new(Inner::default())),
        }
    }

    /// Insert or overwrite an entry. Keyed by `db_id` across all three maps;
    /// if a stale entry with the same `db_id` exists, its old `display_name`
    /// and `pubkey` indices are evicted before the new ones go in.
    pub fn register(&self, entry: DbEntry) {
        let mut inner = self.inner.write().expect("HostedIndex lock poisoned");
        if let Some(prev) = inner.by_id.get(&entry.db_id).cloned() {
            inner.by_name.remove(&prev.display_name);
            inner.by_pubkey.remove(&prev.pubkey);
        }
        inner.by_id.insert(entry.db_id.clone(), entry.clone());
        inner
            .by_name
            .insert(entry.display_name.clone(), entry.clone());
        inner.by_pubkey.insert(entry.pubkey.clone(), entry.clone());
        debug!(
            kind = self.label,
            name = %entry.display_name,
            db_id = %entry.db_id,
            "Registered in hosted index"
        );
    }

    /// Remove an entry by `db_id`. Missing entries are silently ignored.
    pub fn unregister(&self, db_id: &ID) {
        let mut inner = self.inner.write().expect("HostedIndex lock poisoned");
        if let Some(entry) = inner.by_id.remove(db_id) {
            inner.by_name.remove(&entry.display_name);
            inner.by_pubkey.remove(&entry.pubkey);
            debug!(kind = self.label, db_id = %db_id, "Unregistered from hosted index");
        }
    }

    pub fn find_by_id(&self, db_id: &ID) -> Option<DbEntry> {
        self.inner
            .read()
            .expect("HostedIndex lock poisoned")
            .by_id
            .get(db_id)
            .cloned()
    }

    pub fn find_by_name(&self, name: &str) -> Option<DbEntry> {
        self.inner
            .read()
            .expect("HostedIndex lock poisoned")
            .by_name
            .get(name)
            .cloned()
    }

    pub fn find_by_pubkey(&self, pubkey: &PublicKey) -> Option<DbEntry> {
        self.inner
            .read()
            .expect("HostedIndex lock poisoned")
            .by_pubkey
            .get(pubkey)
            .cloned()
    }

    pub fn list(&self) -> Vec<DbEntry> {
        self.inner
            .read()
            .expect("HostedIndex lock poisoned")
            .by_id
            .values()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("HostedIndex lock poisoned")
            .by_id
            .len()
    }
}

/// Walk `user.databases()`, classify each tracked DB by its `meta.kind`
/// marker, and return a fully populated `(agents, banks)` pair. Anything
/// without a recognized marker (chaz_group, chaz_peer, sessions, pre-Stage-4
/// DBs) is skipped.
///
/// O(n) DB opens — fine at chaz scale (dozens of agents/banks). If the
/// installation grows past that, eidetica's `TrackedDatabase` could carry a
/// `properties` map so we read everything off the user's own catalog
/// without opening each DB.
pub async fn build_from_user(user: &User) -> anyhow::Result<(HostedIndex, HostedIndex)> {
    let agents = HostedIndex::empty("agent");
    let banks = HostedIndex::empty("bank");

    let tracked = user.databases().await?;
    for td in tracked {
        let database = match user.open_database(&td.database_id).await {
            Ok(db) => db,
            Err(e) => {
                warn!(db_id = %td.database_id, "Skipping tracked DB: open failed: {e}");
                continue;
            }
        };
        let Some((kind, display_name)) = crate::db_kind::read_marker(&database).await else {
            continue;
        };
        let entry = DbEntry {
            db_id: td.database_id.clone(),
            display_name,
            pubkey: td.key_id.clone(),
        };
        match kind.as_str() {
            crate::db_kind::KIND_AGENT => agents.register(entry),
            crate::db_kind::KIND_BANK => banks.register(entry),
            crate::db_kind::KIND_SESSION => {} // Sessions aren't cached here.
            other => {
                warn!(db_id = %td.database_id, kind = %other, "Unknown entity kind, skipping");
            }
        }
    }

    info!(
        agents = agents.len(),
        banks = banks.len(),
        "Built hosted indices from user.databases()"
    );

    Ok((agents, banks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_db::{create_agent_db, AgentDbConfig, AgentMeta};
    use crate::memory_bank_db::{create_memory_bank, MemoryBankMeta};
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;

    async fn fresh_user() -> (Instance, eidetica::user::User) {
        let instance = Instance::open(Box::new(InMemory::new())).await.unwrap();
        let _ = instance.create_user("t", None).await;
        let user = instance.login_user("t", None).await.unwrap();
        (instance, user)
    }

    #[tokio::test]
    async fn register_then_lookup_round_trip() {
        let index = HostedIndex::empty("agent");
        let (_inst, mut user) = fresh_user().await;
        let cfg = AgentDbConfig::default();
        let meta = AgentMeta {
            display_name: Some("alpha".into()),
            ..Default::default()
        };
        let (db, pubkey) = create_agent_db(&mut user, "alpha", &cfg, &meta)
            .await
            .unwrap();
        let entry = DbEntry {
            db_id: db.id(),
            display_name: "alpha".into(),
            pubkey: pubkey.clone(),
        };
        index.register(entry.clone());

        assert_eq!(index.find_by_id(&db.id()), Some(entry.clone()));
        assert_eq!(index.find_by_name("alpha"), Some(entry.clone()));
        assert_eq!(index.find_by_pubkey(&pubkey), Some(entry));
        assert_eq!(index.len(), 1);
    }

    #[tokio::test]
    async fn unregister_evicts_all_three_keys() {
        let index = HostedIndex::empty("agent");
        let (_inst, mut user) = fresh_user().await;
        let cfg = AgentDbConfig::default();
        let meta = AgentMeta {
            display_name: Some("alpha".into()),
            ..Default::default()
        };
        let (db, pubkey) = create_agent_db(&mut user, "alpha", &cfg, &meta)
            .await
            .unwrap();
        index.register(DbEntry {
            db_id: db.id(),
            display_name: "alpha".into(),
            pubkey: pubkey.clone(),
        });
        index.unregister(&db.id());
        assert!(index.find_by_id(&db.id()).is_none());
        assert!(index.find_by_name("alpha").is_none());
        assert!(index.find_by_pubkey(&pubkey).is_none());
    }

    #[tokio::test]
    async fn build_from_user_classifies_by_kind() {
        let (_inst, mut user) = fresh_user().await;
        let cfg = AgentDbConfig::default();
        let agent_meta = AgentMeta {
            display_name: Some("alpha".into()),
            ..Default::default()
        };
        let _ = create_agent_db(&mut user, "alpha", &cfg, &agent_meta)
            .await
            .unwrap();
        let bank_meta = MemoryBankMeta {
            display_name: Some("patrick".into()),
            ..Default::default()
        };
        let _ = create_memory_bank(&mut user, "patrick", &bank_meta)
            .await
            .unwrap();

        let (agents, banks) = build_from_user(&user).await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(banks.len(), 1);
        assert!(agents.find_by_name("alpha").is_some());
        assert!(banks.find_by_name("patrick").is_some());
    }

    #[tokio::test]
    async fn register_overwrites_stale_name_index() {
        let index = HostedIndex::empty("agent");
        let (_inst, mut user) = fresh_user().await;
        let cfg = AgentDbConfig::default();
        let (db1, pk1) = create_agent_db(
            &mut user,
            "alpha",
            &cfg,
            &AgentMeta {
                display_name: Some("alpha".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        index.register(DbEntry {
            db_id: db1.id(),
            display_name: "alpha".into(),
            pubkey: pk1,
        });

        // Re-register the same db_id under a new name; the old name index
        // should evict.
        let (_db2, pk2) = create_agent_db(
            &mut user,
            "renamed",
            &cfg,
            &AgentMeta {
                display_name: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        index.register(DbEntry {
            db_id: db1.id(),
            display_name: "renamed".into(),
            pubkey: pk2,
        });
        assert!(index.find_by_name("alpha").is_none());
        assert!(index.find_by_name("renamed").is_some());
        assert_eq!(index.len(), 1);
    }
}
