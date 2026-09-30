//! `chaz-discord` — the standalone Discord bridge.
//!
//! Its own process and `fn main()`, linking `chaz-core`. Like `chaz-matrix`
//! it connects to Eidetica through the shared connector: its configuration
//! names the connection, the existing login, and the execution role
//! explicitly. As a direct owner it is its own Eidetica peer that reaches each
//! agent's DB through an access ticket (`/agent share` on the executor →
//! ticket bootstrap here); as a service client it shares the Eidetica daemon's
//! login with the executor and holds the agent DBs already. Either way it is
//! transport I/O: the configured executor runs the agents, and replies are
//! delivered to each channel with durable per-channel progress.
//!
//! Bring-up order is load-bearing for a direct owner: sync is configured by
//! the connector before access can be bootstrapped, and the agent DBs must be
//! ticket-bootstrapped before the `Server` is assembled (so the hosted index
//! discovers them rather than minting local copies).

mod bridge;
mod config;
mod credentials;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chaz_core::bridge::Bridge;
use chaz_core::bridge::process::{self, BridgeGeneration, LoginOutcome, LoginSpec, SyncIdentity};
use chaz_core::bridge_db::{create_bridge_db, find_bridge_db};
use chaz_core::bridge_identity::{BRIDGE_KEY_NAME, BridgeIdentity, ensure_bridge_key};
use chaz_core::config::Config;
use chaz_core::instance::{self, ConnectedInstance, InstanceOwnership};
use chaz_core::server;

use clap::Parser;
use eidetica::auth::crypto::PublicKey;
use tokio::sync::Notify;
use tracing::{info, warn};

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
#[derive(Clone)]
struct ReadyLogin {
    spec: LoginSpec,
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

    // Warn about unrecognised YAML keys — the known set is the union of
    // Config + bridge fields, so the double-parse doesn't produce
    // cross-noise.
    let unknown = chaz_core::config::check_unknown_config_keys(&contents);
    if !unknown.is_empty() {
        warn!(
            config = %config_path.display(),
            count = unknown.len(),
            keys = %unknown.join(", "),
            "Unrecognised config key(s)"
        );
    }

    info!(config = %config_path.display(), label = %bridge_cfg.label, "Starting chaz-discord");

    // The bridge's own state dir: a direct owner's legacy store is expected
    // at `<state_dir>/eidetica.db`.
    let base = bridge_cfg
        .state_dir
        .clone()
        .or_else(|| config.state_dir.clone())
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz-discord")))
        .ok_or_else(|| anyhow::anyhow!("could not determine a state directory"))?;

    // Connect through the shared connector. Nothing is created implicitly:
    // missing settings stop here with the exact example for this binary.
    let mut connected = process::connect(&config, process::DISCORD, &base).await?;
    std::fs::create_dir_all(&base)?;

    // Bridge identity + its own settings DB, then seed credentials (idempotent).
    let bridge_key = ensure_bridge_key(&mut connected.user, BRIDGE_KEY_NAME).await?;
    let (bridge_db, _) = match find_bridge_db(&connected.user, &bridge_cfg.label).await {
        Some(found) => found,
        None => create_bridge_db(&mut connected.user, &bridge_cfg.label).await?,
    };
    bridge_cfg.seed_into(&bridge_db).await?;
    let unlock = bridge_cfg.resolve_unlock_password()?;
    let bridge_db_id = bridge_db.id().to_string();

