//! `chaz-discord` — the standalone Discord bridge, run as its own eidetica peer.
//!
//! Its own process and `fn main()`, linking `chaz-core`. Like `chaz-matrix`, it
//! does **not** share the chaz daemon's database: it opens its **own** backend
//! file, holds its **own** key, and reaches each agent's DB through an access
//! ticket (`/agent share` on the daemon → `bootstrap_with_ticket` here). It owns
//! no agents and runs no routine engine — pure transport I/O; the daemon (a
//! separate peer syncing the session DBs) runs the agents.
//!
//! Bring-up order is load-bearing: sync must be enabled before access can be
//! bootstrapped, and the agent DBs must be ticket-bootstrapped before the
//! `Server` is assembled (so the hosted index discovers them rather than
//! minting local copies — see `BuildOptions::bootstrap_agents_from_config`).

mod bridge;
mod config;
mod credentials;

use std::path::PathBuf;

use chaz_core::agent_db::LoginRef;
use chaz_core::bridge::Bridge;
use chaz_core::bridge_db::{create_bridge_db, find_bridge_db};
use chaz_core::bridge_identity::{
    BRIDGE_KEY_NAME, BridgeIdentity, SyncBootstrap, ensure_bridge_key, establish_login,
};
use chaz_core::config::Config;
use chaz_core::server;
use chaz_core::session::BootstrapOutcome;

use clap::Parser;
use eidetica::sync::DatabaseTicket;
use tracing::{error, info, warn};

use crate::bridge::DiscordBridge;
use crate::config::DiscordBridgeConfig;
use crate::credentials::DiscordCredentials;

#[derive(Parser)]
#[command(author, version, about = "Standalone Discord bridge for chaz", long_about = None)]
struct Args {
    /// Path to the bridge config file. When unset, falls back to
    /// `$XDG_CONFIG_HOME/chaz/discord-bridge.yaml`. The file carries both the
    /// bridge's own settings (`label`, `unlock_password`, `logins`) and the
    /// chaz config the runtime needs (`backends`, `agents`, `security`).
    #[arg(short, long)]
    config: Option<PathBuf>,
}

/// A login that bootstrapped access and has its credentials in hand, ready to
/// spawn a [`DiscordBridge`].
struct ReadyLogin {
    login_id: String,
    owning_agent: String,
    creds: DiscordCredentials,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let config_path = resolve_config_path(args.config.as_deref())?;
    let contents = std::fs::read_to_string(&config_path)?;

    // Parse the same bytes twice: once as the full chaz config (backends,
    // agents, security) the runtime needs, and once for the bridge's own
    // section (label, unlock_password, logins).
    let mut config: Config = serde_yaml::from_str(&contents)?;
    let bridge_cfg: DiscordBridgeConfig = serde_yaml::from_str(&contents)?;

    info!(config = %config_path.display(), label = %bridge_cfg.label, "Starting chaz-discord");

    // The bridge's OWN state dir — distinct from the chaz daemon's.
    let base = bridge_cfg
        .state_dir
        .clone()
        .or_else(|| config.state_dir.clone())
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz-discord")))
        .ok_or_else(|| anyhow::anyhow!("could not determine a state directory"))?;
    std::fs::create_dir_all(&base)?;

    // Open this bridge's own eidetica peer (its own backend file, NOT the
    // daemon's eidetica.db).
    let eidetica_db_path = base.join("eidetica.db");
    let backend = eidetica::backend::database::SqlxBackend::open_sqlite(&eidetica_db_path).await?;
    let (instance, maybe_user) = eidetica::Instance::connect_or_create_backend(
        Box::new(backend),
        eidetica::NewUser::passwordless("chaz-discord"),
    )
    .await?;
    let mut user = match maybe_user {
        Some(u) => u,
        None => instance.login_user("chaz-discord", None).await?,
    };

    // Enable sync up front — access bootstrap needs the live Sync handle.
    instance.enable_sync().await?;
    let sync = instance
        .sync()
        .ok_or_else(|| anyhow::anyhow!("sync failed to enable"))?;
    {
        use eidetica::sync::transports::iroh::IrohTransport;
        sync.register_transport("iroh", IrohTransport::builder())
            .await?;
        if let Some(addr) = &config.sync_listen {
            use eidetica::sync::transports::http::HttpTransport;
            sync.register_transport("http", HttpTransport::builder().bind(addr))
                .await?;
            info!("Sync HTTP transport listening on {addr}");
        }
        sync.accept_connections().await?;
        if let Ok(addr) = sync.get_server_address().await {
            info!("chaz-discord sync address: {addr}");
        }
    }

