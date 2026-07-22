//! Assembling a fully-wired [`Server`] from a [`Config`] — the shared bootstrap
//! path for every chaz binary.
//!
//! The `chaz` binary and any standalone bridge binary (e.g. a Discord bridge)
//! need the *same* runtime behind their transport: eidetica opened, the session
//! registry built, per-agent DBs bootstrapped, the secret store + extension hub
//! assembled, the [`Server`] constructed with the spawn-tool cell wired, agent
//! configs reconciled, YAML schedules materialized, and the routine engine
//! spawned. That sequence is load-bearing (notably the `spawn_server_cell`
//! `OnceLock` must be set *after* `Server::new`, and the extension hub must be
//! `install_all`'d before it) and historically lived inline in the `chaz`
//! binary's `main`. [`build`] is the single shared implementation so external
//! bridge binaries don't copy-paste it.
//!
//! The caller opens eidetica (choosing the backend/path) and hands in the
//! `Instance` + `User`; [`build`] does everything else and returns the wired
//! [`Server`] plus the handles a bridge needs alongside it ([`BuiltServer`]).
//! Mode-specific behavior the config can't express is passed via
//! [`BuildOptions`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tracing::{error, info, warn};

use crate::config::{self, Config};
use crate::server::Server;
use crate::{
    agent, agent_db, backends, commands, db_kind, embedding, extension, extensions, grants,
    hosted_index, mcp, memory_bank_db, routine, security, session, tool, tool_host, tools,
};

/// Knobs [`build`] can't infer from [`Config`] — behavior a one-shot CLI run and
/// a long-lived bridge decide differently.
pub struct BuildOptions {
    /// Path the config was loaded from; recorded on the server so `/agent
    /// reload` (and config reconcile) can re-read it.
    pub config_path: PathBuf,
    /// Enable eidetica P2P sync (iroh transport + optional HTTP bind from
    /// `config.sync_listen`). A one-shot CLI invocation leaves this off — the
    /// engine and public endpoint would outlive the single ReAct loop.
    pub enable_sync: bool,
    /// Spawn the routine engine (heartbeats + schedulers) and register every
    /// hosted session/agent with it. Off for one-shot CLI runs.
    pub run_routine_engine: bool,
    /// Materialize an eidetica AgentDb per config-declared agent (minting one
    /// locally on this peer). True for the chaz daemon, which owns its agents.
    /// A standalone bridge sets this **false**: it owns no agents — it
    /// ticket-bootstraps each agent DB from the daemon before calling `build`,
    /// and the hosted index then discovers those synced DBs. Minting locally
    /// here would fork a second, divergent agent DB against the daemon's.
    pub bootstrap_agents_from_config: bool,
    /// Spawn the agent-running `processing_loop`. True for the chaz daemon,
    /// which runs agents. A standalone (dumb) bridge sets this **false**: it
    /// proxies inbound messages into session DBs and delivers replies back to
    /// its transport, but never runs an agent — the daemon watches the
    /// exposed sessions and runs them. Distinct from `run_routine_engine`,
    /// which only gates schedulers/heartbeats, not the ReAct message loop.
    pub run_agent_loop: bool,
    /// Tools to add to the auto-approved set on top of `config.security`. The
    /// CLI passes its non-interactive allowlist here so `shell`/`write_file`
    /// work under `--print` where there is no interactive approval.
    pub extra_auto_approved_tools: Vec<String>,
}

/// A fully-wired [`Server`] plus the handles a bridge needs beside it.
pub struct BuiltServer {
    pub server: Arc<Server>,
    pub registry: Arc<session::SessionRegistry>,
    pub secret_store: security::SecretStore,
}

