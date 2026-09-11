//! Shared Eidetica connection setup for Chaz processes.
//!
//! This module only connects, logs in, and configures owner-side Eidetica
//! sync. Chaz hooks and application runtimes are installed by later startup
//! layers so client and executor authority cannot be inferred from connection
//! ownership.

use eidetica::sync::DatabaseTicket;
use eidetica::sync::transports::http::HttpTransport;
use eidetica::sync::transports::iroh::IrohTransport;
use eidetica::{Instance, user::User};

use crate::config::{Config, EideticaConfig, EideticaLoginConfig, ExecutionRole};

/// Whether this process owns the embedded backend or uses a daemon-owned one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceOwnership {
    Direct,
    Service,
}

/// Capabilities startup code may install for this connection and role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceCapabilities {
    pub ownership: InstanceOwnership,
    pub execution: ExecutionRole,
}

/// Proof that startup may install an agent-execution runtime.
///
/// Runtime constructors can require this token as they migrate onto the
/// connector, making client-mode execution a checked boundary rather than a
/// convention based on the calling binary.
#[derive(Debug)]
pub struct ExecutorCapability {
    _private: (),
}

impl InstanceCapabilities {
    pub fn administers_backend(self) -> bool {
        self.ownership == InstanceOwnership::Direct
    }

    pub fn administers_sync(self) -> bool {
        self.ownership == InstanceOwnership::Direct
    }

    pub fn runs_agents(self) -> bool {
        self.execution == ExecutionRole::Executor
    }

    pub fn executor(self) -> Result<ExecutorCapability, ConnectError> {
        self.runs_agents()
            .then_some(ExecutorCapability { _private: () })
            .ok_or(ConnectError::Capability(
                "client role cannot install agent execution or autonomous routines",
            ))
    }
}

