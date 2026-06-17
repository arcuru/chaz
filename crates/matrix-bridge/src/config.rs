//! The Matrix bridge's own config file + idempotent credential seeding.
//!
//! Distinct from chaz's runtime config: the bridge owns its eidetica key, its
//! state dir, and the credentials chaz deliberately no longer holds.
//! [`MatrixBridgeConfig`] is that file's shape. Secret fields are given as
//! `${ENV}` references (or literals) and resolved via
//! [`SecretStore::resolve_env`](chaz_core::security::SecretStore::resolve_env)
//! at seed time, so the file never carries a plaintext password.
//!
//! [`MatrixBridgeConfig::seed_into`] resolves every reference and writes each
//! login's [`MatrixCredentials`] into the bridge settings DB; it's idempotent,
//! so the bridge can run it on every boot. Standing up the bridge's eidetica
//! `User`, bootstrapping access, and registering the public `LoginRef` pointer
//! are the binary's job (chaz-core `bridge_identity`); this module stops at
//! "the encrypted creds are in the bridge DB".

use crate::credentials::MatrixCredentials;
use chaz_core::bridge_db::BridgeDb;
use chaz_core::config::{LoginConfig, TransportConfig};
use chaz_core::security::SecretStore;
use serde::Deserialize;

/// The Matrix bridge's own config file.
#[derive(Debug, Clone, Deserialize)]
pub struct MatrixBridgeConfig {
    /// State directory for the bridge's eidetica DB + key material. When unset
    /// the binary falls back to a platform default.
    #[serde(default)]
    pub state_dir: Option<String>,

    /// Label for this bridge's settings DB — the `bridge:<label>` name passed
    /// to [`create_bridge_db`](chaz_core::bridge_db::create_bridge_db).
    #[serde(default = "default_label")]
    pub label: String,

    /// Password that unlocks the bridge settings DB's encrypted credentials
    /// store. A `${ENV}` reference is resolved at seed time.
    pub unlock_password: String,

    /// The per-agent logins this bridge manages.
    #[serde(default)]
    pub logins: Vec<MatrixLoginConfig>,
}

/// One login a Matrix bridge manages, tying Matrix credentials to the agent
/// that owns them. Reuses chaz-core's [`LoginConfig`] verbatim (the same
/// `type:`-tagged shape agents used to carry inline) so there's a single
/// transport schema.
#[derive(Debug, Clone, Deserialize)]
pub struct MatrixLoginConfig {
    /// Display name of the agent this login belongs to. Its AgentDb is where
    /// the public `LoginRef` pointer gets registered.
    pub agent: String,

    /// Transport identity + credentials (`type: matrix`, homeserver, username,
    /// password, allow_list, …). Secret fields may be `${ENV}` references.
    #[serde(flatten)]
    pub login: LoginConfig,
}

fn default_label() -> String {
    "matrix".to_string()
}

impl MatrixLoginConfig {
    /// Map this entry to its `(login_id, credentials)`, resolving any `${ENV}`
    /// reference in the password. The returned `login_id` is the key the
    /// credentials are stored under and matches the public `LoginRef`
    /// identifier.
    pub fn to_credentials(&self) -> anyhow::Result<(String, MatrixCredentials)> {
        let login_id = self.login.login_id().to_string();
        let password = match self.login.secret() {
            Some(raw) => Some(SecretStore::resolve_env(raw).map_err(|e| anyhow::anyhow!(e))?),
            None => None,
        };
        let creds = match &self.login.transport {
            TransportConfig::Matrix(m) => MatrixCredentials {
                homeserver_url: m.homeserver_url.clone(),
                username: m.username.clone(),
                password,
                allow_list: m.allow_list.clone(),
                room_size_limit: m.room_size_limit,
            },
        };
        Ok((login_id, creds))
    }
}

impl MatrixBridgeConfig {
    /// Resolve the settings-DB unlock password (expanding a `${ENV}` ref).
    pub fn resolve_unlock_password(&self) -> anyhow::Result<String> {
        SecretStore::resolve_env(&self.unlock_password).map_err(|e| anyhow::anyhow!(e))
    }

