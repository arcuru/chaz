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

const SERVICE_RECONNECT_BASE: std::time::Duration = std::time::Duration::from_millis(250);
const SERVICE_RECONNECT_MAX: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether this process owns the embedded backend or uses a daemon-owned one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceOwnership {
    Direct,
    Service,
}

/// Capabilities startup code may install for this connection and role.
///
/// The fields are private so callers cannot mint an executor capability without
/// going through [`connect`] or [`connect_with`].
///
/// ```compile_fail
/// use chaz_core::config::ExecutionRole;
/// use chaz_core::instance::{InstanceCapabilities, InstanceOwnership};
///
/// let _ = InstanceCapabilities {
///     ownership: InstanceOwnership::Direct,
///     execution: ExecutionRole::Executor,
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceCapabilities {
    ownership: InstanceOwnership,
    execution: ExecutionRole,
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

/// Replaceable server handle used by tools and extensions. A generation owns
/// exactly one strong server reference; clearing it before reconnect prevents
/// stale tasks from reaching a replacement runtime.
#[derive(Clone, Default)]
pub struct ServerSlot(
    std::sync::Arc<std::sync::RwLock<Option<std::sync::Arc<crate::server::Server>>>>,
);

impl ServerSlot {
    pub fn set(&self, server: std::sync::Arc<crate::server::Server>) {
        *self.0.write().expect("server slot lock poisoned") = Some(server);
    }

    pub fn get(&self) -> Option<std::sync::Arc<crate::server::Server>> {
        self.0.read().expect("server slot lock poisoned").clone()
    }

    pub fn clear(&self) {
        self.0.write().expect("server slot lock poisoned").take();
    }
}

#[cfg(test)]
impl ExecutorCapability {
    pub(crate) fn for_test() -> Self {
        Self { _private: () }
    }
}

pub(crate) fn executor_for_role(role: ExecutionRole) -> Result<ExecutorCapability, ConnectError> {
    (role == ExecutionRole::Executor)
        .then_some(ExecutorCapability { _private: () })
        .ok_or(ConnectError::Capability(
            "client role cannot install agent execution or autonomous routines",
        ))
}

impl InstanceCapabilities {
    pub fn ownership(self) -> InstanceOwnership {
        self.ownership
    }

    pub fn execution(self) -> ExecutionRole {
        self.execution
    }

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
        executor_for_role(self.execution)
    }
}

