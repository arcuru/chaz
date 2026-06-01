//! Per-machine cache of live backend model catalogs, backed by the
//! `chaz_peer` eidetica DB. One DocStore (`model_catalog`) keyed by backend
//! identifier; each value is a JSON-encoded `CachedCatalog` capturing the
//! fetched models and the wall-clock time the fetch completed.
//!
//! Used by the TUI model picker so chaz can show the full live OpenRouter
//! catalog (hundreds of models) without a network round-trip on every open.
//! TTL is the caller's choice (see `CachedCatalog::is_fresh`).

use crate::backends::ModelInfo;
use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};

const STORE: &str = "model_catalog";

#[derive(Clone)]
pub struct ModelCatalogCache {
    db: Database,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCatalog {
    pub fetched_at: DateTime<Utc>,
    pub models: Vec<CachedModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModel {
    pub id: String,
    #[serde(default)]
    pub price_input: Option<f64>,
    #[serde(default)]
    pub price_output: Option<f64>,
    #[serde(default)]
    pub price_cache_read: Option<f64>,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
}

impl CachedCatalog {
    pub fn is_fresh(&self, ttl: chrono::Duration) -> bool {
        Utc::now().signed_duration_since(self.fetched_at) < ttl
    }

    pub fn into_models(self) -> Vec<ModelInfo> {
        self.models
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id,
                price_input: m.price_input,
                price_output: m.price_output,
                price_cache_read: m.price_cache_read,
                input_modalities: m.input_modalities,
                output_modalities: m.output_modalities,
            })
            .collect()
    }
}

impl ModelCatalogCache {
    /// Wrap the `chaz_peer` database. Reads/writes go to the `model_catalog`
    /// DocStore on that DB — created lazily on first write.
    pub fn new(chaz_peer: Database) -> Self {
        Self { db: chaz_peer }
    }

    /// Read the cached catalog for `backend_id`, or `None` if no entry has
    /// been written yet (or the entry is unreadable for any reason).
    /// Freshness is the caller's check.
    pub async fn get(&self, backend_id: &str) -> Option<CachedCatalog> {
        let txn = self.db.new_transaction().await.ok()?;
        let store = txn.get_store::<DocStore>(STORE).await.ok()?;
        let raw = store.get_string(backend_id).await.ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Persist the catalog under `backend_id`, stamping `fetched_at = now`.
    pub async fn put(&self, backend_id: &str, models: Vec<ModelInfo>) -> anyhow::Result<()> {
        let catalog = CachedCatalog {
            fetched_at: Utc::now(),
            models: models
                .into_iter()
                .map(|m| CachedModel {
                    id: m.id,
                    price_input: m.price_input,
                    price_output: m.price_output,
                    price_cache_read: m.price_cache_read,
                    input_modalities: m.input_modalities,
                    output_modalities: m.output_modalities,
                })
                .collect(),
        };
        let json = serde_json::to_string(&catalog)?;
        let txn = self.db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE).await?;
        store.set_string(backend_id, &json).await?;
        txn.commit().await?;
        Ok(())
    }
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
        let mut settings = eidetica::crdt::Doc::new();
        settings.set("name", "chaz_peer");
        let db = user.create_database(settings, &key).await.unwrap();
        (instance, user, db)
    }

    #[tokio::test]
    async fn round_trip() {
        let (_inst, _user, db) = fresh_db().await;
        let cache = ModelCatalogCache::new(db);
        assert!(cache.get("openrouter").await.is_none());
        cache
            .put(
                "openrouter",
                vec![
                    ModelInfo {
                        id: "a/b".into(),
                        price_input: Some(1.5),
                        price_output: Some(7.5),
                        price_cache_read: Some(0.15),
                        input_modalities: vec!["text".into(), "image".into()],
                        output_modalities: vec!["text".into()],
                    },
                    ModelInfo {
                        id: "c/d".into(),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        let got = cache.get("openrouter").await.expect("cache hit");
        assert!(got.is_fresh(chrono::Duration::minutes(1)));
        let models = got.into_models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "a/b");
        assert_eq!(models[0].price_input, Some(1.5));
        assert_eq!(models[0].price_output, Some(7.5));
        assert_eq!(models[0].price_cache_read, Some(0.15));
        assert_eq!(models[0].input_modalities, vec!["text", "image"]);
        assert_eq!(models[1].price_input, None);
        assert!(models[1].input_modalities.is_empty());
    }

    #[tokio::test]
    async fn staleness() {
        let (_inst, _user, db) = fresh_db().await;
        let cache = ModelCatalogCache::new(db);
        cache.put("x", vec![]).await.unwrap();
        let got = cache.get("x").await.unwrap();
        // Always-stale TTL of 0 should report not-fresh even immediately
        // after write.
        assert!(!got.is_fresh(chrono::Duration::zero()));
        assert!(got.is_fresh(chrono::Duration::hours(24)));
    }
}
