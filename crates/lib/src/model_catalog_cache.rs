//! Per-machine cache of live backend model catalogs, backed by the
//! `chaz_peer` eidetica DB. One DocStore (`model_catalog`) keyed by backend
//! identifier; each value is a JSON-encoded `CachedCatalog` capturing the
//! fetched models and the wall-clock time the fetch completed.
//!
//! Used by the TUI model picker so chaz can show the full live OpenRouter
//! catalog (hundreds of models) without a network round-trip on every open.
//! TTL is the caller's choice (see `CachedCatalog::is_fresh`).

use crate::backends::{BackendManager, ModelInfo};
use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const STORE: &str = "model_catalog";

/// Canonical cache key for a backend set: `backends-v2:{sorted,names}`.
///
/// One key holds that backend-set's entire catalog as a single value, so a
/// refetch logically overwrites rather than accumulating per-model keys. The
/// `v2` prefix lets the shape evolve without colliding with older entries.
/// Both the TUI picker (writer) and the runtime warm (reader) derive the key
/// here so they always agree on it.
pub fn cache_key(backend: &BackendManager) -> String {
    let mut names = backend.list_known_backends();
    names.sort();
    if names.is_empty() {
        "backends-v2:".to_string()
    } else {
        format!("backends-v2:{}", names.join(","))
    }
}

#[derive(Clone)]
pub struct ModelCatalogCache {
    db: Database,
}

/// The persisted catalog: `ModelInfo` serializes directly (it derives serde),
/// so there's no parallel "cached" shape to keep in sync. The on-disk JSON
/// field names match `ModelInfo`, so entries written by the older
/// `CachedModel` representation load unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCatalog {
    pub fetched_at: DateTime<Utc>,
    pub models: Vec<ModelInfo>,
}

impl CachedCatalog {
    pub fn is_fresh(&self, ttl: chrono::Duration) -> bool {
        Utc::now().signed_duration_since(self.fetched_at) < ttl
    }

    pub fn into_models(self) -> Vec<ModelInfo> {
        self.models
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

    /// Extract `(model id → context_window)` for the given backend set from
    /// whatever catalog was last persisted, keeping only models that declare
    /// a window. Empty if no catalog has been fetched yet on this machine.
    ///
    /// Freshness is deliberately *not* checked: a model's context window is a
    /// near-static property (unlike pricing), and a slightly-stale window is
    /// still far better than the model-blind static default it replaces.
    /// Feeds [`BackendManager::set_catalog_windows`].
    pub async fn context_windows(&self, backend: &BackendManager) -> HashMap<String, u32> {
        let Some(catalog) = self.get(&cache_key(backend)).await else {
            return HashMap::new();
        };
        catalog
            .models
            .into_iter()
            .filter_map(|m| m.context_window.map(|w| (m.id, w)))
            .collect()
    }

    /// Persist the catalog under `backend_id`, stamping `fetched_at = now`.
    pub async fn put(&self, backend_id: &str, models: Vec<ModelInfo>) -> anyhow::Result<()> {
        let catalog = CachedCatalog {
            fetched_at: Utc::now(),
            models,
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
                        context_window: Some(200_000),
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
        assert_eq!(models[0].context_window, Some(200_000));
        assert_eq!(models[1].price_input, None);
        assert_eq!(models[1].context_window, None);
        assert!(models[1].input_modalities.is_empty());
    }

    #[tokio::test]
    async fn context_windows_extracts_declared_windows() {
        use crate::backends::BackendManager;
        use crate::config::{Backend, BackendType};
        use crate::security::SecretStore;

        let (_inst, _user, db) = fresh_db().await;
        let cache = ModelCatalogCache::new(db.clone());

        let mut b = Backend::new(BackendType::OpenAICompatible);
        b.name = Some("openai".to_string());
        let secrets = SecretStore::new(db).await;
        let backend = BackendManager::new(&Some(vec![b]), secrets);

        // No catalog persisted yet -> empty, never panics.
        assert!(cache.context_windows(&backend).await.is_empty());

        cache
            .put(
                &cache_key(&backend),
                vec![
                    ModelInfo {
                        id: "has/window".into(),
                        context_window: Some(200_000),
                        ..Default::default()
                    },
                    ModelInfo {
                        id: "no/window".into(),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();

        let windows = cache.context_windows(&backend).await;
        // Only the model that declares a window appears.
        assert_eq!(windows.len(), 1);
        assert_eq!(windows.get("has/window"), Some(&200_000));
        assert_eq!(windows.get("no/window"), None);
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
