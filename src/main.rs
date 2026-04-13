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

use config::Config;

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

    // Resolve state directory for persistence
    let state_dir = config
        .state_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz")));
    if let Some(dir) = &state_dir {
        std::fs::create_dir_all(dir)?;
    }

    // Initialize eidetica with SQLite backend for persistent storage
    let eidetica_db_path = state_dir
        .as_ref()
        .map(|d| d.join("eidetica.db"))
        .unwrap_or_else(|| PathBuf::from("eidetica.db"));
    let backend =
        eidetica::backend::database::SqlxBackend::open_sqlite(&eidetica_db_path).await?;
    let instance = eidetica::Instance::open(Box::new(backend)).await?;
    let _ = instance.create_user("chaz", None).await; // OK if already exists
    let user = instance.login_user("chaz", None).await?;

    let session_manager = session::SessionManager::new(instance, user, &config).await?;
    let memory_db = session_manager.database().clone();

    // Register built-in tools
    let mut tools = tool::ToolRegistry::new();
    tools.register(tools::GetTime);
    tools.register(tools::Calculate);
    tools.register(tools::ShellExec);
    tools.register(tools::ReadFile);
    tools.register(tools::WriteFile);
    tools.register(tools::WebFetch);
    tools.register(tools::Remember::new(memory_db.clone()));
    tools.register(tools::Recall::new(memory_db));

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
