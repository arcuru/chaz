//! Per-machine store of pulled `ModelInfo` for the models actually **in use**,
//! backed by the `chaz_peer` eidetica DB. One DocStore (`model_info`) with a
//! single key (`in_use`) holding a JSON `{ model_id -> ModelInfo }` map.
//!
//! This is deliberately *not* a catalog cache. The TUI picker pulls the full
//! live provider catalog (hundreds of models) into memory for browsing; only
//! the model you switch to — or one the runtime actually uses — is persisted
//! here. So the store stays tiny (bounded by the handful of distinct models a
//! peer uses, not the whole catalog), which is all the runtime needs to budget
//! context windows window-aware at startup without a network round-trip.
//!
//! Storing only in-use models also sidesteps the append-only-growth problem of
//! the eidetica DB: a full-catalog blob rewritten on every refresh would
//! accrete forever, whereas this map is rewritten only when a genuinely new
//! model enters use (see [`ModelInfoStore::put`], which no-ops on unchanged
//! entries). See `docs/src/design/model_info_store.md`.

use crate::backends::ModelInfo;
use eidetica::store::{DocStore, StoreError};
use eidetica::{Database, Snapshot, Transaction};
use std::collections::BTreeMap;

const STORE: &str = "model_info";
const KEY: &str = "in_use";

pub type ModelInfos = BTreeMap<String, ModelInfo>;

#[derive(Clone)]
pub struct ModelInfoStore {
    db: Database,
}

impl ModelInfoStore {
    /// Wrap the `chaz_peer` database. Reads/writes go to the `model_info`
    /// DocStore on that DB — created lazily on first write.
    pub fn new(chaz_peer: Database) -> Self {
        Self { db: chaz_peer }
    }

    /// Read the exact delivered frontier. An absent key is empty; a broken
    /// store, transport failure or invalid value must not erase last-good info.
    pub async fn read_at(&self, snapshot: &Snapshot) -> anyhow::Result<ModelInfos> {
        Self::read_in(&self.db.new_transaction_at(snapshot).await?).await
    }

    async fn read_in(txn: &Transaction) -> anyhow::Result<ModelInfos> {
        let store = txn.get_store::<DocStore>(STORE).await?;
        match store.get_string(KEY).await {
            Ok(raw) => Ok(serde_json::from_str(&raw)?),
            Err(eidetica::Error::Store(error))
                if matches!(error.as_ref(), StoreError::KeyNotFound { store, key }
                    if store == STORE && key == KEY) =>
            {
                Ok(ModelInfos::new())
            }
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(test)]
    async fn all(&self) -> ModelInfos {
        self.read_at(&self.db.snapshot().await.unwrap())
            .await
            .unwrap()
    }

    /// Upsert one model's info into the in-use set. Writes nothing — and so
    /// appends nothing to the append-only DB — when the stored entry already
    /// equals `info`, so re-using an already-cached model never causes churn.
    pub async fn put(&self, info: &ModelInfo) -> anyhow::Result<()> {
        let txn = self.db.new_transaction().await?;
        let mut map = Self::read_in(&txn).await?;
        if map.get(&info.id) == Some(info) {
            return Ok(());
        }
        map.insert(info.id.clone(), info.clone());
        let json = serde_json::to_string(&map)?;
        let store = txn.get_store::<DocStore>(STORE).await?;
        store.set_string(KEY, &json).await?;
        txn.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewAccount};

    // Returns the `Instance` and `AccountSession` alongside the DB so callers keep them
    // alive — the `Database` borrows the instance backend, which is dropped
    // (and the DB invalidated: "Instance has been dropped") if they fall.
    async fn fresh_db() -> (Instance, eidetica::AccountSession, Database) {
        let (instance, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::with_password("t", "test-password"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let mut settings = eidetica::crdt::Doc::new();
        settings.set("name", "chaz_peer");
        let db = user.create_database(settings, &key).await.unwrap();
        (instance, user, db)
    }

    fn model(id: &str, window: Option<u32>) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            context_window: window,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn empty_store_reads_empty() {
        let (_inst, _user, db) = fresh_db().await;
        let store = ModelInfoStore::new(db);
        assert!(store.all().await.is_empty());
    }

    #[tokio::test]
    async fn put_then_read_back_with_slashed_ids() {
        let (_inst, _user, db) = fresh_db().await;
        let store = ModelInfoStore::new(db);
        // Model ids carry `/` and `:` — confirm they round-trip as map keys.
        store
            .put(&model("deepseek/deepseek-v4-pro", Some(128_000)))
            .await
            .unwrap();
        store
            .put(&model("inclusionai/ring-2.6-1t:free", None))
            .await
            .unwrap();

        let all = store.all().await;
        assert_eq!(all.len(), 2);
        assert_eq!(
            all.get("deepseek/deepseek-v4-pro").unwrap().context_window,
            Some(128_000)
        );

        assert_eq!(all["inclusionai/ring-2.6-1t:free"].context_window, None);
    }

    #[tokio::test]
    async fn put_upserts_and_skips_unchanged() {
        let (_inst, _user, db) = fresh_db().await;
        let store = ModelInfoStore::new(db);
        let m = model("a/b", Some(1000));
        store.put(&m).await.unwrap();
        // Re-putting the identical entry is a no-op (no panic, still one entry).
        store.put(&m).await.unwrap();
        assert_eq!(store.all().await.len(), 1);
        // A changed window replaces the prior value.
        store.put(&model("a/b", Some(2000))).await.unwrap();
        assert_eq!(
            store.all().await.get("a/b").unwrap().context_window,
            Some(2000)
        );
        assert_eq!(store.all().await.len(), 1);
    }
}
