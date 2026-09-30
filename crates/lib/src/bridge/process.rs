//! Shared startup for standalone bridge processes (`chaz-matrix`,
//! `chaz-discord`).
//!
//! A bridge connects through the same connector as every other Chaz process
//! ([`crate::instance::connect`]): its configuration names the Eidetica
//! connection, the existing login, and the execution role explicitly. The
//! binary name grants nothing. A direct owner brings its logins online by
//! ticket-bootstrapping the owning agent's database and publishing its sync
//! identity; a service client shares the daemon's login, already holds the
//! agent database, and only publishes its login pointer.

use std::path::Path;
use std::sync::Arc;

use eidetica::Instance;
use eidetica::sync::DatabaseTicket;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::agent_db::LoginRef;
use crate::bridge_identity::{BridgeIdentity, SyncBootstrap, establish_login};
use crate::config::Config;
use crate::instance::{self, ConnectedInstance, InstanceOwnership, ServiceGeneration};
use crate::server::{BuiltServer, Server};
use crate::session::BootstrapOutcome;

/// How long transport tasks get to observe the shutdown signal before they
/// are aborted.
const TRANSPORT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// What distinguishes one bridge executable from another in startup
/// diagnostics and login pointers.
#[derive(Debug, Clone, Copy)]
pub struct BridgeTransport {
    /// Transport kind used in routing and login pointers (`"matrix"`).
    pub kind: &'static str,
    /// Executable name, for entry-point-specific diagnostics.
    pub binary: &'static str,
    /// Eidetica username earlier releases created implicitly for this bridge.
    /// Migration examples preserve it so the existing store opens as-is.
    pub legacy_username: &'static str,
}

pub const MATRIX: BridgeTransport = BridgeTransport {
    kind: "matrix",
    binary: "chaz-matrix",
    legacy_username: "chaz-matrix",
};

pub const DISCORD: BridgeTransport = BridgeTransport {
    kind: "discord",
    binary: "chaz-discord",
    legacy_username: "chaz-discord",
};

/// The exact configuration a bridge needs when its connector settings are
/// missing: the direct form pointing at the store this bridge's earlier
/// releases created under `state_dir`, and the service form.
pub fn connector_example(transport: BridgeTransport, state_dir: &Path) -> String {
    let store = state_dir.join("eidetica.db");
    format!(
        "{binary} requires explicit Eidetica connection settings and an execution role.\n\
         \n\
         To keep this bridge as its own Eidetica peer on its existing store, add:\n\
         \n\
         execution: client\n\
         eidetica:\n\
         \x20 connection: \"sqlite://{store}\"\n\
         \x20 login:\n\
         \x20   username: {username}\n\
         \x20   passwordless: true\n\
         \x20 sync:\n\
         \x20   iroh: true\n\
         \n\
         To share the Eidetica daemon's login with the chaz executor instead:\n\
         \n\
         execution: client\n\
         eidetica:\n\
         \x20 connection: \"unix:///run/eidetica/service.sock\"\n\
         \x20 login:\n\
         \x20   username: chaz\n\
         \x20   passwordless: true\n\
         \n\
         Nothing is created or moved implicitly; a store that does not exist yet \
         is provisioned with the Eidetica CLI first.",
        binary = transport.binary,
        store = store.display(),
        username = transport.legacy_username,
    )
}

/// Connect a bridge process through the shared connector. Missing settings
/// fail before anything is opened, with [`connector_example`] for this
/// executable and state directory.
pub async fn connect(
    config: &Config,
    transport: BridgeTransport,
    state_dir: &Path,
) -> anyhow::Result<ConnectedInstance> {
    if config.eidetica.is_none() || config.execution.is_none() {
        anyhow::bail!("{}", connector_example(transport, state_dir));
    }
    Ok(instance::connect(config).await?)
}

/// A configured login before it is brought online.
#[derive(Debug, Clone)]
pub struct LoginSpec {
    pub login_id: String,
    pub agent: String,
    /// Access ticket for the owning agent's database. Required for a direct
    /// owner; owner-only, and therefore rejected, on a service connection.
    pub ticket: Option<String>,
}

/// Result of bringing one login online.
#[derive(Debug)]
pub enum LoginOutcome {
    /// Access is in hand and the pointer is published (direct) or deferred
    /// to [`publish_service_login`] (service).
    Ready,
    /// The owner still has to approve this bridge's key; the login is skipped
    /// until a restart after `/sharing approve`.
    Pending(String),
}

