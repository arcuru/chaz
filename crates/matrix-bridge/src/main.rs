//! `chaz-matrix` — the standalone Matrix bridge.
//!
//! This is its own process and `fn main()`, linking `chaz-core` + the
//! `chaz-matrix-bridge` library. It connects to Eidetica through the shared
//! connector like every other Chaz process: its configuration names the
//! connection, the existing login, and the execution role explicitly. As a
//! direct owner it is its own Eidetica peer — its own backend, its own key —
//! and reaches each agent's DB through an access ticket (`/agent share` on the
//! executor → ticket bootstrap here). As a service client it shares the
//! Eidetica daemon's login with the executor and holds the agent DBs already.
//! Either way it is transport I/O: inbound Matrix messages are proxied into
//! the session DBs, and the configured executor runs the agents whose replies
//! are delivered to the room with durable per-room progress.
//!
//! Bring-up order is load-bearing for a direct owner: sync is configured by
//! the connector before access can be bootstrapped, and the agent DBs must be
//! ticket-bootstrapped before the `Server` is assembled (so the hosted index
//! discovers them rather than minting local copies).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chaz_core::bridge::Bridge;
use chaz_core::bridge::process::{self, BridgeGeneration, LoginOutcome, LoginSpec, SyncIdentity};
use chaz_core::bridge_db::{create_bridge_db, find_bridge_db};
use chaz_core::bridge_identity::{BRIDGE_KEY_NAME, BridgeIdentity, ensure_bridge_key};
use chaz_core::config::Config;
use chaz_core::instance::{self, ConnectedInstance, InstanceOwnership};
use chaz_core::server;

use chaz_matrix_bridge::{MatrixBridge, MatrixBridgeConfig, MatrixCredentials};

use clap::Parser;
use eidetica::auth::crypto::PublicKey;
use tokio::sync::Notify;
use tracing::{info, warn};

#[derive(Parser)]
#[command(author, version, about = "Standalone Matrix bridge for chaz", long_about = None)]
struct Args {
    /// Path to the bridge config file. When unset, falls back to
    /// `$XDG_CONFIG_HOME/chaz/matrix-bridge.yaml`. The file carries both the
    /// bridge's own settings (`label`, `unlock_password`, `logins`) and the
    /// chaz config the runtime needs (`backends`, `agents`, `security`).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Print this bridge's public key and exit, generating it if this is the
    /// first run.
    ///
    /// This is the identity the owning peer authorizes — feed it to `chaz cmd
    /// '/agent invite <agent> <pubkey> write'` to pre-grant access, and the
    /// bridge bootstraps on its next start with no approval round-trip. Run it
    /// with the bridge stopped; it opens the bridge's own backend.
    #[arg(long)]
    print_pubkey: bool,

    /// Maintenance subcommands that run once and exit instead of starting the
    /// bridge.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Inspect or leave rooms for a bridge login.
    Rooms(RoomsArgs),
}

#[derive(clap::Args)]
struct RoomsArgs {
    #[command(subcommand)]
    command: RoomsCommand,
}

#[derive(clap::Subcommand)]
enum RoomsCommand {
    /// List every joined Matrix room without changing membership.
    List(RoomSelection),

    /// Preview leaving every joined room, or perform it with --execute.
    LeaveAll(LeaveAllArgs),
}

#[derive(clap::Args)]
struct RoomSelection {
    /// Operate on this login (MXID) when the bridge config has several.
    #[arg(long)]
    login: Option<String>,
}

#[derive(clap::Args)]
struct LeaveAllArgs {
    #[command(flatten)]
    selection: RoomSelection,

    /// Actually leave the rooms (the default is a dry-run preview).
    #[arg(long)]
    execute: bool,

    /// Leave attempts per room before giving up.
    #[arg(long, default_value_t = 3, value_parser = parse_retries)]
    retries: u32,

    /// Delay between rooms, in milliseconds.
    #[arg(long, default_value_t = 250, value_parser = parse_delay_ms)]
    delay_ms: u64,
}

/// Clap value parser for `rooms leave-all --retries`: leave attempts per room, bounded to
/// 1..=10. Zero would be a no-op loop, and anything above the cap would stall a
/// reset indefinitely.
fn parse_retries(s: &str) -> Result<u32, String> {
    let retries: u32 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid retry count"))?;
    if !(1..=10).contains(&retries) {
        return Err(format!("retries must be between 1 and 10, got {retries}"));
    }
    Ok(retries)
}

