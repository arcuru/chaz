use eidetica::Database;
use eidetica::store::DocStore;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{error, info};

/// Centralized secret storage backed by eidetica DocStore.
///
/// Secrets are referenced by opaque IDs and only materialized at host
/// boundaries (HTTP client creation). Never serialized into LLM context.
///
/// **Not encrypted.** Secrets are stored in plaintext in the eidetica SQLite
/// database. The security boundary here is keeping secrets out of the LLM
/// data flow, not protecting them at rest. For encrypted storage, this could
/// be upgraded to eidetica's `PasswordStore<DocStore>` in the future.
///
/// Architecture:
/// - In-memory `HashMap` cache for fast sync reads (`get()`)
/// - Persistent eidetica `DocStore` ("secrets" subtree) for durability
/// - On startup: load from DocStore, reconcile with config, update if changed
/// - `insert()` writes to both cache and DocStore
#[derive(Clone)]
pub struct SecretStore {
    cache: Arc<RwLock<HashMap<String, String>>>,
    database: Database,
}

impl SecretStore {
    /// Create a new SecretStore backed by the given eidetica database.
    /// Loads any existing secrets from the "secrets" DocStore into memory.
    pub async fn new(database: Database) -> Self {
        let store = Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            database,
        };
        store.load_from_db().await;
        store
    }

    /// Load all secrets from the eidetica DocStore into the in-memory cache.
    async fn load_from_db(&self) {
        let Ok(txn) = self.database.new_transaction().await else {
            error!("Failed to create transaction for loading secrets");
            return;
        };
        let Ok(store) = txn.get_store::<DocStore>("secrets").await else {
            // First run — no secrets subtree yet, that's fine
            return;
        };
        let Ok(doc) = store.get_all().await else {
            return;
        };

        let mut cache = self.cache.write().expect("SecretStore lock poisoned");
        let mut count = 0;
        for (key, value) in doc.iter() {
            if let Ok(s) = value.try_into() {
                let s: String = s;
                cache.insert(key.clone(), s);
                count += 1;
            }
        }
        if count > 0 {
            info!("Loaded {count} secrets from store");
        }
    }

    /// Look up a secret by reference ID. Sync — reads from in-memory cache.
    pub fn get(&self, id: &str) -> Option<String> {
        self.cache
            .read()
            .expect("SecretStore lock poisoned")
            .get(id)
            .cloned()
    }

    /// Store a secret. Updates the in-memory cache immediately and persists
    /// to the eidetica DocStore. Only writes to the store if the value changed.
    pub async fn insert(&self, id: String, value: String) {
        // Check if value actually changed
        let changed = {
            let mut cache = self.cache.write().expect("SecretStore lock poisoned");
            let old = cache.get(&id);
            if old.is_some_and(|v| v == &value) {
                false
            } else {
                cache.insert(id.clone(), value.clone());
                true
            }
        };

        if !changed {
            return;
        }

        // Persist to eidetica DocStore
        match self.database.new_transaction().await {
            Ok(txn) => match txn.get_store::<DocStore>("secrets").await {
                Ok(store) => {
                    if let Err(e) = store.set_string(&id, &value).await {
                        error!("Failed to persist secret '{id}': {e}");
                    } else if let Err(e) = txn.commit().await {
                        error!("Failed to commit secret '{id}': {e}");
                    }
                }
                Err(e) => error!("Failed to open secrets store: {e}"),
            },
            Err(e) => error!("Failed to create transaction for secret: {e}"),
        }
    }

    /// Resolve a config value that may be an environment variable reference.
    ///
    /// - `"${VAR_NAME}"` or `"$VAR_NAME"` → reads the environment variable
    /// - Anything else → returned as-is (literal value)
    pub fn resolve_env(raw: &str) -> Result<String, String> {
        let trimmed = raw.trim();
        if let Some(var) = trimmed.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
            std::env::var(var).map_err(|_| format!("Environment variable '{var}' not set"))
        } else if let Some(var) = trimmed.strip_prefix('$') {
            if var.is_empty() {
                return Ok(raw.to_string());
            }
            std::env::var(var).map_err(|_| format!("Environment variable '{var}' not set"))
        } else {
            Ok(raw.to_string())
        }
    }
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.cache.read().map(|s| s.len()).unwrap_or(0);
        write!(f, "SecretStore({} secrets)", count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: tests that need eidetica would require a test database setup.
    // These tests cover the sync/env-resolution parts only.

    #[test]
    fn test_resolve_env_literal() {
        assert_eq!(
            SecretStore::resolve_env("plain-value").unwrap(),
            "plain-value"
        );
    }

    #[test]
    fn test_resolve_env_dollar_brace() {
        std::env::set_var("CHAZ_TEST_SECRET_1", "from-env");
        assert_eq!(
            SecretStore::resolve_env("${CHAZ_TEST_SECRET_1}").unwrap(),
            "from-env"
        );
        std::env::remove_var("CHAZ_TEST_SECRET_1");
    }

    #[test]
    fn test_resolve_env_dollar() {
        std::env::set_var("CHAZ_TEST_SECRET_2", "also-from-env");
        assert_eq!(
            SecretStore::resolve_env("$CHAZ_TEST_SECRET_2").unwrap(),
            "also-from-env"
        );
        std::env::remove_var("CHAZ_TEST_SECRET_2");
    }

    #[test]
    fn test_resolve_env_missing() {
        let result = SecretStore::resolve_env("${CHAZ_NONEXISTENT_VAR_XYZ}");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_env_bare_dollar() {
        assert_eq!(SecretStore::resolve_env("$").unwrap(), "$");
    }
}