/// The sync identity a direct owner publishes with its logins so the daemon
/// can dial it: device pubkey and every address it is currently serving on.
pub struct SyncIdentity {
    pub peer_pubkey: Option<String>,
    pub sync_addresses: Vec<(String, String)>,
}

impl SyncIdentity {
    /// A service client has no sync identity of its own; the daemon owns
    /// transports, so nothing is published.
    pub fn none() -> Self {
        Self {
            peer_pubkey: None,
            sync_addresses: Vec::new(),
        }
    }

    pub async fn of(connected: &ConnectedInstance) -> Self {
        let Some(sync) = connected.instance.sync() else {
            return Self::none();
        };
        Self {
            peer_pubkey: sync.get_device_pubkey().ok().map(|k| k.to_string()),
            sync_addresses: sync.get_all_server_addresses().await.unwrap_or_default(),
        }
    }
}

/// Bring one login online against its owning agent.
///
/// A direct owner must carry a ticket: it requests `Write` on the agent
/// database through the owner-side bootstrap flow and publishes its sync
/// identity with the pointer. A service client shares the daemon's login and
/// already holds every agent database, so a ticket is an owner-only control
/// and is refused with the exact owner-side redirect; its pointer is
/// published after the server is built, when the agent index can resolve the
/// agent by name.
pub async fn bring_up_login(
    connected: &mut ConnectedInstance,
    transport: BridgeTransport,
    bridge_db_id: &str,
    identity: &BridgeIdentity<'_>,
    sync_identity: &SyncIdentity,
    spec: &LoginSpec,
) -> anyhow::Result<LoginOutcome> {
    match connected.capabilities.ownership() {
        InstanceOwnership::Service => {
            if spec.ticket.is_some() {
                anyhow::bail!(
                    "login {login} sets `ticket`, which is an owner-only control: a service \
                     client shares the Eidetica daemon's login and already holds the agent \
                     database. Omit `ticket` here; share the agent with other peers from the \
                     owner with `/agent share {agent}`.",
                    login = spec.login_id,
                    agent = spec.agent,
                );
            }
            Ok(LoginOutcome::Ready)
        }
        InstanceOwnership::Direct => {
            let Some(raw_ticket) = spec.ticket.as_deref() else {
                anyhow::bail!(
                    "login {login} has no `ticket`: a direct Eidetica owner reaches agent \
                     `{agent}` only through the ticket minted by `/agent share {agent}` on the \
                     executor",
                    login = spec.login_id,
                    agent = spec.agent,
                );
            };
            let sync = connected.instance.sync().ok_or_else(|| {
                anyhow::anyhow!(
                    "login {login} needs `eidetica.sync` on a direct owner: ticket bootstrap \
                     and the daemon's replies both travel over Eidetica sync",
                    login = spec.login_id,
                )
            })?;
            let ticket: DatabaseTicket = raw_ticket
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid ticket for login {}: {e}", spec.login_id))?;
            let login_ref = LoginRef {
                kind: transport.kind.to_string(),
                identifier: spec.login_id.clone(),
                bridge_db_id: bridge_db_id.to_string(),
                peer_pubkey: sync_identity.peer_pubkey.clone(),
                agent_pubkey: None,
                sync_addresses: sync_identity.sync_addresses.clone(),
            };
            let bootstrap = SyncBootstrap::new(sync.clone());
            match establish_login(
                &mut connected.user,
                &bootstrap,
                &sync,
                identity,
                &ticket,
                login_ref,
            )
            .await?
            {
                BootstrapOutcome::Approved => Ok(LoginOutcome::Ready),
                BootstrapOutcome::Pending { message, .. } => {
                    warn!(
                        login = %spec.login_id,
                        "Access pending owner approval ({message}); skipping. \
                         Approve with /sharing approve on the daemon, then restart."
                    );
                    Ok(LoginOutcome::Pending(message))
                }
            }
        }
    }
}