/// Clap value parser for `rooms leave-all --delay-ms`: delay between rooms, bounded to
/// 100..=60000 — a 100ms floor against hammering the homeserver, and a 60s
/// ceiling so a reset over many rooms cannot drag.
fn parse_delay_ms(s: &str) -> Result<u64, String> {
    let delay_ms: u64 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid delay in milliseconds"))?;
    if !(100..=60_000).contains(&delay_ms) {
        return Err(format!(
            "delay_ms must be between 100 and 60000, got {delay_ms}"
        ));
    }
    Ok(delay_ms)
}

/// A login that bootstrapped access and has its credentials in hand, ready to
/// spawn a [`MatrixBridge`].
#[derive(Clone)]
struct ReadyLogin {
    spec: LoginSpec,
    creds: MatrixCredentials,
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

    // The `rooms` maintenance subcommand runs once and exits; it never starts
    // the bridge or opens the bridge's own backend.
    if let Some(command) = &args.command {
        let Command::Rooms(rooms) = command;
        let (login, execute, retries, delay_ms) = match &rooms.command {
            RoomsCommand::List(selection) => (selection.login.as_deref(), false, 3, 250),
            RoomsCommand::LeaveAll(args) => (
                args.selection.login.as_deref(),
                args.execute,
                args.retries,
                args.delay_ms,
            ),
        };
        let contents = std::fs::read_to_string(&config_path)?;
        let bridge_cfg: MatrixBridgeConfig = match serde_yaml::from_str(&contents) {
            Ok(cfg) => cfg,
            Err(e) => {
                println!("cannot parse bridge config {}: {e}", config_path.display());
                std::process::exit(2);
            }
        };
        let code = chaz_matrix_bridge::rooms::run_rooms(
            &config_path,
            &bridge_cfg,
            login,
            execute,
            retries,
            delay_ms,
        )
        .await;
        std::process::exit(i32::from(code));
    }

    let contents = std::fs::read_to_string(&config_path)?;

    // Parse the same bytes twice: once as the full chaz config (backends,
    // agents, security) the runtime needs, and once for the bridge's own
    // section (label, unlock_password, logins).
    let mut config: Config = serde_yaml::from_str(&contents)?;
    let bridge_cfg: MatrixBridgeConfig = serde_yaml::from_str(&contents)?;

    // Warn about unrecognised YAML keys — the known set is the union of
    // Config + MatrixBridgeConfig fields, so the double-parse doesn't
    // produce cross-noise.
    let unknown = chaz_core::config::check_unknown_config_keys(&contents);
    if !unknown.is_empty() {
        warn!(
            config = %config_path.display(),
            count = unknown.len(),
            keys = %unknown.join(", "),
            "Unrecognised config key(s)"
        );
    }

    info!(config = %config_path.display(), label = %bridge_cfg.label, "Starting chaz-matrix");

    // The bridge's own state dir: the Matrix client's sync token and session
    // live here, and a direct owner's legacy store is expected at
    // `<state_dir>/eidetica.db`.
    let base = bridge_cfg
        .state_dir
        .clone()
        .or_else(|| config.state_dir.clone())
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz-matrix")))
        .ok_or_else(|| anyhow::anyhow!("could not determine a state directory"))?;

    // Connect through the shared connector. Nothing is created implicitly:
    // missing settings stop here with the exact example for this binary.
    let mut connected = process::connect(&config, process::MATRIX, &base).await?;
    std::fs::create_dir_all(&base)?;

