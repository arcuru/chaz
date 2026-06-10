//! `chaz-discord` — a standalone Discord gateway for chaz.
//!
//! This is its own process and its own `fn main()`, linking `chaz-core` as a
//! library. It opens the same eidetica DB chaz uses, assembles a fully-wired
//! `Server` via [`chaz_core::server::build`], and runs a [`DiscordGateway`]
//! that maps one Discord channel to its session DB. Nothing is loaded into the
//! chaz binary; this is the "author writes their own binary against
//! chaz-core" composition model in concrete form.

mod config;
mod gateway;

use std::path::PathBuf;

use chaz_core::config::Config;
use chaz_core::gateway::Gateway;
use chaz_core::server;

use clap::Parser;
use tracing::info;

use crate::config::DiscordRoot;
use crate::gateway::DiscordGateway;

#[derive(Parser)]
#[command(author, version, about = "Standalone Discord gateway for chaz", long_about = None)]
struct Args {
    /// Path to the chaz config file (same file chaz reads). When unset, falls
    /// back to `$XDG_CONFIG_HOME/chaz/config.yaml`. The Discord-specific
    /// settings live under a `discord:` section in that same file.
    #[arg(short, long)]
    config: Option<PathBuf>,
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

    // Parse the same bytes twice: once as the full chaz config (state_dir,
    // backends, agents) and once for just our `discord:` section.
    let mut config: Config = serde_yaml::from_str(&contents)?;
    let discord = serde_yaml::from_str::<DiscordRoot>(&contents)?.discord;

    info!(config = %config_path.display(), "Starting chaz-discord");

    let state_dir = config
        .state_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz")));
    if let Some(dir) = &state_dir {
        std::fs::create_dir_all(dir)?;
    }

    // Open the same eidetica DB chaz uses (mirrors crates/bin/src/main.rs).
    let eidetica_db_path = state_dir
        .as_ref()
        .map(|d| d.join("eidetica.db"))
        .unwrap_or_else(|| PathBuf::from("eidetica.db"));
    let backend = eidetica::backend::database::SqlxBackend::open_sqlite(&eidetica_db_path).await?;
    let (instance, maybe_user) = eidetica::Instance::connect_or_create_backend(
        Box::new(backend),
        eidetica::NewUser::passwordless("chaz"),
    )
    .await?;
    let user = match maybe_user {
        Some(u) => u,
        None => instance.login_user("chaz", None).await?,
    };

    // A long-lived gateway: sync and the routine engine both run.
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
            enable_sync: true,
            run_routine_engine: true,
            extra_auto_approved_tools: Vec::new(),
        },
    )
    .await?;

    DiscordGateway::new(config, discord, secret_store)
        .run(server)
        .await
}

/// Resolve the chaz config path: explicit `--config`, else
/// `$XDG_CONFIG_HOME/chaz/config.yaml`.
fn resolve_config_path(explicit: Option<&std::path::Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?;
    Ok(dir.join("chaz").join("config.yaml"))
}