#[cfg(test)]
pub(crate) fn capabilities_for_test(
    config: &EideticaConfig,
    execution: ExecutionRole,
) -> InstanceCapabilities {
    InstanceCapabilities {
        ownership: if is_service_connection(&config.connection) {
            InstanceOwnership::Service
        } else {
            InstanceOwnership::Direct
        },
        execution,
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

impl ConnectError {
    /// Only transport-level service failures are worth reconnecting.
    /// Configuration, login, capability, and direct-owner failures require an
    /// operator decision and must not become an infinite retry loop.
    pub fn is_transient_service(&self) -> bool {
        matches!(self, Self::Connection(error) if error.is_io_error())
    }
}

/// Exponential service reconnect delay, bounded so a daemon returning after a
/// long outage is noticed promptly. `attempt` is zero-based.
pub fn service_reconnect_delay(attempt: u32) -> std::time::Duration {
    let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
    SERVICE_RECONNECT_BASE
        .saturating_mul(factor)
        .min(SERVICE_RECONNECT_MAX)
}

/// A cheap authenticated read used by long-lived service clients to detect a
/// dead socket. It does not mutate state and is deliberately separate from
/// reconnect: callers must tear down their complete application runtime before
/// constructing a replacement.
pub async fn probe_service(instance: &Instance) -> Result<(), ConnectError> {
    let connection = instance.remote_connection().ok_or_else(|| {
        ConnectError::Config("service probe requires a `unix://` Eidetica connection".into())
    })?;
    connection
        .get_instance_metadata()
        .await
        .map(|_| ())
        .map_err(ConnectError::Connection)
}

/// One application generation supervised over a service connection: the
/// runtime built on one connected [`Instance`], torn down whole before a
/// replacement is connected.
#[allow(async_fn_in_trait)]
pub trait ServiceGeneration: Sized {
    /// The connection this generation was built on; probed for liveness.
    fn instance(&self) -> Instance;

    /// Resolves when the generation ends on its own — a transport task
    /// exiting, for example. A generation that only ends on request stays
    /// pending forever.
    async fn finished(&mut self) -> anyhow::Result<()>;

    /// Stop every task, hook, and subscription this generation owns. Runs to
    /// completion before a replacement is constructed.
    async fn shutdown(self);
}

impl ServiceGeneration for crate::server::BuiltServer {
    fn instance(&self) -> Instance {
        self.registry.instance().clone()
    }

    async fn finished(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }

    async fn shutdown(self) {
        crate::server::BuiltServer::shutdown(self).await;
    }
}

/// Whether a rebuild failure is worth another bounded-backoff attempt. Only
/// transport-level service failures are; a configuration, login, or
/// capability error surfaces to the operator instead of looping.
fn is_transient_rebuild_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ConnectError>()
        .is_some_and(ConnectError::is_transient_service)
}

/// Supervise a service-connected application generation until shutdown.
///
/// The connection is probed periodically. On a transient service failure the
/// current generation is shut down completely, then `rebuild` is retried with
/// [`service_reconnect_delay`] between attempts until it yields a replacement
/// that reconnected, re-logged-in, reopened its databases, reinstalled its
/// hooks, and reconciled persisted state. `shutdown` resolves when the process
/// is asked to stop; it is honoured while probing and while waiting to
/// reconnect. A generation that finishes on its own ends supervision with its
/// result after its own teardown.
pub async fn supervise_service<G, Rebuild, RebuildFuture, Shutdown, ShutdownFuture>(
    mut generation: G,
    mut rebuild: Rebuild,
    shutdown: Shutdown,
) -> anyhow::Result<()>
where
    G: ServiceGeneration,
    Rebuild: FnMut() -> RebuildFuture,
    RebuildFuture: std::future::Future<Output = anyhow::Result<G>>,
    Shutdown: Fn() -> ShutdownFuture,
    ShutdownFuture: std::future::Future<Output = ()>,
{
    let mut reconnect_attempt = 0u32;
    loop {
        let instance = generation.instance();
        let probe = async {
            probe_service(&instance).await?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Ok::<(), ConnectError>(())
        };
        tokio::select! {
            _ = shutdown() => {
                generation.shutdown().await;
                return Ok(());
            }
            result = generation.finished() => {
                generation.shutdown().await;
                return result;
            }
            result = probe => match result {
                Ok(()) => reconnect_attempt = 0,
                Err(error) if error.is_transient_service() => {
                    tracing::warn!(%error, "Eidetica service disconnected; tearing down runtime before reconnect");
                    generation = replace_service_generation(
                        generation,
                        |old| async move { old.shutdown().await },
                        || async {
                            loop {
                                let delay = service_reconnect_delay(reconnect_attempt);
                                reconnect_attempt = reconnect_attempt.saturating_add(1);
                                tokio::select! {
                                    _ = shutdown() => anyhow::bail!("shutdown requested"),
                                    _ = tokio::time::sleep(delay) => {}
                                }
                                match rebuild().await {
                                    Ok(replacement) => {
                                        tracing::info!("Eidetica service reconnected; runtime reopened and reconciled");
                                        return Ok(replacement);
                                    }
                                    Err(next) if is_transient_rebuild_error(&next) => {
                                        tracing::warn!(error = %next, "Eidetica service reconnect failed");
                                    }
                                    Err(next) => return Err(next),
                                }
                            }
                        },
                    )
                    .await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// Shut the old generation down to completion, then construct its
/// replacement. Split out so the ordering is testable without a service.
pub async fn replace_service_generation<T, Shutdown, ShutdownFuture, Reconnect, ReconnectFuture>(
    generation: T,
    shutdown: Shutdown,
    reconnect: Reconnect,
) -> anyhow::Result<T>
where
    Shutdown: FnOnce(T) -> ShutdownFuture,
    ShutdownFuture: std::future::Future<Output = ()>,
    Reconnect: FnOnce() -> ReconnectFuture,
    ReconnectFuture: std::future::Future<Output = anyhow::Result<T>>,
{
    shutdown(generation).await;
    reconnect().await
}

/// Block until the process is asked to stop. Ctrl-C covers foreground and
/// container use; SIGTERM is what systemd and a test harness's teardown send,
/// and without it a stop degrades into the kill that follows the timeout.
pub async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to install SIGTERM handler, Ctrl-C only: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
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
    validate_strict_target(config)?;

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

/// Reject absent local targets before handing them to native drivers. Some
/// drivers create schema/files while opening, but runtime connection is
/// load-only: provisioning must be an explicit Eidetica operation.
fn validate_strict_target(config: &EideticaConfig) -> Result<(), ConnectError> {
    let connection = config.connection.trim();
    let Some((scheme, rest)) = connection.split_once("://") else {
        return Ok(());
    };
    if !scheme.eq_ignore_ascii_case("sqlite") {
        return Ok(());
    }

    let path = rest.split('?').next().unwrap_or(rest);
    if path.is_empty() || path == ":memory:" || connection.contains("mode=memory") {
        return Ok(());
    }
    if !std::path::Path::new(path).exists() {
        return Err(ConnectError::Config(format!(
            "SQLite Eidetica store `{path}` does not exist; initialise it explicitly before starting Chaz"
        )));
    }
    Ok(())
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
        .trim()
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
  connection: postgres://user:db-secret@db/chaz
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
        assert_eq!(eidetica.connection, "postgres://user:db-secret@db/chaz");
        let sync = eidetica.sync.as_ref().unwrap();
        assert!(sync.iroh);
        assert_eq!(sync.http_listen.as_deref(), Some("127.0.0.1:8765"));
        let debug = format!("{eidetica:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("user:db-secret"));
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

    #[test]
    fn service_detection_ignores_surrounding_whitespace_and_case() {
        assert!(is_service_connection("unix:///run/eidetica.sock"));
        assert!(is_service_connection("UNIX:///run/eidetica.sock"));
        assert!(is_service_connection("  unix:///run/eidetica.sock  "));
        assert!(!is_service_connection("sqlite://./chaz.db"));
    }

    #[test]
    fn strict_sqlite_validation_does_not_create_a_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        let config = settings(format!("sqlite://{}", path.display()), "chaz");
        let error = validate_strict_target(&config).unwrap_err();
        assert!(error.to_string().contains("initialise it explicitly"));
        assert!(!path.exists());
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

        assert_eq!(
            connected.capabilities.ownership(),
            InstanceOwnership::Direct
        );
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
        assert_eq!(
            connected.capabilities.ownership(),
            InstanceOwnership::Direct
        );
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

    #[tokio::test]
    async fn required_role_is_enforced_by_an_unforgeable_executor_capability() {
        let (client_dir, _, _) = populated_snapshot("client").await;
        let direct_client = connect_with(
            &settings(
                format!(
                    "memory://{}",
                    client_dir.path().join("eidetica.json").display()
                ),
                "client",
            ),
            ExecutionRole::Client,
        )
        .await
        .unwrap();
        assert_eq!(
            direct_client.capabilities.execution(),
            ExecutionRole::Client
        );
        assert!(matches!(
            direct_client.capabilities.executor(),
            Err(ConnectError::Capability(_))
        ));

        let (_server_dir, shutdown, task, _server, url) = service_fixture("executor").await;
        let service_executor = connect_with(&settings(url, "executor"), ExecutionRole::Executor)
            .await
            .unwrap();
        assert_eq!(
            service_executor.capabilities.execution(),
            ExecutionRole::Executor
        );
        service_executor.capabilities.executor().unwrap();
        assert!(!service_executor.capabilities.administers_sync());
        drop(service_executor);
        drop(shutdown);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn service_clients_share_login_and_open_per_database_keys() {
        let (_dir, shutdown, task, server, url) = service_fixture("shared-chaz").await;
        let mut owner_user = server.login_user("shared-chaz", None).await.unwrap();
        let key = owner_user.add_private_key(Some("agent-key")).await.unwrap();
        let db = owner_user.create_database(Doc::new(), &key).await.unwrap();
        let root = db.root_id().clone();
        let default_key = owner_user.get_default_key().unwrap();
        let callback_db = owner_user
            .create_database(Doc::new(), &default_key)
            .await
            .unwrap();
        let callback_root = callback_db.root_id().clone();

        let first = connect_with(
            &settings(url.clone(), "shared-chaz"),
            ExecutionRole::Executor,
        )
        .await
        .unwrap();
        let second = connect_with(&settings(url, "shared-chaz"), ExecutionRole::Client)
            .await
            .unwrap();
        assert_eq!(first.capabilities.ownership(), InstanceOwnership::Service);
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

        let observer_db = second.user.open_database(&callback_root).await.unwrap();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let _callback = observer_db
            .on_write(move |_, _| {
                let event_tx = event_tx.clone();
                async move {
                    event_tx.send(()).unwrap();
                    Ok(())
                }
            })
            .await
            .unwrap();
        let writer_db = first.user.open_database(&callback_root).await.unwrap();
        writer_db
            .with_transaction(|tx| async move {
                tx.get_store::<DocStore>("fixture")
                    .await?
                    .set("from", "first-client")
                    .await
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv())
            .await
            .expect("the daemon must push another client's write")
            .expect("the observer callback channel must remain open");

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

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(
            service_reconnect_delay(0),
            std::time::Duration::from_millis(250)
        );
        assert_eq!(
            service_reconnect_delay(1),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(
            service_reconnect_delay(20),
            std::time::Duration::from_secs(5)
        );
    }

    #[test]
    fn server_slot_clear_prevents_stale_generation_access() {
        let slot = ServerSlot::default();
        assert!(slot.get().is_none());
        slot.clear();
        assert!(slot.get().is_none());
    }

    #[tokio::test]
    async fn service_probe_detects_shutdown_and_fresh_connect_recovers() {
        let (dir, shutdown, task, _owner, url) = service_fixture("shared-chaz").await;
        let config = settings(url.clone(), "shared-chaz");
        let connected = connect_with(&config, ExecutionRole::Client).await.unwrap();
        probe_service(&connected.instance).await.unwrap();

        drop(shutdown);
        task.await.unwrap().unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            probe_service(&connected.instance),
        )
        .await
        .expect("dead service probe must not hang")
        .unwrap_err();
        assert!(
            error.is_transient_service(),
            "unexpected probe error: {error}"
        );
        drop(connected);

        let socket = dir.path().join("service.sock");
        let (owner, _) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewUser::passwordless("shared-chaz"),
        )
        .await
        .unwrap();
        let service = ServiceServer::bind(owner, &socket).await.unwrap();
        let (shutdown, receiver) = watch::channel(());
        let task = tokio::spawn(service.run(receiver));
        let replacement = connect_with(&config, ExecutionRole::Client).await.unwrap();
        probe_service(&replacement.instance).await.unwrap();
        drop(replacement);
        drop(shutdown);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn login_failure_is_terminal_not_reconnectable() {
        let (_dir, shutdown, task, _owner, url) = service_fixture("real-user").await;
        let error = connect_with(&settings(url, "wrong-user"), ExecutionRole::Executor)
            .await
            .err()
            .unwrap();
        assert!(matches!(error, ConnectError::Login { .. }));
        assert!(!error.is_transient_service());
        drop(shutdown);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn replacement_waits_for_shutdown_before_reconnect() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let shutdown_events = events.clone();
        let reconnect_events = events.clone();
        let replacement = replace_service_generation(
            1,
            move |_| async move {
                shutdown_events.lock().unwrap().push("shutdown-complete");
            },
            move || async move {
                reconnect_events.lock().unwrap().push("reconnected");
                Ok(2)
            },
        )
        .await
        .unwrap();
        assert_eq!(replacement, 2);
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["shutdown-complete", "reconnected"]
        );
    }

    /// A generation whose transport work ends on its own, plus a record of
    /// the order supervision tore it down in.
    struct FinishingGeneration {
        instance: Instance,
        finish: Option<tokio::sync::oneshot::Receiver<anyhow::Result<()>>>,
        events: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl ServiceGeneration for FinishingGeneration {
        fn instance(&self) -> Instance {
            self.instance.clone()
        }

        async fn finished(&mut self) -> anyhow::Result<()> {
            match self.finish.take() {
                Some(receiver) => receiver.await.unwrap_or_else(|_| Ok(())),
                None => std::future::pending().await,
            }
        }

        async fn shutdown(self) {
            self.events.lock().unwrap().push("shutdown");
        }
    }

    #[tokio::test]
    async fn supervision_tears_down_a_generation_that_finishes_on_its_own() {
        let (_dir, shutdown, task, _owner, url) = service_fixture("shared").await;
        let connected = connect_with(&settings(url.clone(), "shared"), ExecutionRole::Client)
            .await
            .unwrap();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let generation = FinishingGeneration {
            instance: connected.instance,
            finish: Some(finish_rx),
            events: events.clone(),
        };
        let rebuilds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rebuild_count = rebuilds.clone();
        let supervisor = tokio::spawn(supervise_service(
            generation,
            move || {
                rebuild_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { anyhow::bail!("not expected") }
            },
            std::future::pending::<()>,
        ));
        finish_tx
            .send(Err(anyhow::anyhow!("transport exited")))
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), supervisor)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().to_string(), "transport exited");
        assert_eq!(events.lock().unwrap().as_slice(), ["shutdown"]);
        assert_eq!(rebuilds.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(shutdown);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn supervision_rebuilds_after_the_service_goes_away() {
        let (dir, shutdown, task, _owner, url) = service_fixture("shared").await;
        let connected = connect_with(&settings(url.clone(), "shared"), ExecutionRole::Client)
            .await
            .unwrap();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let generation = FinishingGeneration {
            instance: connected.instance,
            finish: None,
            events: events.clone(),
        };
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let rebuilt = std::sync::Arc::new(std::sync::Mutex::new(None));
        let rebuilt_slot = rebuilt.clone();
        let rebuild_events = events.clone();
        let rebuild_url = url.clone();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let finish_rx = std::sync::Arc::new(std::sync::Mutex::new(Some(finish_rx)));
        let supervisor = tokio::spawn(supervise_service(
            generation,
            move || {
                let events = rebuild_events.clone();
                let url = rebuild_url.clone();
                let rebuilt = rebuilt_slot.clone();
                let finish_rx = finish_rx.clone();
                async move {
                    events.lock().unwrap().push("rebuild-attempt");
                    let connected =
                        connect_with(&settings(url, "shared"), ExecutionRole::Client).await?;
                    *rebuilt.lock().unwrap() = Some(());
                    Ok(FinishingGeneration {
                        instance: connected.instance,
                        finish: finish_rx.lock().unwrap().take(),
                        events,
                    })
                }
            },
            move || {
                let mut stop_rx = stop_rx.clone();
                async move {
                    while !*stop_rx.borrow() {
                        if stop_rx.changed().await.is_err() {
                            return;
                        }
                    }
                }
            },
        ));

        // Kill the daemon: the first generation must be torn down before any
        // rebuild attempt, and rebuilds keep failing while the socket is gone.
        drop(shutdown);
        task.await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let seen = events.lock().unwrap().clone();
                if seen.len() >= 2 {
                    assert_eq!(seen[0], "shutdown");
                    assert!(seen[1..].iter().all(|event| *event == "rebuild-attempt"));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        // Bring the daemon back on the same socket and the replacement lands.
        let socket = dir.path().join("service.sock");
        let _ = std::fs::remove_file(&socket);
        let (instance, _) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("shared"))
                .await
                .unwrap();
        let server = ServiceServer::bind(instance, &socket).await.unwrap();
        let (shutdown, receiver) = watch::channel(());
        let task = tokio::spawn(server.run(receiver));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while rebuilt.lock().unwrap().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        // Ordinary shutdown ends supervision through the replacement's teardown.
        stop_tx.send(true).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), supervisor)
            .await
            .unwrap()
            .unwrap();
        result.unwrap();
        assert_eq!(events.lock().unwrap().last(), Some(&"shutdown"));
        drop(finish_tx);
        drop(shutdown);
        task.await.unwrap().unwrap();
    }
}