/// Publish a service client's login pointer on the owning agent's database.
///
/// Carries no sync identity: the Eidetica daemon owns transports, and the
/// executor reads this database through the same login. Best-effort — a
/// missing agent is logged, because the bridge resolves it again per message.
pub async fn publish_service_login(
    server: &Server,
    transport: BridgeTransport,
    bridge_db_id: &str,
    identity: &BridgeIdentity<'_>,
    spec: &LoginSpec,
) -> anyhow::Result<()> {
    let Some(entry) = server.agent_index().find_by_name(&spec.agent) else {
        warn!(
            login = %spec.login_id,
            agent = %spec.agent,
            "Owning agent is not present in the shared login yet; login pointer not published"
        );
        return Ok(());
    };
    let Some(agent_db) = server.registry().open_agent_db(&entry.db_id, None).await? else {
        return Ok(());
    };
    let login = LoginRef {
        kind: transport.kind.to_string(),
        identifier: spec.login_id.clone(),
        bridge_db_id: bridge_db_id.to_string(),
        peer_pubkey: None,
        agent_pubkey: Some(identity.key.to_string()),
        sync_addresses: Vec::new(),
    };
    if agent_db.find_login(&login.identifier).await?.as_ref() == Some(&login) {
        return Ok(());
    }
    agent_db.register_login(login).await?;
    info!(
        login = %spec.login_id,
        agent = %spec.agent,
        "Published this service client's login pointer in the agent DB"
    );
    Ok(())
}

/// One bridge generation: the application runtime built on one connection
/// plus the transport tasks (one per login) running against it. Torn down
/// as a unit — transports first, then the runtime — before a service
/// reconnect constructs the next generation.
pub struct BridgeGeneration {
    built: BuiltServer,
    tasks: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
    shutdown: Arc<Notify>,
}

impl BridgeGeneration {
    /// `shutdown` is the cooperative signal every transport task of this
    /// generation selects on.
    pub fn new(built: BuiltServer, shutdown: Arc<Notify>) -> Self {
        Self {
            built,
            tasks: Vec::new(),
            shutdown,
        }
    }

    pub fn server(&self) -> Arc<Server> {
        self.built.server.clone()
    }

    pub fn secret_store(&self) -> crate::security::SecretStore {
        self.built.secret_store.clone()
    }

    pub fn shutdown_signal(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    /// Run one transport task as part of this generation.
    pub fn spawn(
        &mut self,
        task: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    ) {
        self.tasks.push(tokio::spawn(task));
    }
}

impl ServiceGeneration for BridgeGeneration {
    fn instance(&self) -> Instance {
        self.built.registry.instance().clone()
    }

    /// Resolves with the first transport task to end. A clean exit ends the
    /// process cleanly; an error is surfaced as the process result.
    async fn finished(&mut self) -> anyhow::Result<()> {
        if self.tasks.is_empty() {
            return std::future::pending().await;
        }
        let (result, index, _) = futures::future::select_all(self.tasks.iter_mut()).await;
        self.tasks.remove(index);
        match result {
            Ok(result) => result,
            Err(join) => Err(anyhow::anyhow!("bridge task panicked: {join}")),
        }
    }

