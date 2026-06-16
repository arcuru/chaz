use eidetica::Database;
use eidetica::store::DocStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
/// - Persistent eidetica `DocStore` (`credentials` subtree on `chaz_peer`)
/// - On startup: load from DocStore, reconcile with config, update if changed
/// - `insert()` writes to both cache and DocStore
///
/// Backed by the `chaz_peer` DB (peer-local, never syncs) — these are
/// third-party API tokens for this binary, not anything that should propagate.
#[derive(Clone)]
pub struct SecretStore {
    cache: Arc<RwLock<HashMap<String, String>>>,
    database: Database,
}

impl SecretStore {
    /// Create a new SecretStore backed by the given eidetica database.
    /// Loads any existing secrets from the "credentials" DocStore into memory.
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
        let Ok(store) = txn.get_store::<DocStore>("credentials").await else {
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
            Ok(txn) => match txn.get_store::<DocStore>("credentials").await {
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

// ---------------------------------------------------------------------------
// Login-secrets unlock key (local disk, never synced)
//
// Each agent's transport-login credentials live in an encrypted
// `PasswordStore<DocStore>` on its synced DB; the password that unlocks that
// store is kept here, on local disk only, so the ciphertext can propagate to
// peers while the key never does. Losing the key file makes the agent's stored
// secrets unrecoverable — re-seed from yaml or re-add via the runtime command.
// ---------------------------------------------------------------------------

/// Filesystem location of an agent's login-secrets unlock key:
/// `<state_dir>/agents/<agent>/login_secrets.key`.
pub fn login_unlock_key_path(state_dir: &Path, agent_name: &str) -> PathBuf {
    state_dir
        .join("agents")
        .join(sanitize_agent_name(agent_name))
        .join("login_secrets.key")
}

/// Read the agent's login-secrets unlock key, generating and persisting a
/// fresh random one (0600 on Unix) on first use. Idempotent: an existing
/// non-empty key file is returned as-is.
pub fn ensure_login_unlock_key(state_dir: &Path, agent_name: &str) -> std::io::Result<String> {
    let path = login_unlock_key_path(state_dir, agent_name);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let key = generate_unlock_key()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

/// 32 bytes of OS randomness, hex-encoded (64 chars).
fn generate_unlock_key() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| std::io::Error::other(format!("OS RNG unavailable: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Map an agent name to a filesystem-safe directory segment.
fn sanitize_agent_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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
        // SAFETY: tests are single-threaded per #[test]; the var name is
        // CHAZ_TEST_SECRET_1, scoped to this test only.
        unsafe { std::env::set_var("CHAZ_TEST_SECRET_1", "from-env") };
        assert_eq!(
            SecretStore::resolve_env("${CHAZ_TEST_SECRET_1}").unwrap(),
            "from-env"
        );
        unsafe { std::env::remove_var("CHAZ_TEST_SECRET_1") };
    }

    #[test]
    fn test_resolve_env_dollar() {
        // SAFETY: tests are single-threaded per #[test]; the var name is
        // CHAZ_TEST_SECRET_2, scoped to this test only.
        unsafe { std::env::set_var("CHAZ_TEST_SECRET_2", "also-from-env") };
        assert_eq!(
            SecretStore::resolve_env("$CHAZ_TEST_SECRET_2").unwrap(),
            "also-from-env"
        );
        unsafe { std::env::remove_var("CHAZ_TEST_SECRET_2") };
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

    #[test]
    fn unlock_key_generates_then_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let k1 = ensure_login_unlock_key(dir.path(), "ava").unwrap();
        assert_eq!(k1.len(), 64, "expected 32 hex-encoded bytes");
        assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
        // Second call reads the persisted key rather than regenerating.
        let k2 = ensure_login_unlock_key(dir.path(), "ava").unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn unlock_key_is_per_agent() {
        let dir = tempfile::tempdir().unwrap();
        let a = ensure_login_unlock_key(dir.path(), "ava").unwrap();
        let b = ensure_login_unlock_key(dir.path(), "chaz").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn unlock_key_path_shape_and_sanitization() {
        let p = login_unlock_key_path(Path::new("/state"), "research/orchestrator");
        assert_eq!(
            p,
            Path::new("/state/agents/research_orchestrator/login_secrets.key")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unlock_key_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        ensure_login_unlock_key(dir.path(), "ava").unwrap();
        let path = login_unlock_key_path(dir.path(), "ava");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