    // Bring each configured login online. A direct owner ticket-bootstraps
    // Write on its agent DB and publishes where its sync identity is
    // reachable; a service client already holds the agent DB. Logins still
    // pending owner approval are skipped until a re-run.
    let sync_identity = SyncIdentity::of(&connected).await;
    info!(
        pubkey = ?sync_identity.peer_pubkey,
        addresses = ?sync_identity.sync_addresses,
        ownership = ?connected.capabilities.ownership(),
        "Bringing Discord logins online"
    );
    let identity = BridgeIdentity {
        key: &bridge_key,
        key_name: BRIDGE_KEY_NAME,
    };
    let mut ready: Vec<ReadyLogin> = Vec::new();
    for entry in &bridge_cfg.logins {
        let spec = LoginSpec {
            login_id: entry.login_id.clone(),
            agent: entry.agent.clone(),
            ticket: entry.ticket.clone(),
        };
        match process::bring_up_login(
            &mut connected,
            process::DISCORD,
            &bridge_db_id,
            &identity,
            &sync_identity,
            &spec,
        )
        .await?
        {
            LoginOutcome::Ready => {
                let creds: DiscordCredentials = bridge_db
                    .read_credentials(&spec.login_id, &unlock)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("no seeded credentials found for login {}", spec.login_id)
                    })?;
                ready.push(ReadyLogin { spec, creds });
            }
            LoginOutcome::Pending(_) => {}
        }
    }

    if ready.is_empty() {
        anyhow::bail!(
            "no Discord logins are ready — none configured, or all are pending owner approval"
        );
    }

    let ownership = connected.capabilities.ownership();
    let generation = build_generation(
        &mut config,
        &config_path,
        connected,
        &bridge_key,
        &bridge_db_id,
        &ready,
    )
    .await?;

    match ownership {
        InstanceOwnership::Direct => process::run_direct(generation).await,
        InstanceOwnership::Service => {
            let context = Arc::new(RebuildContext {
                config,
                config_path,
                bridge_key,
                bridge_db_id,
                ready,
            });
            instance::supervise_service(
                generation,
                move || {
                    let context = context.clone();
                    async move {
                        let connected = instance::connect(&context.config).await?;
                        let mut config = context.config.clone();
                        build_generation(
                            &mut config,
                            &context.config_path,
                            connected,
                            &context.bridge_key,
                            &context.bridge_db_id,
                            &context.ready,
                        )
                        .await
                    }
                },
                instance::wait_for_shutdown,
            )
            .await
        }
    }
}

/// Everything a service reconnect needs to rebuild the generation. The
/// credentials were read before the first generation; a rebuild reuses them
/// rather than reopening the bridge DB on the new connection.
struct RebuildContext {
    config: Config,
    config_path: PathBuf,
    bridge_key: PublicKey,
    bridge_db_id: String,
    ready: Vec<ReadyLogin>,
}

/// Assemble the runtime on a connection and start one [`DiscordBridge`] per
/// ready login against it. A service reconnect calls this again on a fresh
/// connection after the previous generation is fully torn down.
async fn build_generation(
    config: &mut Config,
    config_path: &Path,
    connected: ConnectedInstance,
    bridge_key: &PublicKey,
    bridge_db_id: &str,
    ready: &[ReadyLogin],
) -> anyhow::Result<BridgeGeneration> {
    let capabilities = connected.capabilities;
    // The explicit execution role decides whether this process runs agents;
    // the bridge binary neither assumes nor forces client mode.
    let built = server::build(
        config,
        connected.instance,
        connected.user,
        server::BuildOptions::local(
            config_path.to_path_buf(),
            capabilities,
            Vec::new(),
            // Long-lived: MCP tools land whenever their servers finish.
            server::McpReadiness::Deferred,
        ),
    )
    .await?;
    let mut generation = BridgeGeneration::new(built, Arc::new(Notify::new()));
    let server = generation.server();
    let secret_store = generation.secret_store();
    let shutdown = generation.shutdown_signal();

    if capabilities.ownership() == InstanceOwnership::Service {
        let identity = BridgeIdentity {
            key: bridge_key,
            key_name: BRIDGE_KEY_NAME,
        };
        for login in ready {
            process::publish_service_login(
                &server,
                process::DISCORD,
                bridge_db_id,
                &identity,
                &login.spec,
            )
            .await?;
        }
    }

    for login in ready {
        let bridge = DiscordBridge::new(
            login.spec.login_id.clone(),
            login.spec.agent.clone(),
            login.creds.clone(),
            config.clone(),
            secret_store.clone(),
            shutdown.clone(),
        );
        let server = server.clone();
        info!(login = %login.spec.login_id, agent = %login.spec.agent, "Discord login spawned");
        generation.spawn(async move { bridge.run(server).await });
    }
    Ok(generation)
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