    async fn shutdown(self) {
        self.shutdown.notify_waiters();
        for mut task in self.tasks {
            if tokio::time::timeout(TRANSPORT_SHUTDOWN_GRACE, &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        self.built.shutdown().await;
    }
}

/// Drive a direct-owner generation until the process is asked to stop or a
/// transport task ends. Service clients use
/// [`crate::instance::supervise_service`] instead, which adds reconnect.
pub async fn run_direct(mut generation: BridgeGeneration) -> anyhow::Result<()> {
    let result = tokio::select! {
        _ = instance::wait_for_shutdown() => {
            info!("Shutdown signal received; stopping");
            Ok(())
        }
        result = generation.finished() => result,
    };
    generation.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_names_the_legacy_store_and_login_and_the_service_form() {
        let text = connector_example(MATRIX, Path::new("/var/lib/chaz-matrix"));
        assert!(text.contains("chaz-matrix requires explicit Eidetica connection settings"));
        assert!(text.contains("sqlite:///var/lib/chaz-matrix/eidetica.db"));
        assert!(text.contains("username: chaz-matrix"));
        assert!(text.contains("execution: client"));
        assert!(text.contains("unix:///run/eidetica/service.sock"));
        assert!(!text.contains("execution: executor"));

        let text = connector_example(DISCORD, Path::new("/tmp/d"));
        assert!(text.contains("chaz-discord requires"));
        assert!(text.contains("username: chaz-discord"));
    }

    #[tokio::test]
    async fn missing_settings_fail_before_opening_anything() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let error = match connect(&config, DISCORD, dir.path()).await {
            Ok(_) => panic!("missing settings must not connect"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("chaz-discord requires"));
        assert!(!dir.path().join("eidetica.db").exists());

        let yaml = r#"
execution: client
eidetica:
  connection: "unix:///nonexistent/service.sock"
  login: { username: chaz, passwordless: true }
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let error = match connect(&config, MATRIX, dir.path()).await {
            Ok(_) => panic!("a missing socket must not connect"),
            Err(error) => error,
        };
        assert!(
            error.downcast_ref::<instance::ConnectError>().is_some(),
            "complete settings reach the connector: {error}"
        );
    }

    /// A connected service client and a connected direct owner, with a
    /// bridge key on each, so login bring-up can be exercised end to end.
    async fn connected_fixture() -> (
        tempfile::TempDir,
        tokio::sync::watch::Sender<()>,
        tokio::task::JoinHandle<eidetica::Result<()>>,
        ConnectedInstance,
        ConnectedInstance,
    ) {
        use eidetica::backend::database::InMemory;
        use eidetica::service::ServiceServer;
        use eidetica::{Instance, NewUser};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("service.sock");
        let (owner, _) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("shared"))
                .await
                .unwrap();
        let server = ServiceServer::bind(owner, &socket).await.unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(());
        let task = tokio::spawn(server.run(receiver));
        let settings = |connection: String| crate::config::EideticaConfig {
            connection,
            login: crate::config::EideticaLoginConfig {
                username: "shared".into(),
                password: None,
                passwordless: true,
            },
            sync: None,
        };
        let service = instance::connect_with(
            &settings(format!("unix://{}", socket.display())),
            crate::config::ExecutionRole::Client,
        )
        .await
        .unwrap();
        let snapshot = dir.path().join("direct.json");
        let (direct_owner, _) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("shared"))
                .await
                .unwrap();
        direct_owner.snapshot_to_path(&snapshot).unwrap();
        let direct = instance::connect_with(
            &settings(format!("memory://{}", snapshot.display())),
            crate::config::ExecutionRole::Client,
        )
        .await
        .unwrap();
        (dir, shutdown, task, service, direct)
    }

    #[tokio::test]
    async fn a_service_client_refuses_owner_only_tickets_and_a_direct_owner_requires_one() {
        let (_dir, shutdown, task, mut service, mut direct) = connected_fixture().await;
        let key = crate::bridge_identity::ensure_bridge_key(
            &mut service.user,
            crate::bridge_identity::BRIDGE_KEY_NAME,
        )
        .await
        .unwrap();
        let identity = BridgeIdentity {
            key: &key,
            key_name: crate::bridge_identity::BRIDGE_KEY_NAME,
        };
        let with_ticket = LoginSpec {
            login_id: "@chaz:example".into(),
            agent: "chaz".into(),
            ticket: Some("eidetica:?db=sha256:agentdbid&pr=iroh:peeraddr".into()),
        };
        let without_ticket = LoginSpec {
            ticket: None,
            ..with_ticket.clone()
        };

        let error = bring_up_login(
            &mut service,
            MATRIX,
            "bridge-db",
            &identity,
            &SyncIdentity::none(),
            &with_ticket,
        )
        .await
        .expect_err("owner-only control refused");
        assert!(error.to_string().contains("owner-only"));
        assert!(error.to_string().contains("/agent share chaz"));
        assert!(matches!(
            bring_up_login(
                &mut service,
                MATRIX,
                "bridge-db",
                &identity,
                &SyncIdentity::none(),
                &without_ticket,
            )
            .await
            .unwrap(),
            LoginOutcome::Ready
        ));

        let error = bring_up_login(
            &mut direct,
            DISCORD,
            "bridge-db",
            &identity,
            &SyncIdentity::none(),
            &without_ticket,
        )
        .await
        .expect_err("a direct owner needs a ticket");
        assert!(error.to_string().contains("has no `ticket`"));
        let error = bring_up_login(
            &mut direct,
            DISCORD,
            "bridge-db",
            &identity,
            &SyncIdentity::none(),
            &with_ticket,
        )
        .await
        .expect_err("a direct owner without sync cannot bootstrap");
        assert!(
            error.to_string().contains("needs `eidetica.sync`"),
            "unexpected error: {error}"
        );

        drop(service);
        drop(direct);
        drop(shutdown);
        task.await.unwrap().unwrap();
    }
}
