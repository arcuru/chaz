//! `chaz-matrix` — the standalone Matrix bridge, run as its own eidetica peer.
//!
//! This is its own process and `fn main()`, linking `chaz-core` + the
//! `chaz-matrix-bridge` library. Unlike the old in-process path, it does **not**
//! share the chaz daemon's database: it opens its **own** backend file, holds
//! its **own** key, and reaches each agent's DB through an access ticket
//! (`/agent share` on the daemon → `bootstrap_with_ticket` here), exactly the
//! way `/agent import` works. It owns no agents and runs no routine engine —
//! it is pure transport I/O: inbound Matrix messages are proxied into the
//! session DBs, and the daemon (a separate peer syncing those DBs) runs the
//! agents whose replies sync back and get delivered to the room.
//!
//! Bring-up order is load-bearing: sync must be enabled before access can be
//! bootstrapped, and the agent DBs must be ticket-bootstrapped before the
//! `Server` is assembled (so the hosted index discovers them rather than
//! minting local copies — see `BuildOptions::bootstrap_agents_from_config`).

use std::path::PathBuf;
use std::sync::Arc;

use chaz_core::agent_db::LoginRef;
use chaz_core::bridge::Bridge;
use chaz_core::bridge_db::{create_bridge_db, find_bridge_db};
use chaz_core::bridge_identity::{
    BRIDGE_KEY_NAME, BridgeIdentity, SyncBootstrap, ensure_bridge_key, establish_login,
};
use chaz_core::config::Config;
use chaz_core::server;
use chaz_core::session::BootstrapOutcome;

use chaz_matrix_bridge::{MatrixBridge, MatrixBridgeConfig, MatrixCredentials};

use clap::Parser;
use eidetica::sync::DatabaseTicket;
use tokio::sync::Notify;
use tracing::{error, info, warn};

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
struct ReadyLogin {
    login_id: String,
    owning_agent: String,
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

    // The bridge's OWN state dir — distinct from the chaz daemon's, since it is
    // a separate peer with a separate backend.
    let base = bridge_cfg
        .state_dir
        .clone()
        .or_else(|| config.state_dir.clone())
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz-matrix")))
        .ok_or_else(|| anyhow::anyhow!("could not determine a state directory"))?;
    std::fs::create_dir_all(&base)?;

    // Open this bridge's own eidetica peer (its own backend file, NOT the
    // daemon's eidetica.db).
    let eidetica_db_path = base.join("eidetica.db");
    let backend = eidetica::backend::database::SqlxBackend::open_sqlite(&eidetica_db_path).await?;
    let (instance, maybe_user) = eidetica::Instance::connect_or_create_backend(
        Box::new(backend),
        eidetica::NewUser::passwordless("chaz-matrix"),
    )
    .await?;
    let mut user = match maybe_user {
        Some(u) => u,
        None => instance.login_user("chaz-matrix", None).await?,
    };

    // `--print-pubkey` resolves the bridge's identity and stops here,
    // deliberately ahead of sync: binding a transport is pointless for a
    // question about a key, and it would make the lookup fail on an
    // already-taken port.
    if args.print_pubkey {
        let key = ensure_bridge_key(&mut user, BRIDGE_KEY_NAME).await?;
        println!("{key}");
        return Ok(());
    }

    // Enable sync up front — access bootstrap needs the live Sync handle, and
    // it must be reachable so the daemon can serve the agent DBs we request.
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
            info!("chaz-matrix sync address: {addr}");
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
    // Logins still pending owner approval are skipped until a re-run.
    // Where this bridge's sync identity is currently reachable. Published with
    // every login pointer so the daemon can correct a record that would
    // otherwise still name the address of a previous run — see `LoginRef`.
    let peer_pubkey = sync.get_device_pubkey().ok().map(|k| k.to_string());
    let sync_addresses = sync.get_all_server_addresses().await.unwrap_or_default();
    info!(
        pubkey = ?peer_pubkey,
        addresses = ?sync_addresses,
        "Publishing this bridge's sync identity with its logins"
    );

    let bootstrap = SyncBootstrap::new(sync.clone());
    let identity = BridgeIdentity {
        key: &bridge_key,
        key_name: BRIDGE_KEY_NAME,
    };
    let mut ready: Vec<ReadyLogin> = Vec::new();
    for entry in &bridge_cfg.logins {
        let login_id = entry.login.login_id().to_string();
        let ticket: DatabaseTicket = entry
            .ticket
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid ticket for login {login_id}: {e}"))?;
        let login_ref = LoginRef {
            kind: "matrix".to_string(),
            identifier: login_id.clone(),
            bridge_db_id: bridge_db_id.clone(),
            peer_pubkey: peer_pubkey.clone(),
            // Stamped by `establish_login` from the key actually bootstrapped.
            agent_pubkey: None,
            sync_addresses: sync_addresses.clone(),
        };
        match establish_login(&mut user, &bootstrap, &sync, &identity, &ticket, login_ref).await? {
            BootstrapOutcome::Approved => {
                let creds: MatrixCredentials = bridge_db
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
            "no Matrix logins are ready — none configured, or all are pending owner approval"
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
            // Long-lived: MCP tools land whenever their servers finish.
            mcp_readiness: server::McpReadiness::Deferred,
        },
    )
    .await?;

    // One MatrixBridge per ready login. The shutdown signal is never fired here
    // (the supervisor terminates the process); it exists to satisfy the bridge's
    // cooperative-shutdown contract.
    let shutdown = Arc::new(Notify::new());
    let mut handles = Vec::new();
    for r in ready {
        let login_state_dir = base
            .join("matrix")
            .join(sanitize_login_id(&r.login_id))
            .to_string_lossy()
            .into_owned();
        let bridge = MatrixBridge::new(
            r.creds,
            r.login_id.clone(),
            r.owning_agent.clone(),
            Some(login_state_dir),
            config.clone(),
            secret_store.clone(),
            shutdown.clone(),
        )?;
        let server = server.clone();
        info!(login = %r.login_id, agent = %r.owning_agent, "Matrix login spawned");
        handles.push(tokio::spawn(async move { bridge.run(server).await }));
    }

    // Await every login bridge; surface the first error.
    let mut result = Ok(());
    for handle in handles {
        let res = match handle.await {
            Ok(res) => res,
            Err(join) => Err(anyhow::anyhow!("matrix bridge task panicked: {join}")),
        };
        if let Err(e) = res {
            error!("Matrix bridge error: {e}");
            if result.is_ok() {
                result = Err(e);
            }
        }
    }
    result
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