/// Assemble a fully-wired [`Server`] from `config` and an opened eidetica
/// `instance`/`user`. See the module docs for the invariants this preserves.
///
/// `config` is taken by `&mut` because backend / web-search / embedding API
/// keys are resolved out of it into the secret store and replaced with secret
/// refs; the caller keeps ownership and sees those edits.
pub async fn build(
    config: &mut Config,
    instance: eidetica::Instance,
    mut user: eidetica::user::User,
    opts: BuildOptions,
) -> anyhow::Result<BuiltServer> {
    // Wall-clock instrumentation for the critical-path build. Individual
    // heavy steps log their own `elapsed_ms`; this bounds the whole
    // gateway-blocking prefix so regressions are visible in the log.
    let build_start = Instant::now();

    // Enable eidetica sync for session sharing. Register iroh P2P transport
    // by default (stable peer identity, no address config needed). If
    // sync_listen is configured, also bind HTTP for traditional access.
    if opts.enable_sync {
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

    let agent_registry = std::sync::Arc::new(agent::AgentRegistry::from_config(config));
    if agent_registry.is_empty() {
        agent_registry.register_default_chaz(config)?;
    }
    info!(
        agents = agent_registry.names().len(),
        "Agent registry initialized"
    );

    // Materialize an eidetica DB per yaml-declared agent. Idempotent on
    // re-runs (yaml is a first-boot template; AgentDb is the source of
    // truth afterwards). Skipped for a standalone bridge, which owns no
    // agents and instead ticket-bootstraps the daemon's agent DBs (minting
    // here would fork a divergent copy); the hosted index below discovers
    // those already-synced DBs.
    if opts.bootstrap_agents_from_config {
        let t = Instant::now();
        let bootstrapped = agent_db::bootstrap_from_config(&mut user, config).await?;
        if !bootstrapped.is_empty() {
            info!(
                count = bootstrapped.len(),
                elapsed_ms = t.elapsed().as_millis() as u64,
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
    }

    let t = Instant::now();
    let registry = session::SessionRegistry::new(instance, user, agent_registry.clone()).await?;
    info!(
        elapsed_ms = t.elapsed().as_millis() as u64,
        "Session registry initialized"
    );
    let chaz_peer = registry.chaz_peer().clone();

    // Build the peer-local Agent and Memory Bank indices in-memory by
    // walking eidetica's tracked-DBs list. Each entry's `meta.kind` marker
    // classifies it. `/agent new`, `/memory new`, `/agent delete`, etc.
    // mutate these caches at runtime.
    let (agent_index_store, memory_bank_index_store, skill_bank_index_store) = {
        let user = registry.user_lock().await;
        hosted_index::build_from_user(&user).await?
    };

    // Surface pre-existing co-owned agents/sessions whose `home_pubkey` is
    // still unset (legacy default). These keep working as before — any
    // keyholder may run — but on co-owned agents that's the multi-peer
    // race the home-peer system exists to fix. WARN with the recovery
    // command so operators see actionable migration guidance instead of
    // silent forks.
    warn_unset_home_pubkey_on_coowned(&registry, &agent_index_store).await;

    // Attach default memory banks declared in agent configs. Idempotent —
    // already-attached banks are skipped (grant_on_memory_bank is idempotent,
    // and attach_memory_bank overwrites by name). Missing banks/agents are
    // logged at warn and skipped so a typo in config doesn't fail startup.
    if let Some(agent_configs) = &config.agents {
        for ac in agent_configs {
            if let Some(banks) = &ac.default_memory_banks {
                for bank_name in banks {
                    let Some(agent_entry) = agent_index_store.find_by_name(&ac.name) else {
                        warn!(agent = %ac.name, bank = %bank_name, "Agent not in index; skipping default bank attach");
                        continue;
                    };
                    let bank_entry = match memory_bank_index_store.find_by_name(bank_name) {
                        Some(e) => e,
                        None => {
                            // Auto-create the bank if it doesn't exist
                            let meta = memory_bank_db::MemoryBankMeta {
                                display_name: Some(bank_name.clone()),
                                description: Some(
                                    "Auto-created from default_memory_banks config".into(),
                                ),
                            };
                            match registry.create_new_memory_bank(bank_name, &meta).await {
                                Ok((bank, pubkey)) => {
                                    let entry = hosted_index::DbEntry {
                                        db_id: bank.id(),
                                        display_name: bank_name.clone(),
                                        pubkey,
                                    };
                                    memory_bank_index_store.register(entry.clone());
                                    info!(bank = %bank_name, "Auto-created default memory bank");
                                    entry
                                }
                                Err(e) => {
                                    warn!(agent = %ac.name, bank = %bank_name, error = %e, "Failed to auto-create default bank");
                                    continue;
                                }
                            }
                        }
                    };
                    let key_label = format!("memory:{}:{}", bank_name, ac.name);
                    if let Err(e) = registry
                        .grant_on_memory_bank(
                            &bank_entry.db_id,
                            &agent_entry.pubkey,
                            &key_label,
                            agent_db::BankPermission::Write,
                        )
                        .await
                    {
                        warn!(agent = %ac.name, bank = %bank_name, error = %e, "Failed to grant bank access");
                        continue;
                    }
                    match registry
                        .open_agent_db(&agent_entry.db_id, Some(&agent_entry.pubkey))
                        .await
                    {
                        Ok(Some(agent_db)) => {
                            let ref_entry = agent_db::MemoryBankRef {
                                name: bank_name.clone(),
                                db_id: bank_entry.db_id.to_string(),
                                permission: agent_db::BankPermission::Write,
                            };
                            if let Err(e) = agent_db.attach_memory_bank(ref_entry).await {
                                warn!(agent = %ac.name, bank = %bank_name, error = %e, "Failed to attach bank ref; auth already granted");
                            } else {
                                info!(agent = %ac.name, bank = %bank_name, "Attached default memory bank");
                            }
                        }
                        _ => {
                            warn!(agent = %ac.name, "Cannot open agent DB for default bank attach");
                        }
                    }
                }
            }
        }
    }
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
    let web_search_backends = build_web_search_backends(config, &secret_store).await;

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

    // Caller-supplied extras (the CLI's non-interactive allowlist under
    // `--print`, where shell/write_file have no interactive approval).
    auto_approved.extend(opts.extra_auto_approved_tools);

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

    // SpawnAgent / SpawnWorker route through the server — a single OnceLock
    // is shared; it's set once after Server::new below. The core extension
    // takes ownership of the cell and constructs the spawn tools.
    let spawn_server_cell = std::sync::Arc::new(std::sync::OnceLock::new());

    // Shared MCP server directory — populated by McpExtension::instantiate
    // below, exposed to readers (TUI Peer→MCP settings) via Server.
    let mcp_registry = Arc::new(mcp::McpRegistry::new());

    // Default backend used for schedule-fired Fresh sessions and as a
    // fallback when a Pinned session has no registered SessionRuntime.
    let default_backend = backends::BackendManager::new(&config.backends, secret_store.clone());

    // Set up extension hub infrastructure before install_all.
    // Tools and commands flow through per-extension caps;
    // install_all drains them into owner-attributed registries.
    extension_hub.set_session_registry(registry.clone());
    extension_hub.set_hosted_index(agent_index_store.clone());
    extension_hub.set_agent_state_allowlist(config.agent_state_allowlist.clone());
    extension_hub.set_peer_handles(Arc::new(extension::PeerHandles {
        registry: registry.clone(),
        agent_index: agent_index_store.clone(),
        memory_bank_index: memory_bank_index_store.clone(),
        skill_bank_index: skill_bank_index_store.clone(),
        embedder: embedder.clone(),
        secrets: Some(Arc::new(secret_store.clone())),
        server_cell: spawn_server_cell.clone(),
        mcp_registry: mcp_registry.clone(),
        agent_state_allowlist: config.agent_state_allowlist.clone(),
    }));

    // Collect MCP server configs from inline config + directory scanning.
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

    // Build the extension list: builtins + one McpExtension per MCP server.
    // MCP servers are data-driven at startup, not compile-time builtins,
    // but they participate in the same extension lifecycle — tool
    // attribution, per-session filtering, hook surface.
    let mut extensions = extensions::all_builtins(extensions::BuiltinDeps {
        agent_index: agent_index_store.clone(),
        memory_bank_index: memory_bank_index_store.clone(),
        skill_bank_index: skill_bank_index_store.clone(),
        session_registry: registry.clone(),
        embedder: embedder.clone(),
        web_search_backends,
        spawn_server_cell: spawn_server_cell.clone(),
        backend_manager: default_backend.clone(),
        security: security_ctx.clone(),
    });
    if !mcp_configs.is_empty() {
        for config in &mcp_configs {
            extensions.push(Arc::new(extensions::mcp::McpExtension::new(config.clone())));
        }
    }

    let t = Instant::now();
    extension_hub.install_all(extensions).await?;
    info!(
        elapsed_ms = t.elapsed().as_millis() as u64,
        mcp_servers = mcp_configs.len(),
        "Extensions installed (MCP servers started)"
    );
    let extension_names = extension_hub.extension_names();
    if !extension_names.is_empty() {
        info!(?extension_names, "Extensions registered");
    }

    // Build the legacy ToolRegistry from extension-contributed tools.
    // MCP tools now arrive through the same path as built-in tools
    // (McpExtension contributes them via ToolRegistration cap).
    let mut tool_registry = tool::ToolRegistry::new();
    for (owner, _name, tool) in extension_hub.tools_for_registry() {
        tool_registry.register_arc_owned(tool, Some(owner));
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

    let server = Server::new(
        registry.clone(),
        agent_registry,
        agent_index_store,
        memory_bank_index_store,
        skill_bank_index_store.clone(),
        tool_registry,
        policies,
        security_ctx,
        tool_profiles,
        context_config,
        tool_host,
        extension_hub,
        default_backend.clone(),
        mcp_registry.clone(),
        opts.run_agent_loop,
    );
    assert!(
        spawn_server_cell.set(server.clone()).is_ok(),
        "Spawn tool server cell already set"
    );

    // Runtime-ownership mode for this peer. A dumb bridge (no agent loop)
    // never claims runtime — it is pure transport. The daemon/frontend uses
    // `config.runtime` (default Auto). Set before any session registration so
    // `register_session`'s claim path sees the right mode.
    let runtime_mode = if opts.run_agent_loop {
        config.runtime.unwrap_or_default()
    } else {
        config::RuntimeMode::Never
    };
    server.set_runtime_mode(runtime_mode);
    info!(?runtime_mode, "Applied runtime-ownership mode");

    // Apply operator multi-agent tuning before the bridge starts
    // delivering messages (set_agent_burst_budget is read by
    // process_session, which only fires on the first inbound notify).
    if let Some(mc) = &config.multi_agent {
        server.set_agent_burst_budget(mc.burst_budget);
        info!(burst_budget = mc.burst_budget, "Applied multi_agent config");
    }

    // Record the config path before anything (the gateway, `/agent reload`,
    // or the deferred reconcile below) can re-read it.
    server.set_config_path(opts.config_path.clone());

    // Apply default_agents list: which agents auto-attach to new
    // sessions. First entry is the routing host. Set before any
    // session-creation path runs.
    //
    // Precedence: peer-DB override (Settings → Defaults) beats yaml so
    // runtime edits survive restart. Falling back to yaml when the DB
    // hasn't been written keeps fresh installs honouring config.
    let db_defaults = server.registry().load_peer_default_agents().await;
    if let Some(default_agents) = db_defaults {
        info!(
            agents = ?default_agents,
            "Applied default_agents (peer DB override) — these will auto-attach to new sessions"
        );
        server.set_default_agents(default_agents);
    } else if let Some(default_agents) = config.default_agents.clone() {
        info!(
            agents = ?default_agents,
            "Applied default_agents (yaml) — these will auto-attach to new sessions"
        );
        server.set_default_agents(default_agents);
    }

    // Fast-start: the server is now fully constructed and the spawn-tool
    // cell is wired, so the gateway can draw and accept input immediately.
    // The remaining at-startup work — model-window warm, agent reconcile,
    // schedule materialization, routine engine — is heavy (per-agent and
    // per-session DB opens) and does NOT need to block the first render.
    // Run it in the background; message *processing* is gated on the
    // reconcile via `mark_startup_pending`/`mark_startup_ready` so a turn
    // never executes against half-reconciled agent config.
    info!(
        critical_path_ms = build_start.elapsed().as_millis() as u64,
        "Critical-path build complete; deferring at-startup work to background"
    );
    server.mark_startup_pending();
    {
        let server = server.clone();
        let registry = registry.clone();
        let default_backend = default_backend.clone();
        let deferred_config = config.clone();
        let run_routine_engine = opts.run_routine_engine;
        let run_agent_loop = opts.run_agent_loop;
        tokio::spawn(async move {
            run_deferred_startup(
                server,
                registry,
                deferred_config,
                default_backend,
                run_routine_engine,
                run_agent_loop,
            )
            .await;
        });
    }

    Ok(BuiltServer {
        server,
        registry,
        secret_store,
    })
}

/// The deferred at-startup work, run in a background task off the
/// critical path so the gateway draws/serves immediately (see the
/// fast-start block at the end of [`build`]). Order matters:
///
/// 1. `warm_model_windows` — best-effort; the per-turn budget falls back
///    to the static default until this lands, so a miss is tolerated.
/// 2. `reconcile_agents_from_config` — resolves yaml system prompts into
///    the blob store and refreshes declarative agent fields. This is the
///    data-correctness gate: `mark_startup_ready` runs immediately after
///    so held turns release the moment agent config is consistent, before
///    the (slower) schedule + routine work below.
/// 3. Schedule materialization + routine engine — long-lived background
///    services; nothing user-facing waits on them.
///
/// Errors are logged, not propagated: the gateway is already live, so a
/// failure here degrades a feature rather than aborting the process.
async fn run_deferred_startup(
    server: Arc<Server>,
    registry: Arc<session::SessionRegistry>,
    config: Config,
    default_backend: backends::BackendManager,
    run_routine_engine: bool,
    run_agent_loop: bool,
) {
    let deferred_start = Instant::now();

    // 1. Seed the context-window overlay from the persisted in-use model
    //    store. No-op on a fresh machine until a model is used or picked.
    let t = Instant::now();
    server.warm_model_windows().await;
    info!(
        elapsed_ms = t.elapsed().as_millis() as u64,
        "Deferred: warmed model windows"
    );

    // 2. Reconcile each agent's DB config from yaml, then open the gate.
    //    `/agent reload` runs this same path on demand.
    let t = Instant::now();
    server.reconcile_agents_from_config(&config).await;
    info!(
        elapsed_ms = t.elapsed().as_millis() as u64,
        "Deferred: reconciled agents from config"
    );
    server.mark_startup_ready();
    info!(
        ready_after_ms = deferred_start.elapsed().as_millis() as u64,
        "Startup gate released — message processing enabled"
    );

    // 3a. Translate YAML `schedules:` into agent-owned Schedules. Each
    //     ScheduleConfig becomes one cron Schedule in the owning agent's
    //     DB, Pinned to the resolved session. Idempotent by schedule id ==
    //     schedule name within the owning agent.
    if let Some(schedules) = config.schedules.clone() {
        for cfg in schedules {
            if !cfg.enabled {
                info!(schedule = %cfg.name, "Schedule disabled, skipping");
                continue;
            }
            // Owning agent: explicit `agent:` else the peer's default.
            let owner_ref = cfg
                .agent
                .clone()
                .unwrap_or_else(|| server.agents().default_agent().name);
            let entry = match server.agent_index().find_by_name(&owner_ref).or_else(|| {
                eidetica::entry::ID::parse(&owner_ref)
                    .ok()
                    .and_then(|id| server.agent_index().find_by_id(&id))
            }) {
                Some(e) => e,
                None => {
                    error!(
                        schedule = %cfg.name,
                        agent = %owner_ref,
                        "Failed to resolve schedule owning agent; skipping"
                    );
                    continue;
                }
            };
            // Resolve the Pinned target session.
            let session_db_id = match registry.resolve_session(&cfg.session).await {
                Ok((_conv, sdb)) => sdb.root_id().to_string(),
                Err(e) => {
                    error!(
                        schedule = %cfg.name,
                        session = %cfg.session,
                        "Failed to resolve schedule target session: {e}"
                    );
                    continue;
                }
            };
            // Open the owning agent's DB.
            let opened = {
                let user = registry.user_lock().await;
                user.open_database(&entry.db_id).await
            };
            let adb = match opened {
                Ok(db) => agent_db::AgentDb::from_database(db),
                Err(e) => {
                    error!(
                        schedule = %cfg.name,
                        agent = %entry.display_name,
                        "Open agent DB for schedule failed: {e}"
                    );
                    continue;
                }
            };
            // Idempotent by schedule id within the owning agent.
            match adb.find_schedule(&cfg.name).await {
                Ok(Some(_)) => {
                    info!(
                        schedule = %cfg.name,
                        agent = %entry.display_name,
                        "Schedule already present on agent; leaving in place"
                    );
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    error!(schedule = %cfg.name, "find_schedule failed: {e}");
                    continue;
                }
            }
            let mut schedule = agent_db::Schedule::new(
                cfg.name.clone(),
                routine::Trigger::Cron {
                    expr: cfg.cron.clone(),
                },
                cfg.task.clone(),
                agent_db::ScheduleTarget::Pinned { session_db_id },
            );
            schedule.max_fires = cfg.max_fires;
            schedule.expires_at = cfg.expires_at;
            if let Err(e) = adb.upsert_schedule(schedule).await {
                error!(schedule = %cfg.name, "Failed to save schedule: {e}");
            } else {
                info!(
                    schedule = %cfg.name,
                    agent = %entry.display_name,
                    session = %cfg.session,
                    cron = %cfg.cron,
                    "Schedule registered as agent-owned schedule"
                );
            }
        }
    }

    // 3b. Spawn the routine engine. Loads global routines from
    //     `chaz_peer.routines`, then walks every hosted session and
    //     registers its session-scoped routines (heartbeats + scheduler
    //     fires). Skipped for one-shot CLI: a single ReAct loop doesn't
    //     need the engine running.
    if run_routine_engine {
        let chaz_peer = registry.chaz_peer().clone();
        let engine =
            match routine::RoutineEngine::new(chaz_peer, Some(server.extensions().clone())).await {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to start routine engine: {e}");
                    return;
                }
            };
        // Hand the engine to the server so each `HookContext` / `ToolContext`
        // built for a session can resync the live schedule after a committed
        // mutation (the `/schedule` command, `schedule_*` tools, `schedule_once`,
        // and `agent_delete`'s sweep).
        server.set_routine_engine(engine.clone());
        // Pick up every session's routines + ensure the server is
        // watching those sessions so directive writes from fires drive
        // an agent turn.
        let sessions = registry.list_sessions().await.unwrap_or_default();
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
        // Register every hosted agent's own schedules (Agent-Owned
        // Schedules). The agent is the unit of ownership; chaz is the
        // runtime that loads it and fires the callback. Schedules persist
        // in the agent's DB, so this picks up whatever synced/created
        // since last boot.
        for entry in server.agent_index().list() {
            let opened = {
                let user = registry.user_lock().await;
                user.open_database(&entry.db_id).await
            };
            let db = match opened {
                Ok(db) => db,
                Err(e) => {
                    error!(agent = %entry.display_name, "open agent DB for schedules failed: {e}");
                    continue;
                }
            };
            let adb = agent_db::AgentDb::from_database(db);
            if let Err(e) = engine.register_agent(&entry.db_id.to_string(), &adb).await {
                error!(agent = %entry.display_name, "engine.register_agent failed: {e}");
            }
        }

        let engine_clone = engine.clone();
        tokio::spawn(async move {
            engine_clone.run().await;
        });
    }

    // 4. Watch each hosted agent DB's session registry so the daemon runs the
    //    agent on sessions a (dumb) bridge created and exposed. Only the
    //    agent-running peer does this — a bridge sets `run_agent_loop:false`.
    if run_agent_loop {
        watch_agent_session_registries(server.clone(), registry.clone(), default_backend.clone())
            .await;
    }

    info!(
        total_ms = deferred_start.elapsed().as_millis() as u64,
        "Deferred at-startup work complete"
    );
}

/// Run the agent on sessions that a dumb bridge created and exposed.
///
/// A bridge (a separate peer) creates a session DB, attaches a daemon-owned
/// agent to it (granting the agent Write + delegating session auth to the agent
/// DB), and records a [`agent_db::SessionRef`] in the agent DB's synced session
/// registry with the bridge in `exposed_on`. That registry write syncs to the
/// daemon. This installs an `on_write` on each hosted agent DB and, on every
/// write (local *or* remote/synced — eidetica's `on_write` fires for both),
/// re-scans the registry and `register_session`s any exposed session not yet
/// watched, so the daemon's ReAct loop answers it.
///
/// `open_session` on a freshly-exposed session can transiently fail if its DB
/// hasn't synced over yet; that's fine — the next registry sync-write re-scans
/// and retries, so registration is self-healing rather than one-shot. Agents
/// created after startup aren't watched here (config-bootstrapped agents are
/// all present by now); revisit if runtime agent creation needs it.
async fn watch_agent_session_registries(
    server: Arc<Server>,
    registry: Arc<session::SessionRegistry>,
    default_backend: backends::BackendManager,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(64);

    // Install a write-watch on every hosted agent DB → ping the rescan channel.
    let mut watched_agents = 0usize;
    for entry in server.agent_index().list() {
        let Some(adb) = registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
            .ok()
            .flatten()
        else {
            warn!(agent = %entry.display_name, "Could not open agent DB for registry watch");
            continue;
        };
        let tx = tx.clone();
        match adb
            .database()
            .on_write(move |_event, _db| {
                let tx = tx.clone();
                Box::pin(async move {
                    let _ = tx.send(()).await;
                    Ok(())
                })
            })
            .await
        {
            Ok(sub) => {
                sub.detach();
                watched_agents += 1;
            }
            Err(e) => {
                warn!(agent = %entry.display_name, "agent DB on_write failed: {e}");
            }
        }
    }
    drop(tx); // only the per-agent clones keep the channel open
    info!(
        agents = watched_agents,
        "Watching agent session registries for bridge-exposed sessions"
    );

    // Initial pass: register sessions already exposed before startup.
    register_exposed_sessions(&server, &registry, &default_backend).await;

    // React to every agent-DB write (a bridge's SessionRef sync lands here),
    // debouncing a burst into one rescan.
    while rx.recv().await.is_some() {
        while rx.try_recv().is_ok() {}
        register_exposed_sessions(&server, &registry, &default_backend).await;
    }
}

/// One rescan pass: for every hosted agent, `register_session` each exposed
/// session not already watched. Idempotent — `register_session` early-returns
/// for an already-watched session, so re-scans are cheap.
pub(crate) async fn register_exposed_sessions(
    server: &Arc<Server>,
    registry: &Arc<session::SessionRegistry>,
    default_backend: &backends::BackendManager,
) {
    for entry in server.agent_index().list() {
        let Some(adb) = registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
            .ok()
            .flatten()
        else {
            continue;
        };
        let refs = adb.list_session_refs().await.unwrap_or_default();
        for r in refs {
            if r.exposed_on.is_empty() || server.is_watching_session(&r.session_db_id).await {
                continue;
            }
            // The registry entry is only a *pointer* to the session; its own tree
            // is separate and, for a bridge-created session, has no sync-peer
            // relationship with this daemon yet — so it never arrives on its own.
            // Open it; if the tree isn't local, pull it from the peers we already
            // sync this agent DB with (the exposing bridge is one of them), then
            // retry. Best-effort — a miss just defers to the next registry rescan.
            let mut opened = registry.open_session(&r.session_db_id).await;
            if opened.is_err() {
                pull_session_tree_from_agent_peers(registry, &entry, &adb, &r.session_db_id).await;
                opened = registry.open_session(&r.session_db_id).await;
            }
            match opened {
                Ok((_conv, sdb)) => {
                    // Push the agent's replies straight back to the exposing
                    // bridge on every commit, not just the 300s tick. The session
                    // arrived via sync and may not be in the daemon's tracked
                    // list, so provide the hosting agent's key explicitly.
                    if let Err(e) = registry
                        .enable_on_commit_sync_with_key(sdb.root_id(), &entry.pubkey)
                        .await
                    {
                        warn!(
                            session_db_id = %r.session_db_id,
                            "enable on-commit sync for exposed session failed: {e}"
                        );
                    }
                    // Proxy tool approvals over the session DB: the runtime
                    // blocks on this channel, the proxy writes a request entry,
                    // and the exposing bridge renders it + writes the decision.
                    let approval_tx = super::approval_proxy::spawn_session_db_approval_proxy(
                        sdb.clone(),
                        entry.display_name.clone(),
                    )
                    .await;
                    if let Err(e) = server
                        .register_session(&sdb, default_backend.clone(), None, Some(approval_tx))
                        .await
                    {
                        warn!(session_db_id = %r.session_db_id, "register exposed session failed: {e}");
                    } else {
                        info!(
                            session_db_id = %r.session_db_id,
                            agent = %entry.display_name,
                            exposed_on = ?r.exposed_on,
                            "Daemon registered bridge-exposed session"
                        );
                        // Also register in the daemon's local session catalog
                        // so the TUI `/sessions` list picks it up. Build the
                        // source from the first transport binding so the
                        // `BridgeKind::from_source` derivation is accurate.
                        let source = session::transport_bindings(&sdb)
                            .await
                            .ok()
                            .and_then(|b| b.first().map(|(t, _l, c)| format!("{t}:{c}")));
                        if let Err(e) = registry
                            .upsert_session_catalog(&r.session_db_id, source.as_deref())
                            .await
                        {
                            warn!(session_db_id = %r.session_db_id, "Failed to upsert session catalog: {e}");
                        }
                        // A bridge-exposed session that names some other peer as
                        // home will never run: the only other peer holding it is
                        // the exposing bridge, which has no agent loop. The
                        // session still syncs and registers, so it looks healthy
                        // while being permanently mute — worth a WARN naming the
                        // repair rather than a DEBUG on each skipped turn.
                        if !server
                            .peer_is_home_for(&r.session_db_id, &entry.display_name)
                            .await
                        {
                            warn!(
                                session_db_id = %r.session_db_id,
                                agent = %entry.display_name,
                                "Bridge-exposed session's home peer is not this daemon; no peer \
                                 will run its turns. Repair with `/agent rehost` on that session."
                            );
                        }
                    }
                }
                Err(e) => {
                    // Still not openable after a pull attempt (peer unreachable,
                    // or the tree genuinely absent) — a later registry sync-write
                    // re-triggers this scan and retries.
                    tracing::debug!(
                        session_db_id = %r.session_db_id,
                        "Exposed session not openable even after pull; will retry on next registry write: {e}"
                    );
                }
            }
        }
    }
}

/// Pull a bridge-exposed session's own tree onto this daemon.
///
/// The agent-DB registry only carries a *pointer* (the session root id); the
/// session's data lives in a separate eidetica tree that a dumb bridge created
/// and never established a sync-peer relationship for with the daemon. So the
/// daemon can see the session exists (the agent DB syncs) but never receives its
/// contents. Pull that tree from the peers we already sync the *agent* DB with —
/// the exposing bridge is one of them — over the connection that already exists.
/// The session DB is auth-gated, so a plain sync is rejected outright
/// (`database requires authentication`). The pull presents the hosting agent's
/// credentials instead: the session delegates to the agent DB, where that key
/// holds authority, and eidetica's bootstrap path resolves the delegation. Any
/// failure just leaves the session unopenable for the next rescan to retry.
async fn pull_session_tree_from_agent_peers(
    registry: &Arc<session::SessionRegistry>,
    agent: &crate::hosted_index::DbEntry,
    adb: &crate::agent_db::AgentDb,
    session_db_id: &str,
) {
    let Some(sync) = registry.instance().sync() else {
        return;
    };
    let session_id = match eidetica::entry::ID::parse(session_db_id) {
        Ok(id) => id,
        Err(e) => {
            tracing::debug!(session_db_id, "invalid session id, cannot pull: {e}");
            return;
        }
    };
    // The session DB is auth-gated, so a plain sync is rejected. Present the
    // hosting agent's credentials: its pubkey was granted `Write(10)` on the
    // session at attach time under the name `agent:<display_name>`
    // (see `attach_agent_to_session`), so this authorizes the pull without any
    // new grant/approval. The request is signed with the agent's own key —
    // eidetica authorizes entry-serving against a *proven* key, so naming the
    // pubkey is no longer enough to be served.
    let signing_key = match registry.signing_key_for(&agent.pubkey).await {
        Ok(k) => k,
        Err(e) => {
            tracing::debug!(
                session_db_id,
                agent = %agent.display_name,
                "this peer holds no signing key for the agent; cannot pull: {e}"
            );
            return;
        }
    };
    let key_name = format!("agent:{}", agent.display_name);
    let permission = eidetica::auth::types::Permission::Write(10);
    let agent_root = adb.database().root_id();
    let peers = match sync.get_tree_peers(agent_root).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(session_db_id, "get_tree_peers failed: {e}");
            return;
        }
    };
    for peer in peers {
        // Stale peers from earlier bootstrap cycles time out here; the live
        // bridge answers. One success makes the tree openable.
        match sync
            .sync_tree_with_peer_auth(
                peer.public_key(),
                &session_id,
                Some(&signing_key),
                Some(&key_name),
                Some(permission.clone()),
                None,
            )
            .await
        {
            Ok(()) => {
                // The pull moves entries at the sync layer but establishes no
                // User-layer SigKey mapping, so `open_session` would still fail
                // "No key found for database". Track the session under the key
                // we just authenticated with — the same call that turns on
                // push-on-commit, so the agent's replies flow straight back to
                // the exposing bridge once it starts running turns.
                if let Err(e) = registry
                    .enable_on_commit_sync_with_key(&session_id, &agent.pubkey)
                    .await
                {
                    tracing::debug!(
                        session_db_id,
                        "tracking pulled session under the agent key failed: {e}"
                    );
                }
                info!(session_db_id, peer = %peer, "Pulled exposed session tree from peer");
                break;
            }
            Err(e) => {
                tracing::debug!(session_db_id, peer = %peer, "pull from peer failed: {e}");
            }
        }
    }
}

/// Startup migration WARN: any locally-hosted agent that is co-owned
/// (more than one active AuthKey) but has no `home_pubkey` set — either
/// on its meta store (agent-level) or on a session's AgentRef — is still
/// running on the legacy multi-peer "any keyholder runs" path that the
/// home-peer system exists to fix. WARN with the exact `/agent rehost`
/// command the operator should run.
///
/// Solo agents (single AuthKey) are skipped: there's only one peer that
/// can run them anyway, so the lack of `home_pubkey` causes no fork.
async fn warn_unset_home_pubkey_on_coowned(
    registry: &session::SessionRegistry,
    agent_index: &hosted_index::HostedIndex,
) {
    let agents = agent_index.list();
    let Ok(sessions) = registry.list_sessions().await else {
        return;
    };

    for entry in &agents {
        // Count active AuthKeys on the agent DB.
        let active_count = match registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
        {
            Ok(Some(adb)) => {
                let Ok(settings) = adb.database().get_settings().await else {
                    continue;
                };
                let Ok(snap) = settings.auth_snapshot().await else {
                    continue;
                };
                let Ok(all) = snap.get_all_keys() else {
                    continue;
                };
                all.values()
                    .filter(|k| matches!(k.status(), eidetica::auth::types::KeyStatus::Active))
                    .count()
            }
            _ => continue,
        };

        if active_count <= 1 {
            continue; // Solo agent, no race possible.
        }

        // Agent-level home_pubkey check.
        let agent_level_home = match registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
        {
            Ok(Some(adb)) => db_kind::read_agent_home_pubkey(adb.database()).await,
            _ => None,
        };
        if agent_level_home.is_none() {
            warn!(
                agent = %entry.display_name,
                active_keys = active_count,
                "Co-owned agent has no agent-level home_pubkey set — Fresh schedule \
                 fires may run on multiple peers. Run `/agent rehost --agent {}` \
                 from the peer that should own them.",
                entry.display_name
            );
        }

        // Per-session home_pubkey scan.
        for s in &sessions {
            let Ok((_conv, db)) = registry.open_session(&s.session_db_id).await else {
                continue;
            };
            let meta = session::read_meta_from_db(&db).await;
            if let Some(ar) = meta
                .agents
                .iter()
                .find(|a| a.db_id == entry.db_id.to_string())
                && ar.home_pubkey.is_none()
            {
                warn!(
                    session_db_id = %s.session_db_id,
                    agent = %entry.display_name,
                    "Co-owned agent has no home_pubkey on this session — multiple peers \
                     may both respond. Run `/agent rehost {}` from the peer that should \
                     own execution.",
                    entry.display_name
                );
            }
        }
    }
}

/// Resolve the configured web-search backends: extract each API key (if any)
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