/// A strictly connected Eidetica instance and its existing logged-in user.
pub struct ConnectedInstance {
    pub instance: Instance,
    pub user: User,
    pub capabilities: InstanceCapabilities,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("invalid Eidetica configuration: {0}")]
    Config(String),
    #[error("failed to connect to Eidetica: {0}")]
    Connection(#[source] eidetica::Error),
    #[error("failed to log in to Eidetica user `{username}`: {source}")]
    Login {
        username: String,
        #[source]
        source: eidetica::Error,
    },
    #[error("failed to configure direct Eidetica sync: {0}")]
    Sync(#[source] eidetica::Error),
    #[error("Eidetica capability denied: {0}")]
    Capability(&'static str),
}

/// Validate the required connector settings at the new API boundary.
pub fn required_settings(
    config: &Config,
) -> Result<(&EideticaConfig, ExecutionRole), ConnectError> {
    let eidetica = config.eidetica.as_ref().ok_or_else(|| {
        ConnectError::Config(
            "missing `eidetica`; configure `connection` and `login` explicitly".into(),
        )
    })?;
    let execution = config.execution.ok_or_else(|| {
        ConnectError::Config("missing required `execution: executor | client`".into())
    })?;
    validate(eidetica)?;
    Ok((eidetica, execution))
}

/// Connect from the complete Chaz config without installing application hooks.
pub async fn connect(config: &Config) -> Result<ConnectedInstance, ConnectError> {
    let (settings, execution) = required_settings(config)?;
    connect_with(settings, execution).await
}

/// Connect from already-selected settings without installing application hooks.
pub async fn connect_with(
    config: &EideticaConfig,
    execution: ExecutionRole,
) -> Result<ConnectedInstance, ConnectError> {
    validate(config)?;
    let service_connection = is_service_connection(&config.connection);
    if service_connection && config.sync.is_some() {
        return Err(ConnectError::Config(
            "`eidetica.sync` is owner-only and must be omitted for a service connection; configure sync on the Eidetica daemon".into(),
        ));
    }

    // Strict connect is load-only for every native target. Provisioning uses
    // Eidetica's explicit create APIs and never happens as connector fallback.
    let instance = Instance::connect(&config.connection)
        .await
        .map_err(ConnectError::Connection)?;
    let ownership = if service_connection {
        InstanceOwnership::Service
    } else {
        InstanceOwnership::Direct
    };

    let user = login(&instance, &config.login).await?;
    if let Some(sync_config) = &config.sync {
        configure_sync(&instance, sync_config).await?;
    }

    Ok(ConnectedInstance {
        instance,
        user,
        capabilities: InstanceCapabilities {
            ownership,
            execution,
        },
    })
}

fn validate(config: &EideticaConfig) -> Result<(), ConnectError> {
    if config.connection.trim().is_empty() {
        return Err(ConnectError::Config(
            "`eidetica.connection` must not be empty".into(),
        ));
    }
    if config.login.username.trim().is_empty() {
        return Err(ConnectError::Config(
            "`eidetica.login.username` must not be empty".into(),
        ));
    }
    match (&config.login.password, config.login.passwordless) {
        (Some(_), true) => Err(ConnectError::Config(
            "set either `eidetica.login.password` or `passwordless: true`, not both".into(),
        )),
        (None, false) => Err(ConnectError::Config(
            "set `eidetica.login.password` or explicitly opt into `passwordless: true`".into(),
        )),
        _ => Ok(()),
    }
}

fn is_service_connection(connection: &str) -> bool {
    connection
        .split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("unix"))
}

async fn login(instance: &Instance, config: &EideticaLoginConfig) -> Result<User, ConnectError> {
    instance
        .login_user(&config.username, config.password.as_deref())
        .await
        .map_err(|source| ConnectError::Login {
            username: config.username.clone(),
            source,
        })
}

async fn configure_sync(
    instance: &Instance,
    config: &crate::config::EideticaSyncConfig,
) -> Result<(), ConnectError> {
    instance.enable_sync().await.map_err(ConnectError::Sync)?;
    let sync = instance.sync().ok_or_else(|| {
        ConnectError::Config("direct Eidetica instance did not expose its sync handle".into())
    })?;

    if config.iroh {
        sync.register_transport("iroh", IrohTransport::builder())
            .await
            .map_err(ConnectError::Sync)?;
    }
    if let Some(address) = &config.http_listen {
        sync.register_transport("http", HttpTransport::builder().bind(address))
            .await
            .map_err(ConnectError::Sync)?;
    }
    if config.iroh || config.http_listen.is_some() {
        sync.accept_connections()
            .await
            .map_err(ConnectError::Sync)?;
    }
    for raw_ticket in &config.tickets {
        let ticket = raw_ticket
            .parse::<DatabaseTicket>()
            .map_err(ConnectError::Sync)?;
        sync.sync_with_ticket(&ticket)
            .await
            .map_err(ConnectError::Sync)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use eidetica::NewUser;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;
    use eidetica::service::ServiceServer;
    use eidetica::store::DocStore;
    use tokio::sync::watch;

    use super::*;
    use crate::config::EideticaSyncConfig;

    fn settings(connection: String, username: &str) -> EideticaConfig {
        EideticaConfig {
            connection,
            login: EideticaLoginConfig {
                username: username.into(),
                password: None,
                passwordless: true,
            },
            sync: None,
        }
    }

    async fn populated_snapshot(username: &str) -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eidetica.json");
        let (instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless(username))
                .await
                .unwrap();
        let identity = user.get_default_key().unwrap().to_string();
        let named_key = user.add_private_key(Some("agent-key")).await.unwrap();
        let mut db_settings = Doc::new();
        db_settings.set("name", "legacy-agent");
        let db = user.create_database(db_settings, &named_key).await.unwrap();
        db.with_transaction(|tx| async move {
            tx.get_store::<DocStore>("fixture")
                .await?
                .set("value", "preserved")
                .await
        })
        .await
        .unwrap();
        let root = db.root_id().to_string();
        instance.snapshot_to_path(&path).unwrap();
        (dir, identity, root)
    }

    async fn service_fixture(
        username: &str,
    ) -> (
        tempfile::TempDir,
        watch::Sender<()>,
        tokio::task::JoinHandle<eidetica::Result<()>>,
        Instance,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("service.sock");
        let (instance, _) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless(username))
                .await
                .unwrap();
        let server = ServiceServer::bind(instance.clone(), &socket)
            .await
            .unwrap();
        let (shutdown, receiver) = watch::channel(());
        let task = tokio::spawn(server.run(receiver));
        let url = format!("unix://{}", socket.display());
        (dir, shutdown, task, instance, url)
    }

    #[test]
    fn required_boundary_rejects_missing_and_mixed_settings() {
        let missing = Config::default();
        assert!(
            required_settings(&missing)
                .unwrap_err()
                .to_string()
                .contains("missing `eidetica`")
        );

        let yaml = r#"
execution: executor
eidetica:
  connection: memory://
  login:
    username: chaz
    password: secret
    passwordless: true
"#;
        let mixed: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(
            required_settings(&mixed)
                .unwrap_err()
                .to_string()
                .contains("not both")
        );

        let no_role: Config = serde_yaml::from_str(
            "eidetica:\n  connection: memory://\n  login:\n    username: chaz\n    passwordless: true\n",
        )
        .unwrap();
        assert!(
            required_settings(&no_role)
                .unwrap_err()
                .to_string()
                .contains("execution")
        );
    }

    #[test]
    fn config_parses_roles_targets_and_native_sync_settings() {
        let yaml = r#"
execution: client
eidetica:
  connection: postgres://user@db/chaz
  login:
    username: shared-chaz
    password: secret
  sync:
    iroh: true
    http_listen: 127.0.0.1:8765
    tickets: ["eidetica:?db=sha256:00"]
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let (eidetica, role) = required_settings(&config).unwrap();
        assert_eq!(role, ExecutionRole::Client);
        assert_eq!(eidetica.connection, "postgres://user@db/chaz");
        let sync = eidetica.sync.as_ref().unwrap();
        assert!(sync.iroh);
        assert_eq!(sync.http_listen.as_deref(), Some("127.0.0.1:8765"));
        assert!(!format!("{:?}", eidetica.login).contains("secret"));
    }

    #[test]
    fn config_accepts_every_native_connection_target_without_shadow_validation() {
        for connection in [
            "sqlite://./chaz.db",
            "postgres://user@db/chaz",
            "memory:///var/lib/chaz/eidetica.json",
            "unix:///run/eidetica/service.sock",
        ] {
            let yaml = format!(
                "execution: executor\neidetica:\n  connection: {connection}\n  login:\n    username: chaz\n    passwordless: true\n"
            );
            let config: Config = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(required_settings(&config).unwrap().0.connection, connection);
        }
    }

    #[tokio::test]
    async fn direct_connect_preserves_legacy_identity_data_and_named_key() {
        let (_dir, identity, root) = populated_snapshot("legacy-chaz").await;
        let path = _dir.path().join("eidetica.json");
        let connected = connect_with(
            &settings(format!("memory://{}", path.display()), "legacy-chaz"),
            ExecutionRole::Executor,
        )
        .await
        .unwrap();

        assert_eq!(connected.capabilities.ownership, InstanceOwnership::Direct);
        assert!(connected.capabilities.runs_agents());
        assert_eq!(
            connected.user.get_default_key().unwrap().to_string(),
            identity
        );
        let root = eidetica::entry::ID::parse(&root).unwrap();
        let key = connected.user.find_keys_by_display_name("agent-key")[0].clone();
        let db = connected
            .user
            .open_database_with_key(&root, &key)
            .await
            .unwrap();
        let store = db.get_store_viewer::<DocStore>("fixture").await.unwrap();
        assert_eq!(store.get_string("value").await.unwrap(), "preserved");
    }

    #[tokio::test]
    async fn strict_direct_connect_never_provisions_or_falls_back() {
        let error = connect_with(&settings("memory://".into(), "chaz"), ExecutionRole::Client)
            .await
            .err()
            .expect("an empty direct target must not be provisioned");
        assert!(error.to_string().contains("not initialised"));
    }

    #[tokio::test]
    async fn unsupported_target_is_rejected_by_eidetica_without_sqlite_fallback() {
        let error = connect_with(
            &settings("mysql://localhost/chaz".into(), "chaz"),
            ExecutionRole::Client,
        )
        .await
        .err()
        .expect("unsupported targets must fail");
        assert!(error.to_string().contains("did you mean `postgres://`"));
    }

    #[tokio::test]
    async fn sqlite_target_preserves_existing_login_and_rejects_an_empty_store() {
        let populated = tempfile::tempdir().unwrap();
        let path = populated.path().join("eidetica.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let backend = eidetica::backend::database::Sqlite::connect(&url)
            .await
            .unwrap();
        let (created, user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("legacy-chaz"))
                .await
                .unwrap();
        let identity = user.get_default_key().unwrap();
        drop(user);
        drop(created);

        let connected = connect_with(&settings(url, "legacy-chaz"), ExecutionRole::Client)
            .await
            .unwrap();
        assert_eq!(connected.capabilities.ownership, InstanceOwnership::Direct);
        assert_eq!(connected.user.get_default_key().unwrap(), identity);
        drop(connected);

        let empty = tempfile::tempdir().unwrap();
        let empty_path = empty.path().join("empty.db");
        let empty_url = format!("sqlite://{}?mode=rwc", empty_path.display());
        drop(
            eidetica::backend::database::Sqlite::connect(&empty_url)
                .await
                .unwrap(),
        );
        let strict_url = format!("sqlite://{}", empty_path.display());
        let error = connect_with(&settings(strict_url, "chaz"), ExecutionRole::Client)
            .await
            .err()
            .expect("an uninitialised SQLite target must be rejected");
        assert!(
            error.to_string().contains("not initialised"),
            "unexpected strict-connect error: {error:?}"
        );
    }

    #[tokio::test]
    async fn bad_direct_login_is_actionable() {
        let (_dir, _, _) = populated_snapshot("real-user").await;
        let path = _dir.path().join("eidetica.json");
        let error = connect_with(
            &settings(format!("memory://{}", path.display()), "wrong-user"),
            ExecutionRole::Client,
        )
        .await
        .err()
        .expect("an unknown direct login must be rejected");
        assert!(matches!(error, ConnectError::Login { .. }));
        assert!(error.to_string().contains("wrong-user"));
    }

    #[test]
    fn required_role_is_enforced_by_an_unforgeable_executor_capability() {
        let direct_client = InstanceCapabilities {
            ownership: InstanceOwnership::Direct,
            execution: ExecutionRole::Client,
        };
        assert!(matches!(
            direct_client.executor(),
            Err(ConnectError::Capability(_))
        ));

        let service_executor = InstanceCapabilities {
            ownership: InstanceOwnership::Service,
            execution: ExecutionRole::Executor,
        };
        service_executor.executor().unwrap();
        assert!(!service_executor.administers_sync());
    }

    #[tokio::test]
    async fn service_clients_share_login_and_open_per_database_keys() {
        let (_dir, shutdown, task, server, url) = service_fixture("shared-chaz").await;
        let mut owner_user = server.login_user("shared-chaz", None).await.unwrap();
        let key = owner_user.add_private_key(Some("agent-key")).await.unwrap();
        let db = owner_user.create_database(Doc::new(), &key).await.unwrap();
        let root = db.root_id().clone();

        let first = connect_with(
            &settings(url.clone(), "shared-chaz"),
            ExecutionRole::Executor,
        )
        .await
        .unwrap();
        let second = connect_with(&settings(url, "shared-chaz"), ExecutionRole::Client)
            .await
            .unwrap();
        assert_eq!(first.capabilities.ownership, InstanceOwnership::Service);
        assert!(first.capabilities.runs_agents());
        assert!(!second.capabilities.runs_agents());
        assert!(!second.capabilities.administers_backend());
        first.capabilities.executor().unwrap();
        assert!(matches!(
            second.capabilities.executor(),
            Err(ConnectError::Capability(_))
        ));

        for connected in [&first, &second] {
            let selected = connected.user.find_keys_by_display_name("agent-key")[0].clone();
            connected
                .user
                .open_database_with_key(&root, &selected)
                .await
                .unwrap();
        }

        drop(first);
        drop(second);
        drop(shutdown);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn service_rejects_owner_settings_without_fallback_or_mutation() {
        let (_dir, shutdown, task, server, url) = service_fixture("shared-chaz").await;
        let before_identity = server.id();
        let mut config = settings(url, "shared-chaz");
        config.sync = Some(EideticaSyncConfig::default());
        let error = connect_with(&config, ExecutionRole::Client)
            .await
            .err()
            .expect("service owner settings must be rejected");
        assert!(error.to_string().contains("owner-only"));
        assert_eq!(server.id(), before_identity);
        server
            .login_user("shared-chaz", None)
            .await
            .expect("rejected client settings must leave the daemon usable");
        assert!(
            server.sync().is_none(),
            "client validation must not enable daemon sync"
        );
        drop(shutdown);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unavailable_service_and_bad_login_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let unavailable = settings(
            format!("unix://{}", dir.path().join("missing.sock").display()),
            "shared-chaz",
        );
        assert!(matches!(
            connect_with(&unavailable, ExecutionRole::Client)
                .await
                .err()
                .expect("an unavailable endpoint must fail"),
            ConnectError::Connection(_)
        ));

        let (_server_dir, shutdown, task, _server, url) = service_fixture("real-user").await;
        assert!(matches!(
            connect_with(&settings(url, "wrong-user"), ExecutionRole::Client)
                .await
                .err()
                .expect("a bad service login must fail"),
            ConnectError::Login { .. }
        ));
        drop(shutdown);
        task.await.unwrap().unwrap();
    }
}
