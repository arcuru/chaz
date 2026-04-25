mod agent;
mod agent_db;
mod backends;
mod commands;
mod config;
mod context;
mod db_kind;
mod defaults;
mod error;
mod gateway;
mod grants;
mod heartbeat;
mod hosted_index;
mod mcp;
mod memory_bank_db;
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
mod util;

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

    // Init tracing. Honour RUST_LOG; default to info when unset.
    // In TUI mode stdout belongs to ratatui, so we route logs to a rolling
    // file instead (the alt-screen buffer gets corrupted by stray writes).
    // Daily rotation, keep the last 7 days. Users can tail the file in
    // another terminal.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _tui_log_guard = if args.tui {
        let log_dir = state_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("chaz-tui")
            .filename_suffix("log")
            .max_log_files(7)
            .build(&log_dir)?;
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();
        eprintln!(
            "chaz TUI logs: {}/chaz-tui.log (daily, keeps 7 days)",
            log_dir.display()
        );
        Some(guard)
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        None
    };

    info!(config = %args.config.display(), tui = args.tui, "Starting chaz");
    info!("Config loaded from {}", args.config.display());

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

    // Materialize an eidetica DB per yaml-declared agent. Idempotent on
    // re-runs (yaml is a first-boot template; AgentDb is the source of
    // truth afterwards).
    let bootstrapped = agent_db::bootstrap_from_config(&mut user, &config).await?;
    if !bootstrapped.is_empty() {
        info!(
            count = bootstrapped.len(),
            "Agent DBs bootstrapped from config"
        );
    }

    // Every AgentRegistry entry needs an AgentDb so per-agent memory tools
    // resolve. The default `chaz` agent (when no yaml `agents:` block) has
    // no bootstrap entry — ensure one exists.
    for name in agent_registry.names() {
        if !bootstrapped.contains_key(&name) {
            let bs = agent_db::ensure_agent_db(&mut user, &name).await?;
            info!(agent = %name, db_id = %bs.db.id(), "Created default Agent DB");
        }
    }

    let registry = session::SessionRegistry::new(instance, user, agent_registry.clone()).await?;
    let chaz_peer = registry.chaz_peer().clone();

    // Stage 3 of the Database Layout Refactor: build the peer-local Agent
    // and Memory Bank indices in-memory by walking eidetica's tracked-DBs
    // list. `meta.kind` (Stage 4) classifies each entry. `/agent new`,
    // `/memory new`, `/agent delete`, etc. mutate these caches at runtime.
    let (agent_index_store, memory_bank_index_store) = {
        let user = registry.user_lock().await;
        hosted_index::build_from_user(&user).await?
    };

    // Build secret store backed by the chaz_peer DB.
    let secret_store = security::SecretStore::new(chaz_peer.clone()).await;
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

    // Resolve the web_search API key (if any) into the secret store, same
    // `${VAR}` handling as LLM backend keys.
    let web_search_backends = build_web_search_backends(&mut config, &secret_store).await;

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
    tool_registry.register(tools::WebSearch::new(web_search_backends));
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
    tool_registry.register(tools::HeartbeatAdd::new(agent_index_store.clone()));
    tool_registry.register(tools::HeartbeatModify::new(agent_index_store.clone()));
    tool_registry.register(tools::HeartbeatRemove);
    tool_registry.register(tools::HeartbeatList::new(agent_index_store.clone()));
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
                    chaz_peer.clone(),
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
    let heartbeat_runner = heartbeat::HeartbeatRunner::new(server.clone(), chaz_peer);
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

/// Resolve the configured web-search backend: extract its API key (if any)
/// into the SecretStore, then materialize the `SearchBackend` enum. Falls
/// back to DuckDuckGo HTML scraping when no config or no key is present.
/// Missing keys for API-backed providers log a warning and also fall back to
/// DuckDuckGo rather than failing startup.
async fn build_web_search_backends(
    config: &mut Config,
    secrets: &security::SecretStore,
) -> Vec<tools::SearchBackend> {
    use config::WebSearchBackendKind as Kind;
    let Some(ws_config) = config.web_search.as_mut() else {
        info!(chain = ?["duckduckgo"], "web_search backends (default)");
        return vec![tools::SearchBackend::DuckDuckGo];
    };

    let mut built: Vec<tools::SearchBackend> = Vec::with_capacity(ws_config.backends.len());
    for (idx, entry) in ws_config.backends.iter_mut().enumerate() {
        // Resolve `${VAR}` in api_key, then stash the secret under a unique
        // per-entry ref ID. Same env-resolution pattern as LLM backend keys.
        let resolved_key = entry.api_key.take().and_then(|raw| {
            let resolved = security::SecretStore::resolve_env(&raw).unwrap_or_else(|e| {
                tracing::warn!("Failed to resolve web_search.backends[{idx}] api_key: {e}");
                raw
            });
            if resolved.is_empty() {
                None
            } else {
                Some(resolved)
            }
        });
        if let Some(ref key) = resolved_key {
            let ref_id = format!("secret:web_search.{idx}.api_key");
            secrets.insert(ref_id.clone(), key.clone()).await;
            entry.api_key_ref = Some(ref_id);
        }

        let needs_key = matches!(
            entry.kind,
            Kind::Kagi | Kind::Tavily | Kind::Brave | Kind::Serper
        );
        if needs_key && resolved_key.is_none() {
            tracing::warn!(
                index = idx,
                backend = ?entry.kind,
                "web_search backend requires an api_key — skipping"
            );
            continue;
        }
        match entry.kind {
            Kind::Kagi => built.push(tools::SearchBackend::Kagi {
                api_key: resolved_key.expect("needs_key guard"),
            }),
            Kind::Tavily => built.push(tools::SearchBackend::Tavily {
                api_key: resolved_key.expect("needs_key guard"),
            }),
            Kind::Brave => built.push(tools::SearchBackend::Brave {
                api_key: resolved_key.expect("needs_key guard"),
            }),
            Kind::Serper => built.push(tools::SearchBackend::Serper {
                api_key: resolved_key.expect("needs_key guard"),
            }),
            Kind::Searxng => {
                let Some(base_url) = entry.url.clone() else {
                    tracing::warn!(
                        index = idx,
                        "web_search searxng entry missing required `url:` — skipping"
                    );
                    continue;
                };
                built.push(tools::SearchBackend::Searxng { base_url });
            }
            Kind::DuckDuckGo => built.push(tools::SearchBackend::DuckDuckGo),
        }
    }

    if built.is_empty() {
        tracing::warn!(
            "web_search: no usable backends after resolution — falling back to duckduckgo"
        );
        built.push(tools::SearchBackend::DuckDuckGo);
    }

    let chain: Vec<&'static str> = built
        .iter()
        .map(|b| match b {
            tools::SearchBackend::Kagi { .. } => "kagi",
            tools::SearchBackend::Tavily { .. } => "tavily",
            tools::SearchBackend::Brave { .. } => "brave",
            tools::SearchBackend::Serper { .. } => "serper",
            tools::SearchBackend::Searxng { .. } => "searxng",
            tools::SearchBackend::DuckDuckGo => "duckduckgo",
        })
        .collect();
    info!(chain = ?chain, "web_search backends");
    built
}