    // Bridge identity + its own settings DB, then seed credentials (idempotent).
    let bridge_key = ensure_bridge_key(&mut user, BRIDGE_KEY_NAME).await?;
    let (bridge_db, _) = match find_bridge_db(&user, &bridge_cfg.label).await {
        Some(found) => found,
        None => create_bridge_db(&mut user, &bridge_cfg.label).await?,
    };
    bridge_cfg.seed_into(&bridge_db).await?;
    let unlock = bridge_cfg.resolve_unlock_password()?;
    let bridge_db_id = bridge_db.id().to_string();

    // Bring each configured login online: ticket-bootstrap Write on its agent
    // DB, register the public pointer, and stash its resolved credentials.
    let bootstrap = SyncBootstrap::new(sync.clone());
    let identity = BridgeIdentity {
        key: &bridge_key,
        key_name: BRIDGE_KEY_NAME,
    };
    let mut ready: Vec<ReadyLogin> = Vec::new();
    for entry in &bridge_cfg.logins {
        let login_id = entry.login_id.clone();
        let ticket: DatabaseTicket = entry
            .ticket
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid ticket for login {login_id}: {e}"))?;
        let login_ref = LoginRef {
            kind: "discord".to_string(),
            identifier: login_id.clone(),
            bridge_db_id: bridge_db_id.clone(),
        };
        match establish_login(&mut user, &bootstrap, &identity, &ticket, login_ref).await? {
            BootstrapOutcome::Approved => {
                let creds: DiscordCredentials = bridge_db
                    .read_credentials(&login_id, &unlock)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("no seeded credentials found for login {login_id}")
                    })?;
                ready.push(ReadyLogin {
                    login_id,
                    owning_agent: entry.agent.clone(),
                    creds,
                });
            }
            BootstrapOutcome::Pending { message, .. } => {
                warn!(
                    login = %login_id,
                    "Access pending owner approval ({message}); skipping. \
                     Approve with /sharing approve on the daemon, then restart."
                );
            }
        }
    }

    if ready.is_empty() {
        anyhow::bail!(
            "no Discord logins are ready — none configured, or all are pending owner approval"
        );
    }

    // Assemble the Server WITHOUT minting agent DBs (already ticket-bootstrapped
    // above) and WITHOUT the routine engine (pure I/O — the daemon runs agents).
    let server::BuiltServer {
        server,
        secret_store,
        ..
    } = server::build(
        &mut config,
        instance,
        user,
        server::BuildOptions {
            config_path: config_path.clone(),
            enable_sync: true, // idempotent — already enabled above
            run_routine_engine: false,
            bootstrap_agents_from_config: false,
            // Dumb bridge: proxy + deliver only. The daemon runs agents.
            run_agent_loop: false,
            extra_auto_approved_tools: Vec::new(),
        },
    )
    .await?;

    // One DiscordBridge per ready login.
    let mut handles = Vec::new();
    for r in ready {
        let bridge = DiscordBridge::new(
            r.login_id.clone(),
            r.owning_agent.clone(),
            r.creds,
            config.clone(),
            secret_store.clone(),
        );
        let server = server.clone();
        info!(login = %r.login_id, agent = %r.owning_agent, "Discord login spawned");
        handles.push(tokio::spawn(async move { bridge.run(server).await }));
    }

    // Await every login bridge; surface the first error.
    let mut result = Ok(());
    for handle in handles {
        let res = match handle.await {
            Ok(res) => res,
            Err(join) => Err(anyhow::anyhow!("discord bridge task panicked: {join}")),
        };
        if let Err(e) = res {
            error!("Discord bridge error: {e}");
            if result.is_ok() {
                result = Err(e);
            }
        }
    }
    result
}

/// Resolve the bridge config path: explicit `--config`, else
/// `$XDG_CONFIG_HOME/chaz/discord-bridge.yaml`.
fn resolve_config_path(explicit: Option<&std::path::Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?;
    Ok(dir.join("chaz").join("discord-bridge.yaml"))
}