    // `--print-pubkey` resolves the bridge's identity and stops here.
    if args.print_pubkey {
        let key = ensure_bridge_key(&mut connected.user, BRIDGE_KEY_NAME).await?;
        println!("{key}");
        return Ok(());
    }

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
        "Bringing Matrix logins online"
    );
    let identity = BridgeIdentity {
        key: &bridge_key,
        key_name: BRIDGE_KEY_NAME,
    };
    let mut ready: Vec<ReadyLogin> = Vec::new();
    for entry in &bridge_cfg.logins {
        let spec = LoginSpec {
            login_id: entry.login.login_id().to_string(),
            agent: entry.agent.clone(),
            ticket: entry.ticket.clone(),
        };
        match process::bring_up_login(
            &mut connected,
            process::MATRIX,
            &bridge_db_id,
            &identity,
            &sync_identity,
            &spec,
        )
        .await?
        {
            LoginOutcome::Ready => {
                let creds: MatrixCredentials = bridge_db
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
            "no Matrix logins are ready — none configured, or all are pending owner approval"
        );
    }

    let ownership = connected.capabilities.ownership();
    let generation = build_generation(
        &mut config,
        &config_path,
        &base,
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
                base,
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
                            &context.base,
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
    base: PathBuf,
    bridge_key: PublicKey,
    bridge_db_id: String,
    ready: Vec<ReadyLogin>,
}

/// Assemble the runtime on a connection and start one [`MatrixBridge`] per
/// ready login against it. A service reconnect calls this again on a fresh
/// connection after the previous generation is fully torn down.
async fn build_generation(
    config: &mut Config,
    config_path: &Path,
    base: &Path,
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
                process::MATRIX,
                bridge_db_id,
                &identity,
                &login.spec,
            )
            .await?;
        }
    }

    for login in ready {
        let login_state_dir = base
            .join("matrix")
            .join(sanitize_login_id(&login.spec.login_id))
            .to_string_lossy()
            .into_owned();
        let bridge = MatrixBridge::new(
            login.creds.clone(),
            login.spec.login_id.clone(),
            login.spec.agent.clone(),
            Some(login_state_dir),
            config.clone(),
            secret_store.clone(),
            shutdown.clone(),
        )?;
        let server = server.clone();
        info!(login = %login.spec.login_id, agent = %login.spec.agent, "Matrix login spawned");
        generation.spawn(async move { bridge.run(server).await });
    }
    Ok(generation)
}

/// Per-login matrix-client state-dir component: keep it filesystem-safe by
/// replacing anything outside `[A-Za-z0-9_-]` (MXIDs carry `@` and `:`).
fn sanitize_login_id(login_id: &str) -> String {
    login_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve the bridge config path: explicit `--config`, else
/// `$XDG_CONFIG_HOME/chaz/matrix-bridge.yaml`.
fn resolve_config_path(explicit: Option<&std::path::Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?;
    Ok(dir.join("chaz").join("matrix-bridge.yaml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `rooms leave-all` invocation through the real clap wiring,
    /// returning `(execute, retries, delay_ms)` or `None` when clap rejects it.
    fn parse_leave_all(args: &[&str]) -> Option<(bool, u32, u64)> {
        let argv = ["chaz-matrix", "rooms", "leave-all"]
            .into_iter()
            .chain(args.iter().copied())
            .collect::<Vec<_>>();
        let parsed = Args::try_parse_from(argv).ok()?;
        let Command::Rooms(rooms) = parsed.command?;
        match rooms.command {
            RoomsCommand::LeaveAll(args) => Some((args.execute, args.retries, args.delay_ms)),
            RoomsCommand::List(_) => None,
        }
    }

    #[test]
    fn room_commands_have_the_requested_shape() {
        let parsed = Args::try_parse_from(["chaz-matrix", "rooms", "list"]).unwrap();
        let Some(Command::Rooms(RoomsArgs {
            command: RoomsCommand::List(_),
        })) = parsed.command
        else {
            panic!("rooms list parsed as the wrong command");
        };

        assert_eq!(parse_leave_all(&[]), Some((false, 3, 250)));
        assert_eq!(parse_leave_all(&["--execute"]), Some((true, 3, 250)));
        assert!(Args::try_parse_from(["chaz-matrix", "rooms"]).is_err());
        assert!(Args::try_parse_from(["chaz-matrix", "rooms", "list", "--execute"]).is_err());
        assert!(Args::try_parse_from(["chaz-matrix", "rooms", "list", "--retries", "3"]).is_err());
    }

    #[test]
    fn leave_all_args_reject_zero_and_over_max() {
        assert!(parse_leave_all(&["--retries", "0"]).is_none());
        assert!(parse_leave_all(&["--delay-ms", "0"]).is_none());
        assert!(parse_leave_all(&["--retries", "11"]).is_none());
        assert!(parse_leave_all(&["--delay-ms", "60001"]).is_none());
        assert!(parse_leave_all(&["--retries", "many"]).is_none());
        assert!(parse_leave_all(&["--delay-ms", "soon"]).is_none());
    }

    #[test]
    fn leave_all_args_accept_boundaries_and_defaults() {
        assert_eq!(parse_leave_all(&["--retries", "1"]), Some((false, 1, 250)));
        assert_eq!(
            parse_leave_all(&["--retries", "10"]),
            Some((false, 10, 250))
        );
        assert_eq!(
            parse_leave_all(&["--delay-ms", "100"]),
            Some((false, 3, 100))
        );
        assert_eq!(
            parse_leave_all(&["--delay-ms", "60000"]),
            Some((false, 3, 60000))
        );
    }
}
