mod agent;
mod backends;
mod config;
mod defaults;
mod gateway;
mod openai;
mod role;
mod runtime;
mod security;
pub mod server;
mod session;
mod tool;
mod tools;
mod types;

use config::Config;
use gateway::Gateway;

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

    let mut config: Config = serde_yaml::from_str(&contents)?;

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
    let backend = eidetica::backend::database::SqlxBackend::open_sqlite(&eidetica_db_path).await?;
    let instance = eidetica::Instance::open(Box::new(backend)).await?;
    let _ = instance.create_user("chaz", None).await; // OK if already exists
    let user = instance.login_user("chaz", None).await?;

    let agent_registry = std::sync::Arc::new(agent::AgentRegistry::from_config(&config));
    let registry =
        session::SessionRegistry::new(instance, user, agent_registry.clone()).await?;
    let central_db = registry.central_db().clone();

    // Build secret store backed by the central eidetica database.
    let secret_store = security::SecretStore::new(central_db.clone()).await;
    if let Some(backends) = &mut config.backends {
        for backend in backends.iter_mut() {
            if let Some(raw_key) = backend.api_key.take() {
                let resolved = security::SecretStore::resolve_env(&raw_key).unwrap_or_else(|e| {
                    tracing::warn!(
                        "Failed to resolve API key for backend '{}': {e}",
                        backend.get_name()
                    );
                    raw_key
                });
                let ref_id = backend.secret_key();
                secret_store.insert(ref_id.clone(), resolved).await;
                backend.api_key_ref = Some(ref_id);
            }
        }
    }

    // Build security context from config
    let sec = config.security.clone().unwrap_or_default();
    let leak_policy = match sec.leak_policy.as_deref() {
        Some("block") => security::LeakPolicy::Block,
        _ => security::LeakPolicy::Redact,
    };
    let leak_detector = security::LeakDetector::new(leak_policy);
    let network_policy = std::sync::Arc::new(security::NetworkPolicy::new(
        sec.allowed_endpoints
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|e| security::network::EndpointPattern {
                host: e.host,
                path_prefix: e.path_prefix,
                methods: e.methods,
            })
            .collect(),
        true, // always deny private IPs
    ));
    let auto_approved: std::collections::HashSet<String> = sec
        .auto_approved_tools
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect();

    let security_ctx = security::SecurityContext {
        leak_detector,
        auto_approved_tools: auto_approved,
        approval_callback: None, // set per-session by server
    };

    // Register built-in tools
    let mut tools = tool::ToolRegistry::new();
    tools.register(tools::GetTime);
    tools.register(tools::Calculate);
    tools.register(tools::ShellExec::new(
        sec.shell_allowlist.clone(),
        sec.shell_denylist.clone(),
    ));
    tools.register(tools::ReadFile);
    tools.register(tools::WriteFile);
    tools.register(tools::WebFetch::new(network_policy));
    tools.register(tools::Remember::new(central_db.clone()));
    tools.register(tools::Recall::new(central_db));
    tools.register(tools::SpawnAgent);

    let tools = std::sync::Arc::new(tools);
    let registry = std::sync::Arc::new(registry);

    // Create the callback-driven server
    let server = server::Server::new(registry, agent_registry, tools, security_ctx);

    // Run the selected gateway
    let result = if args.tui {
        let gateway = gateway::tui::TuiGateway::new(config, secret_store);
        gateway.run(server).await
    } else {
        let gateway = gateway::matrix::MatrixGateway::new(config, secret_store)?;
        gateway.run(server).await
    };

    if let Err(e) = result {
        error!("Gateway error: {e}");
    }

    Ok(())
}
