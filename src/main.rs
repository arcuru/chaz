mod agent;
mod agent_db;
mod backends;
mod bubblewrap_host;
mod commands;
mod config;
mod context;
mod db_kind;
mod defaults;
mod embedding;
mod error;
mod extension;
mod extensions;
mod gateway;
mod grants;
mod heartbeat;
mod hosted_index;
mod mcp;
mod memory_bank_db;
mod openai;
mod persona;
mod role;
mod routine;
mod runtime;
mod security;
pub mod server;
mod session;
mod tool;
mod tool_host;
mod tools;
mod types;
mod util;
mod wasm_host;

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

    /// Run a single CLI prompt and exit. By default each invocation creates
    /// a fresh ephemeral session; pass --session NAME to reuse one.
    #[arg(long)]
    cli: bool,

    /// Named session to reuse with --cli (find-or-create). When omitted,
    /// --cli creates a fresh session per invocation.
    #[arg(long, requires = "cli", value_name = "NAME")]
    session: Option<String>,

    /// The prompt to send when --cli is used.
    #[arg(required_if_eq("cli", "true"))]
    prompt: Option<String>,

    #[command(subcommand)]
    subcommand: Option<Subcommand>,
}

#[derive(clap::Subcommand)]
enum Subcommand {
    /// Aggregate LLM usage and cost across all sessions, then exit.
    /// Reads the user-central session catalog; no gateway is started.
    Usage(UsageArgs),
}

#[derive(clap::Args)]
struct UsageArgs {
    /// Emit the rollup as JSON for machine consumption.
    #[arg(long)]
    json: bool,

    /// Only include sessions originating from this gateway (cli, tui,
    /// matrix, spawn, other).
    #[arg(long, value_name = "KIND")]
    gateway: Option<String>,

    /// Skip sessions marked closed.
    #[arg(long)]
    active_only: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = ChazArgs::parse();

    if args.tui && args.cli {
        anyhow::bail!("--tui and --cli are mutually exclusive");
    }

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

    // Subcommand short-circuit: read-only utilities open the DB, do their
    // work, and exit — no gateway, scheduler, MCP, or sync setup.
    if let Some(sub) = args.subcommand {
        // Bare stderr logging — stdout is reserved for the subcommand's
        // own output (text or JSON) so it stays pipe-friendly.
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
        return match sub {
            Subcommand::Usage(usage_args) => {
                run_usage_subcommand(usage_args, &config, state_dir.as_deref()).await
            }
        };
    }

    // Init tracing. Honour RUST_LOG; default to info when unset.
    //
    // - TUI: stdout belongs to ratatui, so logs go to a rolling file
    //   (the alt-screen buffer gets corrupted by stray writes).
    // - CLI: stdout is reserved for the model's reply so it can be piped /
    //   captured cleanly. Logs go to a rolling file mirroring the TUI path.
    // - Matrix (default): logs go to stdout, where systemd / docker / etc.
    //   collect them via their usual mechanisms.
    //
    // File-mode rotations: daily, keep the last 7 days. Tail the file in
    // another terminal to follow live.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _file_log_guard = if args.tui || args.cli {
        let log_dir = state_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        let prefix = if args.tui { "chaz-tui" } else { "chaz-cli" };
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(prefix)
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
            "chaz {} logs: {}/{}.log (daily, keeps 7 days)",
            if args.tui { "TUI" } else { "CLI" },
            log_dir.display(),
            prefix,
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

    // Enable eidetica sync for session sharing. Register iroh P2P transport
    // by default (stable peer identity, no address config needed). If
    // sync_listen is configured, also bind HTTP for traditional access.
    //
    // Skipped in --cli mode: starting iroh, registering with the n0 relay,
    // and spinning up the 300s periodic-sync engine all run *after* exit
    // for one-shot CLI invocations. The setup is pure overhead and exposes
    // a public sync endpoint that lives for the lifetime of the process —
    // a few seconds. Long-lived TUI/Matrix modes still get full sync.
    if !args.cli {
        instance.enable_sync().await?;
        if let Some(sync) = instance.sync() {
            use eidetica::sync::transports::iroh::IrohTransport;
            sync.register_transport("iroh", IrohTransport::builder())
                .await?;

            if let Some(ref addr) = config.sync_listen {
                use eidetica::sync::transports::http::HttpTransport;
                sync.register_transport("http", HttpTransport::builder().bind(addr))
                    .await?;
                info!("Sync HTTP transport listening on {addr}");
            }

            sync.accept_connections().await?;
            if let Ok(addr) = sync.get_server_address().await {
                info!("Eidetica sync address: {addr}");
            }
        }
    }

