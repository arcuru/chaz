mod agent;
mod agent_db;
mod agent_index;
mod backends;
mod commands;
mod config;
mod context;
mod defaults;
mod error;
mod gateway;
mod grants;
mod heartbeat;
mod mcp;
mod memory_bank_db;
mod memory_bank_index;
mod openai;
mod role;
mod runtime;
mod scheduler;
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
use tracing::{error, info};

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
    info!(config = %args.config.display(), tui = args.tui, "Starting chaz");

    let mut file = File::open(&args.config)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let mut config: Config = serde_yaml::from_str(&contents)?;
    info!("Config loaded from {}", args.config.display());

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
    let mut user = instance.login_user("chaz", None).await?;

    // Enable eidetica sync with HTTP transport for session sharing
    instance.enable_sync().await?;
    if let Some(sync) = instance.sync() {
        use eidetica::sync::transports::http::HttpTransport;
        sync.register_transport("http", HttpTransport::builder())
            .await?;
        sync.accept_connections().await?;
        match sync.get_server_address().await {
            Ok(addr) => info!("Eidetica sync listening on {addr}"),
            Err(e) => tracing::warn!("Could not get sync server address: {e}"),
        }
    }

    let agent_registry = std::sync::Arc::new(agent::AgentRegistry::from_config(&config));
    info!(
        agents = agent_registry.names().len(),
        "Agent registry initialized"
    );

    // Stage 1 of Living Agents: materialize an eidetica DB per yaml-declared
    // agent. Idempotent on re-runs. Routing still uses the in-memory
    // AgentRegistry; Stage 3 switches that to key-possession checks.
    let mut agent_dbs = agent_db::bootstrap_from_config(&mut user, &config).await?;
    if !agent_dbs.is_empty() {
        info!(
            count = agent_dbs.len(),
            "Agent DBs bootstrapped from config"
        );
    }

    // Every AgentRegistry entry needs an AgentDb for Stage 7 per-agent memory
    // to resolve. The default `chaz` agent (when no yaml `agents:` block) has
    // no bootstrap entry — fill it in here.
    for name in agent_registry.names() {
        if let std::collections::hash_map::Entry::Vacant(slot) = agent_dbs.entry(name.clone()) {
            let bs = agent_db::ensure_agent_db(&mut user, &name).await?;
            info!(agent = %name, db_id = %bs.db.id(), "Created default Agent DB");
            slot.insert(bs);
        }
    }

    let registry = session::SessionRegistry::new(instance, user, agent_registry.clone()).await?;
    let central_db = registry.central_db().clone();

    // Stage 2 of Living Agents: maintain a local index of which Agent DBs
    // this peer hosts. Needed for O(1) routing in Stage 3 (eidetica has no
    // inverse "DBs where key K has permission P" query).
    let agent_index_store = agent_index::AgentIndex::new(central_db.clone());
    agent_index_store.sync_from_bootstrap(&agent_dbs).await?;

    // Memory Banks Stage 9.D: peer-local index of bank DBs this peer hosts,
    // maintained alongside the agent index. Same rationale (no inverse
    // list-my-DBs query in eidetica). Populated by `/memory new` + `/memory
    // import` — nothing populates it at startup yet.
    let memory_bank_index_store = memory_bank_index::MemoryBankIndex::new(central_db.clone());

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

    // Build tool policy registry from config, merging legacy SecurityConfig
    // fields (shell_allowlist/denylist, allowed_endpoints) into per-tool grants.
    let policy_overrides =
        grants::merge_legacy_security(sec.tool_policies.clone().unwrap_or_default(), &sec);
    let policies = std::sync::Arc::new(tool::ToolPolicyRegistry::new(policy_overrides));

    let registry = std::sync::Arc::new(registry);

    // Register built-in tools
    let mut tool_registry = tool::ToolRegistry::new();
    tool_registry.register(tools::GetTime);
    tool_registry.register(tools::Calculate);
    tool_registry.register(tools::DescribeTool);
    tool_registry.register(tools::ShellExec);
    tool_registry.register(tools::ReadFile);
    tool_registry.register(tools::WriteFile);
    tool_registry.register(tools::WebFetch);
    tool_registry.register(tools::Remember::new(
        registry.clone(),
        agent_index_store.clone(),
    ));
    tool_registry.register(tools::Recall::new(
        registry.clone(),
        agent_index_store.clone(),
    ));
    tool_registry.register(tools::ListMemoryBanks::new(
        registry.clone(),
        agent_index_store.clone(),
    ));
    tool_registry.register(tools::Compact);
    // SpawnAgent / SpawnTask both route through the server — a single OnceLock
    // is shared; it's set once after Server::new below.
    let spawn_server_cell = std::sync::Arc::new(std::sync::OnceLock::new());
    tool_registry.register(tools::SpawnAgent {
        server: spawn_server_cell.clone(),
        backend: backends::BackendManager::new(&config.backends, secret_store.clone()),
        security: security_ctx.clone(),
    });
    tool_registry.register(tools::SpawnTask {
        server: spawn_server_cell.clone(),
        backend: backends::BackendManager::new(&config.backends, secret_store.clone()),
        security: security_ctx.clone(),
    });

    // Collect MCP server configs from inline config + directory scanning
    let mut mcp_configs: Vec<config::McpServerConfig> =
        config.mcp_servers.clone().unwrap_or_default();
    if let Some(dir) = &config.mcp_server_dir {
        let dir_path = std::path::Path::new(dir);
        let dir_configs = mcp::load_server_configs_from_dir(dir_path);
        if !dir_configs.is_empty() {
            info!(
                count = dir_configs.len(),
                dir = %dir,
                "Loaded MCP server configs from directory"
            );
        }
        mcp_configs.extend(dir_configs);
    }

    // Start MCP servers and register their tools
    if !mcp_configs.is_empty() {
        let mcp_tools = mcp::start_mcp_servers(&mcp_configs).await;
        let mcp_count = mcp_tools.len();
        for t in mcp_tools {
            tool_registry.register_boxed(t);
        }
        if mcp_count > 0 {
            info!(mcp_tools = mcp_count, "MCP tools registered");
        }
    }

    info!("Tool registry initialized");
    let tool_registry = std::sync::Arc::new(tool_registry);

    // Build tool profiles from config
    let tool_profiles: std::collections::HashMap<String, tool::ToolProfile> = config
        .tool_profiles
        .as_ref()
        .map(|profiles| {
            profiles
                .iter()
                .map(|(name, cfg)| {
                    let profile = tool::ToolProfile {
                        default_mode: cfg.default.clone().unwrap_or_default(),
                        tool_modes: cfg.tools.clone().unwrap_or_default(),
                    };
                    (name.clone(), profile)
                })
                .collect()
        })
        .unwrap_or_default();

    // Create the callback-driven server
    let context_config = config.context.clone().unwrap_or_default();
    let server = server::Server::new(
        registry,
        agent_registry,
        agent_index_store,
        memory_bank_index_store,
        tool_registry,
        policies,
        security_ctx,
        tool_profiles,
        context_config,
    );
    assert!(
        spawn_server_cell.set(server.clone()).is_ok(),
        "Spawn tool server cell already set"
    );

    // Start the scheduler if any schedules are configured
    let scheduler = if let Some(schedules) = config.schedules.clone() {
        if !schedules.is_empty() {
            let sched = std::sync::Arc::new(
                scheduler::Scheduler::new(
                    schedules,
                    server.clone(),
                    backends::BackendManager::new(&config.backends, secret_store.clone()),
                    central_db.clone(),
                )
                .await,
            );
            sched.start();
            Some(sched)
        } else {
            None
        }
    } else {
        None
    };

    // Start the heartbeat runner. Polls every 30s across all hosted sessions
    // for due rules whose target agent this peer hosts.
    let heartbeat_runner = heartbeat::HeartbeatRunner::new(server.clone(), central_db);
    heartbeat_runner.start();

    // Run the selected gateway
    info!(
        mode = if args.tui { "tui" } else { "matrix" },
        "Starting gateway"
    );
    let result = if args.tui {
        let gateway = gateway::tui::TuiGateway::new(config, secret_store).with_scheduler(scheduler);
        gateway.run(server).await
    } else {
        let gateway =
            gateway::matrix::MatrixGateway::new(config, secret_store)?.with_scheduler(scheduler);
        gateway.run(server).await
    };

    if let Err(e) = result {
        error!("Gateway error: {e}");
    }

    Ok(())
}