    /// Seed every configured login's credentials into `bridge_db`, encrypting
    /// them under the resolved unlock password. Idempotent — re-seeding a
    /// `login_id` replaces its prior value — so this is safe to run on every
    /// boot. Any unresolved `${ENV}` reference (unlock password or a login
    /// password) aborts before writing anything for that login.
    pub async fn seed_into(&self, bridge_db: &BridgeDb) -> anyhow::Result<()> {
        let unlock = self.resolve_unlock_password()?;
        for entry in &self.logins {
            let (login_id, creds) = entry.to_credentials()?;
            bridge_db
                .seed_credentials(&login_id, &creds, &unlock)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chaz_core::bridge_db::{BridgeDb, create_bridge_db};
    use eidetica::backend::database::InMemory;
    use eidetica::user::User;
    use eidetica::{Instance, NewUser};

    async fn test_bridge_db() -> (User, BridgeDb) {
        let backend = InMemory::new();
        let (_instance, mut user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
        let (db, _) = create_bridge_db(&mut user, "matrix").await.unwrap();
        (user, db)
    }

    /// Build a sample config whose unlock-password and login-password env refs
    /// use the given (test-unique) var names. Keeps tests from racing on a
    /// shared process-global env var, mirroring `security::secrets` tests.
    fn sample_with(unlock_var: &str, pw_var: &str) -> String {
        format!(
            r#"
state_dir: /var/lib/chaz-matrix
label: matrix
unlock_password: ${{{unlock_var}}}
logins:
  - agent: chaz
    type: matrix
    homeserver_url: https://matrix.example
    username: "@chaz:example"
    password: ${{{pw_var}}}
    allow_list: "@patrick:example"
"#
        )
    }

    #[test]
    fn parses_full_config() {
        let cfg: MatrixBridgeConfig = serde_yaml::from_str(&sample_with("UNLOCK", "PW")).unwrap();
        assert_eq!(cfg.state_dir.as_deref(), Some("/var/lib/chaz-matrix"));
        assert_eq!(cfg.label, "matrix");
        assert_eq!(cfg.logins.len(), 1);
        let entry = &cfg.logins[0];
        assert_eq!(entry.agent, "chaz");
        assert_eq!(entry.login.login_id(), "@chaz:example");
        assert_eq!(entry.login.transport_kind(), "matrix");
    }

    #[test]
    fn label_defaults_when_omitted() {
        let cfg: MatrixBridgeConfig =
            serde_yaml::from_str("unlock_password: literal-pw\n").unwrap();
        assert_eq!(cfg.label, "matrix");
        assert!(cfg.logins.is_empty());
    }

    #[test]
    fn to_credentials_resolves_env_secret() {
        // SAFETY: single-threaded per #[test]; var name scoped to this test.
        unsafe { std::env::set_var("CHAZ_MATRIX_PW_RESOLVE", "from-env-pw") };
        let cfg: MatrixBridgeConfig = serde_yaml::from_str(&sample_with(
            "CHAZ_MATRIX_UNLOCK_RESOLVE",
            "CHAZ_MATRIX_PW_RESOLVE",
        ))
        .unwrap();
        let (login_id, creds) = cfg.logins[0].to_credentials().unwrap();
        unsafe { std::env::remove_var("CHAZ_MATRIX_PW_RESOLVE") };

        assert_eq!(login_id, "@chaz:example");
        assert_eq!(creds.homeserver_url, "https://matrix.example");
        assert_eq!(creds.username, "@chaz:example");
        assert_eq!(creds.password.as_deref(), Some("from-env-pw"));
        assert_eq!(creds.allow_list.as_deref(), Some("@patrick:example"));
    }

    #[test]
    fn missing_env_secret_errors() {
        // This var is never set → resolution fails.
        let cfg: MatrixBridgeConfig =
            serde_yaml::from_str(&sample_with("UNLOCK", "CHAZ_MATRIX_NEVER_SET_XYZ")).unwrap();
        assert!(cfg.logins[0].to_credentials().is_err());
    }

    #[tokio::test]
    async fn seed_into_writes_then_reads_back() {
        // SAFETY: single-threaded per #[test]; var names scoped to this test.
        unsafe {
            std::env::set_var("CHAZ_MATRIX_UNLOCK_SEED", "unlock-pw");
            std::env::set_var("CHAZ_MATRIX_PW_SEED", "hunter2");
        }
        let cfg: MatrixBridgeConfig = serde_yaml::from_str(&sample_with(
            "CHAZ_MATRIX_UNLOCK_SEED",
            "CHAZ_MATRIX_PW_SEED",
        ))
        .unwrap();
        let (_user, db) = test_bridge_db().await;

        // Idempotent: seeding twice leaves a single, current value.
        cfg.seed_into(&db).await.unwrap();
        cfg.seed_into(&db).await.unwrap();

        let got: MatrixCredentials = db
            .read_credentials("@chaz:example", "unlock-pw")
            .await
            .unwrap()
            .expect("seeded");
        unsafe {
            std::env::remove_var("CHAZ_MATRIX_UNLOCK_SEED");
            std::env::remove_var("CHAZ_MATRIX_PW_SEED");
        }
        assert_eq!(got.password.as_deref(), Some("hunter2"));
        assert_eq!(got.username, "@chaz:example");
        assert_eq!(got.homeserver_url, "https://matrix.example");
    }
}