    // Surface deprecation warnings for legacy role-based config so users
    // get a single, explicit nudge per startup.
    warn_on_legacy_role_config(&config);

    let agent_registry = std::sync::Arc::new(agent::AgentRegistry::from_config(&config));
    if agent_registry.is_empty() {
        agent_registry.register_default_chaz(&config)?;
    }
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

    // Same env-resolution dance for the embedding API key, then build the
    // shared `Arc<dyn Embedder>` (None when no embedding section configured).
    if let Some(emb) = &mut config.embedding
        && let Some(raw_key) = emb.api_key.take()
    {
        let resolved = security::SecretStore::resolve_env(&raw_key).unwrap_or_else(|e| {
            tracing::warn!("Failed to resolve API key for embedding: {e}");
            raw_key
        });
        let ref_id = emb.secret_key();
        secret_store.insert(ref_id.clone(), resolved).await;
        emb.api_key_ref = Some(ref_id);
    }
    let embedder = match embedding::build_embedder(config.embedding.as_ref(), &secret_store) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!("Embedding config invalid; falling back to lexical-only: {err}");
            None
        }
    };
    if let Some(e) = embedder.as_ref() {
        info!(model_id = %e.model_id(), "Embedder configured");
    }

    // Build security context from config
    let sec = config.security.clone().unwrap_or_default();
    let leak_policy = match sec.leak_policy.as_deref() {
        Some("block") => security::LeakPolicy::Block,
        _ => security::LeakPolicy::Redact,
    };
    let leak_detector = security::LeakDetector::new(leak_policy);
    let mut auto_approved: std::collections::HashSet<String> = sec
        .auto_approved_tools
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect();

    // In CLI mode there is no interactive approval; add the configured
    // (or default) CLI auto-approved tools so shell/write_file work.
    if args.cli {
        let cli_tools = config
            .cli
            .as_ref()
            .map(|c| c.auto_approved_tools.clone())
            .unwrap_or_else(config::default_cli_auto_approved);
        auto_approved.extend(cli_tools);
    }

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

    // Build the extension hub and reserve built-in slash command names so
    // extensions can't shadow them.
    let mut extension_hub = extension::ExtensionHub::new();
    extension_hub.reserve_builtin_commands(commands::BUILTIN_COMMAND_NAMES.iter().copied());

    // SpawnAgent / SpawnTask route through the server — a single OnceLock
    // is shared; it's set once after Server::new below. The core extension
    // takes ownership of the cell and constructs the spawn tools.
    let spawn_server_cell = std::sync::Arc::new(std::sync::OnceLock::new());

    // Install every built-in extension on the hub via the cap-based
    // install path. Tools and commands flow through the per-extension
    // `caps.tool_registration` / `caps.command_registration` queues that
    // `install_all` drains; hook handlers returned in each extension's
    // `InstalledExtension` are bridged into the legacy hook vectors so
    // the existing `fire_*` paths run unchanged.
    extension_hub.set_session_registry(registry.clone());
    extension_hub
        .install_all(extensions::all_builtins(extensions::BuiltinDeps {
            agent_index: agent_index_store.clone(),
            session_registry: registry.clone(),
            embedder: embedder.clone(),
            web_search_backends,
            spawn_server_cell: spawn_server_cell.clone(),
            backend_manager: backends::BackendManager::new(&config.backends, secret_store.clone()),
            security: security_ctx.clone(),
        }))
        .await?;
    let extension_names = extension_hub.extension_names();
    if !extension_names.is_empty() {
        info!(?extension_names, "Extensions registered");
    }

    // Build the legacy ToolRegistry from extension-contributed tools plus
    // any MCP-provided tools. MCP tools don't fit the extension model yet
    // (they're config-driven at startup and may grow/shrink at reload time)
    // so they continue to live directly in the registry — they're just
    // un-attributed for now.
    let mut tool_registry = tool::ToolRegistry::new();
    for (owner, _name, tool) in extension_hub.tools_for_registry() {
        tool_registry.register_arc_owned(tool, Some(owner));
    }

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

    let extension_hub = std::sync::Arc::new(extension_hub);

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
    let tool_host = std::sync::Arc::new(tool_host::NativeToolHost::new())
        as std::sync::Arc<dyn tool_host::ToolHost>;

    let server = server::Server::new(
        registry.clone(),
        agent_registry,
        agent_index_store,
        memory_bank_index_store,
        tool_registry,
        policies,
        security_ctx,
        tool_profiles,
        context_config,
        tool_host,
        extension_hub,
    );
    assert!(
        spawn_server_cell.set(server.clone()).is_ok(),
        "Spawn tool server cell already set"
    );

    // Translate YAML schedules into session-scoped Routine rows. Each
    // ScheduleConfig becomes one cron Routine targeting the scheduler
    // extension on the resolved session's `routines` table; the engine
    // (spawned below) picks them up via `register_session`. Idempotent
    // by routine id == schedule name.
    if let Some(schedules) = config.schedules.clone() {
        for cfg in schedules {
            if !cfg.enabled {
                info!(schedule = %cfg.name, "Schedule disabled, skipping");
                continue;
            }
            let (_conv, sdb) = match registry.resolve_session(&cfg.session).await {
                Ok(s) => s,
                Err(e) => {
                    error!(
                        schedule = %cfg.name,
                        session = %cfg.session,
                        "Failed to resolve schedule target session: {e}"
                    );
                    continue;
                }
            };
            let existing = routine::list_session_routines(&sdb)
                .await
                .unwrap_or_default();
            if existing.iter().any(|r| r.id.as_str() == cfg.name) {
                info!(
                    schedule = %cfg.name,
                    "Schedule already present as routine; leaving in place"
                );
                continue;
            }
            let payload = match serde_json::to_value(extensions::scheduler::SchedulePayload {
                schedule_name: cfg.name.clone(),
                task: cfg.task.clone(),
            }) {
                Ok(v) => v,
                Err(e) => {
                    error!(schedule = %cfg.name, "Failed to encode payload: {e}");
                    continue;
                }
            };
            let r = routine::Routine::cron(
                routine::RoutineId::new(&cfg.name),
                &cfg.name,
                cfg.cron.clone(),
                routine::RoutineTarget {
                    extension: "scheduler".into(),
                    payload,
                },
            );
            if let Err(e) = routine::upsert_session_routine(&sdb, &r).await {
                error!(schedule = %cfg.name, "Failed to upsert schedule routine: {e}");
            } else {
                info!(
                    schedule = %cfg.name,
                    session = %cfg.session,
                    cron = %cfg.cron,
                    "Schedule registered as session-scoped routine"
                );
            }
        }
    }

    // Spawn the routine engine. Loads global routines from
    // `chaz_peer.routines`, then walks every hosted session and
    // registers its session-scoped routines (heartbeats + scheduler
    // fires). Skipped in --cli mode: a single ReAct loop doesn't need
    // the engine running.
    if !args.cli {
        let engine =
            routine::RoutineEngine::new(chaz_peer.clone(), Some(server.extensions().clone()))
                .await?;
        // Pick up every session's routines + ensure the server is
        // watching those sessions so directive writes from fires drive
        // an agent turn.
        let sessions = registry.list_sessions().await.unwrap_or_default();
        let default_backend = backends::BackendManager::new(&config.backends, secret_store.clone());
        for s in sessions {
            let Ok((_conv, sdb)) = registry.open_session(&s.session_db_id).await else {
                continue;
            };
            if let Err(e) = engine.register_session(&s.session_db_id, &sdb).await {
                error!(session = %s.session_db_id, "engine.register_session failed: {e}");
                continue;
            }
            let routines = routine::list_session_routines(&sdb)
                .await
                .unwrap_or_default();
            if routines.is_empty() {
                continue;
            }
            if let Err(e) = server
                .register_session(&sdb, default_backend.clone(), None, None)
                .await
            {
                error!(session = %s.session_db_id, "server.register_session failed: {e}");
            }
        }
        let engine_clone = engine.clone();
        tokio::spawn(async move {
            engine_clone.run().await;
        });

        // Legacy heartbeat runner — gutted in commit C; survives only
        // so this call site keeps compiling until commit F deletes it.
        let heartbeat_runner = heartbeat::HeartbeatRunner::new(server.clone(), chaz_peer);
        heartbeat_runner.start();
    }

    // Run the selected gateway
    let mode = if args.cli {
        "cli"
    } else if args.tui {
        "tui"
    } else {
        "matrix"
    };
    info!(mode, "Starting gateway");
    let result = if args.cli {
        let prompt = args.prompt.clone().expect("--cli requires PROMPT");
        let gateway = gateway::cli::CliGateway::new(config, secret_store, prompt, args.session);
        gateway.run(server).await
    } else if args.tui {
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

/// One-shot deprecation banner at startup. Logs a single warning per
/// legacy concept (`role:` and `roles:`) so users know to migrate to
/// `agents: [{persona: ...}]`. Silent when neither legacy key is set.
fn warn_on_legacy_role_config(config: &Config) {
    if config.role.is_some() {
        tracing::warn!(
            "config.role is deprecated; declare an `agents:` block with a default agent and an embedded `persona:` instead. See docs/src/user_guide/agents.md."
        );
    }
    if config.roles.is_some() {
        tracing::warn!(
            "config.roles is deprecated; built-in personas now live on agents. Migrate user-defined roles into `agents[].persona.prompt` (or `persona.files` for file-backed prompts)."
        );
    }
    if let Some(agents) = config.agents.as_ref() {
        for a in agents {
            if a.persona.is_none() && a.role.is_some() {
                tracing::warn!(
                    agent = %a.name,
                    role = ?a.role,
                    "agent.role is deprecated; the role's prompt is auto-migrated into a persona at runtime, but new configs should use `persona:` directly."
                );
            }
        }
    }
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

/// `chaz usage` — open the eidetica DB read-only, walk the user-central
/// session catalog, aggregate per-message `ResponseMetadata`, print either
/// human-readable text or JSON, then exit. Skips all gateway/sync/scheduler
/// setup since we never serve a session here.
async fn run_usage_subcommand(
    args: UsageArgs,
    config: &Config,
    state_dir: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let gateway_filter = match args.gateway.as_deref() {
        Some(s) => Some(session::GatewayKind::from_filter_str(s).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown --gateway value '{s}' (expected: cli, tui, matrix, spawn, other)"
            )
        })?),
        None => None,
    };

    let eidetica_db_path = state_dir
        .map(|d| d.join("eidetica.db"))
        .unwrap_or_else(|| PathBuf::from("eidetica.db"));
    let backend = eidetica::backend::database::SqlxBackend::open_sqlite(&eidetica_db_path).await?;
    let instance = eidetica::Instance::open(Box::new(backend)).await?;
    let _ = instance.create_user("chaz", None).await;
    let user = instance.login_user("chaz", None).await?;

    let agent_registry = std::sync::Arc::new(agent::AgentRegistry::from_config(config));
    if agent_registry.is_empty() {
        agent_registry.register_default_chaz(config)?;
    }
    let registry = session::SessionRegistry::new(instance, user, agent_registry).await?;

    let filter = session::usage::UsageFilter {
        since: None,
        gateway: gateway_filter,
        active_only: args.active_only,
    };
    let rollup = session::usage::collect_usage(&registry, &filter).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rollup)?);
    } else {
        print!("{}", session::usage::render_text(&rollup));
    }
    Ok(())
}
