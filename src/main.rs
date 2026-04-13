mod agent;
mod backends;
mod config;
mod defaults;
mod gateway;
mod openai;
mod role;
mod router;
mod runtime;
mod session;
mod tool;
mod tools;
mod types;

use config::*;

use clap::Parser;
use std::{fs::File, io::Read, path::PathBuf};
use tracing::error;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct ChazArgs {
    /// Path to config file
    #[arg(short, long)]
    config: PathBuf,

    /// Run in TUI mode (stdin/stdout) instead of Matrix
    #[arg(long)]
    tui: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = ChazArgs::parse();
    let mut file = File::open(&args.config)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let config: Config = serde_yaml::from_str(&contents)?;
    *GLOBAL_CONFIG.lock().unwrap() = Some(config.clone());

    // Initialize eidetica for session management
    // Using InMemory backend for now due to libsqlite3-sys version conflict
    // between matrix-sdk (rusqlite) and eidetica (sqlx). SQLite persistence
    // can be enabled once the deps align.
    let backend = eidetica::backend::database::InMemory::new();
    let instance = eidetica::Instance::open(Box::new(backend)).await?;
    let _ = instance.create_user("chaz", None).await; // OK if already exists
    let user = instance.login_user("chaz", None).await?;

    // Resolve state directory for file persistence
    let state_dir = config
        .state_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz")));
    if let Some(dir) = &state_dir {
        std::fs::create_dir_all(dir)?;
    }

    let session_manager =
        session::SessionManager::new(instance, user, &config, state_dir.clone()).await?;

    // Register built-in tools
    let mut tools = tool::ToolRegistry::new();
    tools.register(tools::GetTime);
    tools.register(tools::Calculate);
    tools.register(tools::ShellExec);
    tools.register(tools::ReadFile);
    tools.register(tools::WriteFile);
    tools.register(tools::WebFetch);
    if let Some(dir) = &state_dir {
        tools.register(tools::Remember::new(dir));
        tools.register(tools::Recall::new(dir));
    }

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(100);

    // Spawn the router with session management and tools
    let router_handle = tokio::spawn(router::run(event_rx, session_manager, tools));

    // Run the selected gateway
    let result = if args.tui {
        let gateway = gateway::tui::TuiGateway::new(config);
        gateway.run(event_tx).await
    } else {
        let gateway = gateway::matrix::MatrixGateway::new(config)?;
        gateway.run(event_tx).await
    };

    if let Err(e) = result {
        error!("Gateway error: {e}");
    }

    router_handle.abort();
    Ok(())
}
