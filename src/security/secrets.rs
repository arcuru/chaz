use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Centralized secret storage. Secrets are referenced by opaque IDs and only
/// materialized at host boundaries (HTTP client creation). Never serialized,
/// never enters LLM context.
///
/// Uses `Arc<RwLock<..>>` so it can be shared across threads and extended at
/// runtime (e.g., when Matrix room tag backends provide API keys).
#[derive(Clone, Default)]
pub struct SecretStore {
    secrets: Arc<RwLock<HashMap<String, String>>>,
}

impl SecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a secret by reference ID. Returns a clone (never a reference
    /// into the locked map) so the lock is held only briefly.
    pub fn get(&self, id: &str) -> Option<String> {
        self.secrets
            .read()
            .expect("SecretStore lock poisoned")
            .get(id)
            .cloned()
    }

    /// Store a secret under the given reference ID.
    pub fn insert(&self, id: String, value: String) {
        self.secrets
            .write()
            .expect("SecretStore lock poisoned")
            .insert(id, value);
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
        let count = self.secrets.read().map(|s| s.len()).unwrap_or(0);
        write!(f, "SecretStore({} secrets)", count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_get() {
        let store = SecretStore::new();
        store.insert("test-key".into(), "secret-value".into());
        assert_eq!(store.get("test-key"), Some("secret-value".to_string()));
        assert_eq!(store.get("missing"), None);
    }

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
        // Just "$" alone should be treated as literal
        assert_eq!(SecretStore::resolve_env("$").unwrap(), "$");
    }
}
