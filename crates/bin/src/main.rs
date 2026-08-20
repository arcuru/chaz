mod bridge;
// The client half of service mode. The daemon holds the lifetime claim from
// here; the auto-start machinery is built and tested ahead of the frontends
// that will consume it, which is why parts of it are not called yet.
#[allow(dead_code)]
mod service_client;

use chaz_core::bridge::Bridge;
use chaz_core::config::Config;
use chaz_core::{agent, config, server, session};

use anyhow::Context;
use clap::Parser;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};
use std::{fs::File, io::Read, path::PathBuf};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct ChazArgs {
    /// Print the response and exit — non-interactive one-shot. By default
    /// each invocation creates a fresh ephemeral session; pass --session
    /// NAME to reuse one. Without --print, chaz launches the TUI.
    #[arg(short = 'p', long = "print")]
    print: bool,

    /// Path to config file. When unset, falls back to
    /// `$XDG_CONFIG_HOME/chaz/config.yaml` (typically
    /// `~/.config/chaz/config.yaml`).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Named session to reuse with --print (find-or-create). When omitted,
    /// --print creates a fresh session per invocation.
    #[arg(long, requires = "print", value_name = "NAME")]
    session: Option<String>,

    /// Initial prompt. With --print, sent as the one-shot message
    /// (required). Without --print, pre-fills the TUI input box on launch.
    #[arg(required_if_eq("print", "true"))]
    prompt: Option<String>,

    #[command(subcommand)]
    subcommand: Option<Subcommand>,
}

#[derive(clap::Subcommand)]
enum Subcommand {
    /// Aggregate LLM usage and cost across all sessions, then exit.
    /// Reads the user-central session catalog; no bridge is started.
    Usage(UsageArgs),

    /// Run one `/command` non-interactively, print its result, and exit.
    ///
    /// Reaches the same command grammar as the TUI and the Matrix bridge, so
    /// peer administration is scriptable — notably the bridge bring-up
    /// sequence (`/pubkey`, `/agent invite`, `/agent share`, `/sharing
    /// approve`), which otherwise needs a human at a terminal.
    ///
    /// Exits non-zero when the command reports an error. Run it with the
    /// daemon stopped: it opens the same state directory, and two processes
    /// on one backend do not observe each other's writes.
    Cmd(CmdArgs),

    /// Run the agent peer with no user interface, until terminated.
    ///
    /// Same runtime as the TUI — sync, schedules, the routine engine, and the
    /// agent loop — minus the terminal. This is the process transport bridges
    /// (`chaz-matrix`, `chaz-discord`) sync against, and the form to run under
    /// systemd, a container, or a test harness, none of which can offer the
    /// TTY the TUI's raw mode requires.
    ///
    /// With `service.enabled` in config, also serves this peer's eidetica
    /// Instance on `<state_dir>/eidetica.sock` and refuses to start if another
    /// daemon is already serving there.
    ///
    /// Logs to stdout. Stops cleanly on Ctrl-C or SIGTERM.
    Daemon,
}

#[derive(clap::Args)]
struct CmdArgs {
    /// The command to run, including its leading `/` — e.g. '/sharing requests'.
    #[arg(value_name = "COMMAND")]
    command: String,

    /// Named session to run the command against (find-or-create). Peer-scoped
    /// commands ignore it; session-scoped ones (`/info`, `/share`) need it to
    /// address anything but a fresh throwaway session.
    #[arg(long, value_name = "NAME")]
    session: Option<String>,
}

#[derive(clap::Args)]
struct UsageArgs {
    /// Emit the rollup as JSON for machine consumption.
    #[arg(long)]
    json: bool,

    /// Only include sessions originating from this bridge (cli, tui,
    /// matrix, spawn, other). Flag name kept as `--gateway` to preserve the
    /// existing CLI contract.
    #[arg(long = "gateway", value_name = "KIND")]
    bridge: Option<String>,

    /// Skip sessions marked closed.
    #[arg(long)]
    active_only: bool,
}

/// Resolve the config path: explicit `--config` wins; otherwise fall back to
/// `$XDG_CONFIG_HOME/chaz/config.yaml` (typically `~/.config/chaz/config.yaml`).
/// Errors with a helpful message when neither is available.
fn resolve_config_path(explicit: Option<&std::path::Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let default = dirs::config_dir()
        .map(|d| d.join("chaz").join("config.yaml"))
        .ok_or_else(|| anyhow::anyhow!("could not determine user config directory"))?;
    if default.exists() {
        Ok(default)
    } else {
        anyhow::bail!(
            "no --config provided and no default config at {}\n\
             create that file or pass --config <path>",
            default.display()
        )
    }
}

/// Resolve the configured state directory, expanding a leading `~` when set.
/// Falls back to the platform XDG state directory when it is absent.
fn resolve_state_dir(config: &Config) -> Option<PathBuf> {
    config
        .state_dir
        .as_ref()
        .map(|path| agent::expand_home(std::path::Path::new(path)))
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz")))
}

/// Where the daemon serves its eidetica Instance, or `None` when the service
/// socket is switched off.
///
/// The default sits beside the database it fronts rather than at eidetica's
/// per-user default path: one user runs several chaz peers — a daemon and one
/// or more transport bridges — and each owns a separate backend, so a per-user
/// singleton socket would front the wrong one.
fn resolve_service_socket(config: &Config, state_dir: Option<&std::path::Path>) -> Option<PathBuf> {
    let service = config.service.as_ref()?;
    if !service.enabled {
        return None;
    }
    Some(match &service.path {
        Some(path) => agent::expand_home(std::path::Path::new(path)),
        None => state_dir
            .map(|d| d.join("eidetica.sock"))
            .unwrap_or_else(|| PathBuf::from("eidetica.sock")),
    })
}

/// Take the claim that says "I am the daemon for this state directory", to be
/// held for the daemon's whole life.
///
/// This is the sole-opener rule's enforcement. It is an atomic `flock` rather
/// than a look-then-act check, so two daemons starting at the same instant
/// cannot both conclude they are the first; and the kernel drops it when the
/// holder dies, so a crashed daemon leaves nothing to clean up.
///
/// Returns the open file — dropping it releases the claim, so the caller must
/// hold it.
fn claim_daemon_role(state_dir: Option<&std::path::Path>) -> anyhow::Result<std::fs::File> {
    let path = state_dir
        .map(|d| d.join(service_client::DAEMON_LOCK_FILE))
        .unwrap_or_else(|| PathBuf::from(service_client::DAEMON_LOCK_FILE));
    let file = std::fs::File::create(&path)
        .with_context(|| format!("could not open the daemon claim at {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "another chaz daemon already holds {} — refusing to start a second \
             opener of the same eidetica backend",
            path.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow::Error::new(e).context(format!(
            "could not take the daemon claim at {}",
            path.display()
        ))),
    }
}

/// Where the advisory claim for a service socket lives: the socket path with
/// `.lock` appended. Kept beside the socket — not in the state directory — so
/// every daemon that names the same socket contends on the same file, even
/// when their state directories differ.
fn service_socket_lock_path(path: &std::path::Path) -> PathBuf {
    let mut lock = PathBuf::from(path);
    lock.as_mut_os_string().push(".lock");
    lock
}

/// Take the claim that says "I am the daemon serving this socket path", to be
/// held for the daemon's whole life.
///
/// Keyed to the socket path itself, not the state directory: eidetica's
/// `ServiceServer` unlinks any socket it finds rather than checking, so two
/// daemons that name the same `service.path` from different state directories
/// would otherwise each pass the liveness probe before the other binds and
/// then silently steal each other's socket. An atomic `flock` on a file
/// adjacent to the socket makes the acquisition itself the arbitration, with
/// the kernel dropping the claim when the holder dies.
///
/// Returns the open file — dropping it releases the claim, so the caller must
/// hold it.
fn claim_service_socket(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
    let lock_path = service_socket_lock_path(path);
    let file = std::fs::File::create(&lock_path).with_context(|| {
        format!(
            "could not open the service socket claim at {}",
            lock_path.display()
        )
    })?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "another daemon already holds the service socket claim at {} — \
             refusing to start a second opener of {}",
            lock_path.display(),
            path.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow::Error::new(e).context(format!(
            "could not take the service socket claim at {}",
            lock_path.display()
        ))),
    }
}

/// The socket is ready only once it accepts a connection *and* is mode 0600.
/// `ServiceServer` binds before it chmods, so connect alone would let a
/// server that fails its chmod pass as ready in the gap before it exits.
fn socket_is_ready(path: &std::path::Path) -> bool {
    let mode_0600 = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777 == 0o600)
        .unwrap_or(false);
    mode_0600 && service_client::socket_is_live(path)
}

/// Fail startup unless the spawned service server actually comes up on `path`.
///
/// The server task races a bounded readiness probe under `tokio::select!`,
/// biased so a finished task wins a tie — a server that just died must never
/// be reported ready. On timeout the only stop sender is dropped (the server's
/// shutdown signal) and the task awaited so its socket file is gone before
/// startup fails. On success the sender and still-running task are returned
/// for the existing shutdown path.
async fn await_service_readiness(
    path: &std::path::Path,
    task: tokio::task::JoinHandle<eidetica::Result<()>>,
    stop: tokio::sync::watch::Sender<()>,
    timeout: Duration,
) -> anyhow::Result<(
    tokio::sync::watch::Sender<()>,
    tokio::task::JoinHandle<eidetica::Result<()>>,
)> {
    let ready = async {
        let deadline = Instant::now() + timeout;
        loop {
            if socket_is_ready(path) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out after {timeout:?} waiting for the eidetica service socket at {} \
                     to accept connections with mode 0600",
                    path.display()
                );
            }
            tokio::time::sleep(service_client::DEFAULT_POLL_INTERVAL).await;
        }
    };

    let mut task = task;
    tokio::select! {
        biased;
        result = &mut task => match result {
            Ok(Ok(())) => anyhow::bail!(
                "eidetica service server exited before serving {}",
                path.display()
            ),
            Ok(Err(e)) => anyhow::bail!(
                "eidetica service server failed to start on {}: {e}",
                path.display()
            ),
            Err(e) => anyhow::bail!(
                "eidetica service server task failed before serving {}: {e}",
                path.display()
            ),
        },
        result = ready => match result {
            Ok(()) => Ok((stop, task)),
            Err(e) => {
                drop(stop);
                let _ = task.await;
                Err(e)
            }
        },
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = ChazArgs::parse();

    let config_path = resolve_config_path(args.config.as_deref())?;

    let mut file = File::open(&config_path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let mut config: Config = serde_yaml::from_str(&contents)?;

    // Warn about unrecognised YAML keys before proceeding.
    let unknown = config::check_unknown_config_keys(&contents);
    if !unknown.is_empty() {
        warn!(
            config = %config_path.display(),
            count = unknown.len(),
            keys = %unknown.join(", "),
            "Unrecognised config key(s)"
        );
    }

    // Resolve state directory for persistence
    let state_dir = resolve_state_dir(&config);
    if let Some(dir) = &state_dir {
        std::fs::create_dir_all(dir)?;
    }

    // Subcommand routing. `usage` is a read-only utility: it opens the DB,
    // does its work, and exits without a bridge, scheduler, MCP, or sync.
    // `cmd` needs the fully-wired server, so it falls through and is dispatched
    // as a bridge below.
    let mut cmd_args: Option<CmdArgs> = None;
    let mut daemon_mode = false;
    if let Some(sub) = args.subcommand.take() {
        match sub {
            Subcommand::Daemon => daemon_mode = true,
            Subcommand::Usage(usage_args) => {
                // Bare stderr logging — stdout is reserved for the subcommand's
                // own output (text or JSON) so it stays pipe-friendly.
                let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(std::io::stderr)
                    .init();
                return run_usage_subcommand(usage_args, &config, state_dir.as_deref()).await;
            }
            Subcommand::Cmd(a) => cmd_args = Some(a),
        }
    }

    // Both one-shot modes reserve stdout for their result, so neither can log
    // to it.
    let headless_oneshot = args.print || cmd_args.is_some();

    // Init tracing. Honour RUST_LOG; default to info when unset.
    //
    // - daemon: logs go to stdout, where systemd / docker / a test harness
    //   collect them via their usual mechanisms.
    // - TUI (default): stdout belongs to ratatui, so logs go to a rolling file
    //   (the alt-screen buffer gets corrupted by stray writes).
    // - --print / cmd: stdout is reserved for the model's reply (or the
    //   command's result) so it can be piped / captured cleanly. Logs go to a
    //   rolling file mirroring the TUI path.
    //
    // File-mode rotations: daily, keep the last 7 days. Tail the file in
    // another terminal to follow live.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _file_log_guard = if daemon_mode {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stdout)
            .init();
        None
    } else {
        let log_dir = state_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        let prefix = match (args.print, cmd_args.is_some()) {
            (_, true) => "chaz-cmd",
            (true, _) => "chaz-cli",
            _ => "chaz-tui",
        };
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
            "chaz logs: {}/{}.log (daily, keeps 7 days)",
            log_dir.display(),
            prefix,
        );
        Some(guard)
    };

    info!(
        config = %config_path.display(),
        print = args.print,
        cmd = cmd_args.is_some(),
        daemon = daemon_mode,
        "Starting chaz"
    );
    info!("Config loaded from {}", config_path.display());

    // Whole-startup wall clock: time from here to the gateway taking over.
    let startup_start = Instant::now();

    // The eidetica service socket, when the daemon is asked to serve one.
    // Only the daemon serves: every other mode is a frontend, and step one of
    // the migration gives them nothing to connect to yet.
    let service_socket = if daemon_mode {
        resolve_service_socket(&config, state_dir.as_deref())
    } else {
        None
    };
    //
    // All three checks happen before the backend is opened, because refusing
    // to start is the whole point: a daemon that discovered it was second only
    // after opening the database would already be the second opener.
    //
    // The daemon claim covers two chaz daemons on one state directory. The
    // socket claim covers two daemons on one socket path regardless of state
    // directory — eidetica's service server unlinks any socket it finds, so
    // without an atomic claim two daemons naming the same path would each pass
    // the probe below before the other binds and then steal each other's
    // socket. The probe covers whatever else is bound at the path that never
    // took the claim.
    let _daemon_claim = match &service_socket {
        Some(_) => Some(claim_daemon_role(state_dir.as_deref())?),
        None => None,
    };
    let _socket_claim = match &service_socket {
        Some(socket) => Some(claim_service_socket(socket)?),
        None => None,
    };
    if let Some(socket) = &service_socket
        && service_client::socket_is_live(socket)
    {
        anyhow::bail!(
            "something is already serving {} — refusing to start a second \
             opener of the same eidetica backend",
            socket.display()
        );
    }

    // Initialize eidetica with SQLite backend for persistent storage
    let eidetica_db_path = state_dir
        .as_ref()
        .map(|d| d.join("eidetica.db"))
        .unwrap_or_else(|| PathBuf::from("eidetica.db"));
    let t = Instant::now();
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
    info!(
        elapsed_ms = t.elapsed().as_millis() as u64,
        "eidetica opened"
    );

    // Clone before `server::build` takes ownership. The service server serves
    // the very Instance the daemon runs on — that is what makes a connected
    // client see the daemon's writes instead of racing them.
    let service_instance = service_socket.as_ref().map(|_| instance.clone());

    // In non-interactive --print mode there is no approval UI; pass the
    // configured (or default) CLI auto-approved tools so shell/write_file work
    // in the one-shot loop. Long-lived modes leave the set empty (interactive
    // approval governs).
    let extra_auto_approved_tools = if args.print {
        config
            .cli
            .as_ref()
            .map(|c| c.auto_approved_tools.clone())
            .unwrap_or_else(config::default_cli_auto_approved)
    } else {
        Vec::new()
    };

    // Assemble the fully-wired server (registry, agent DBs, secret store,
    // extension hub, schedules, routine engine) from the opened eidetica
    // instance. Sync and the routine engine are long-lived and skipped for a
    // one-shot CLI run. See `chaz_core::server::build`.
    let t = Instant::now();
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
            // Command mode needs sync even though it is one-shot: `/agent
            // share` mints a ticket out of the sync layer and refuses outright
            // without it, and minting tickets is most of the point.
            enable_sync: cmd_args.is_some() || !args.print,
            run_routine_engine: !headless_oneshot,
            // The chaz daemon owns its agents — mint their DBs from config.
            bootstrap_agents_from_config: true,
            // Command mode administers the peer; it never runs a turn, and
            // starting the loop would risk billing one as a side effect.
            run_agent_loop: cmd_args.is_none(),
            extra_auto_approved_tools,
            // `--print` runs exactly one turn, so its tool list has to be
            // complete before that turn starts. Long-lived modes take the
            // tools whenever they land. `chaz cmd` runs no turn at all and
            // so never reaches the gate either way.
            mcp_readiness: if args.print {
                server::McpReadiness::AwaitReady
            } else {
                server::McpReadiness::Deferred
            },
        },
    )
    .await?;
    info!(
        build_ms = t.elapsed().as_millis() as u64,
        time_to_gateway_ms = startup_start.elapsed().as_millis() as u64,
        "Server built; handing off to gateway"
    );

    // Bridge dispatch.
    //
    // - `cmd`     : one-shot slash command
    // - `--print` : one-shot CLI
    // - default   : TUI
    //
    // Transport bridges (Matrix, Discord) are their own standalone peer
    // binaries (`chaz-matrix`, `chaz-discord`) — this process no longer spawns
    // any in-process.
    let mode = match (args.print, cmd_args.is_some(), daemon_mode) {
        (_, _, true) => "daemon",
        (_, true, _) => "cmd",
        (true, ..) => "cli",
        _ => "tui",
    };
    info!(mode, "Starting bridge");

    let result = if daemon_mode {
        // Serve the backend to other local processes, when configured to.
        let service = match (service_socket, service_instance) {
            (Some(path), Some(instance)) => {
                let (stop, stopped) = tokio::sync::watch::channel(());
                let server = eidetica::service::ServiceServer::new(instance, path.clone());
                let task = tokio::spawn(async move { server.run(stopped).await });
                // Fail closed: do not log "serving" — and do not enter the
                // shutdown wait below — until the socket has actually
                // accepted a connection. A server that failed to bind must
                // fail the daemon's startup, not leave it idling with a
                // socket nobody can reach.
                let (stop, task) = await_service_readiness(
                    &path,
                    task,
                    stop,
                    service_client::DEFAULT_READINESS_TIMEOUT,
                )
                .await?;
                info!(socket = %path.display(), "eidetica service socket serving");
                Some((stop, task))
            }
            _ => None,
        };

        // No bridge: the server is already running sync, schedules, the
        // routine engine, and the agent loop. Hold the process open so that
        // work continues, and let the runtime shut down through the same
        // `Drop` path any other mode uses.
        info!("chaz daemon ready; waiting for shutdown signal");
        wait_for_shutdown().await;
        info!("Shutdown signal received; stopping");

        // Dropping the sender is the service server's stop signal. Await the
        // task so the socket file is gone before the process is: a socket
        // outliving its daemon is the crash leftover a later start has to
        // reason about.
        if let Some((stop, task)) = service {
            drop(stop);
            match task.await {
                Ok(Ok(())) => info!("eidetica service socket closed"),
                Ok(Err(e)) => error!("eidetica service server error: {e}"),
                Err(e) => error!("eidetica service server task failed: {e}"),
            }
        }
        Ok(())
    } else if let Some(a) = cmd_args {
        let bridge = bridge::cmd::CommandBridge::new(config, secret_store, a.command, a.session);
        bridge.run(server).await
    } else if args.print {
        // One-shot: no background bridges, no shutdown plumbing needed.
        let prompt = args.prompt.clone().expect("--print requires PROMPT");
        let bridge = bridge::cli::CliBridge::new(config, secret_store, prompt, args.session);
        bridge.run(server).await
    } else {
        let mut tui_bridge = bridge::tui::TuiBridge::new(config, secret_store);
        if let Some(prompt) = args.prompt {
            tui_bridge = tui_bridge.with_initial_prompt(prompt);
        }
        tui_bridge.run(server).await
    };

    // Propagate rather than swallow: the exit code is the only signal a calling
    // script gets, and a bridge that failed did not do the work it was asked to.
    if let Err(e) = result {
        error!("Bridge error: {e}");
        return Err(e);
    }

    Ok(())
}

/// Block until the process is asked to stop. Ctrl-C covers foreground and
/// container use; SIGTERM is what systemd and a test harness's teardown send,
/// and without it a stop degrades into the kill that follows the timeout.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to install SIGTERM handler, Ctrl-C only: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `chaz usage` — open the eidetica DB read-only, walk the user-central
/// session catalog, aggregate per-message `ResponseMetadata`, print either
/// human-readable text or JSON, then exit. Skips all bridge/sync/scheduler
/// setup since we never serve a session here.
async fn run_usage_subcommand(
    args: UsageArgs,
    config: &Config,
    state_dir: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let bridge_filter = match args.bridge.as_deref() {
        Some(s) => Some(session::BridgeKind::from_filter_str(s).ok_or_else(|| {
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
    let (instance, maybe_user) = eidetica::Instance::connect_or_create_backend(
        Box::new(backend),
        eidetica::NewUser::passwordless("chaz"),
    )
    .await?;
    let user = match maybe_user {
        Some(u) => u,
        None => instance.login_user("chaz", None).await?,
    };

    let agent_registry = std::sync::Arc::new(agent::AgentRegistry::from_config(config));
    if agent_registry.is_empty() {
        agent_registry.register_default_chaz(config)?;
    }
    let registry = session::SessionRegistry::new(instance, user, agent_registry).await?;

    let filter = session::usage::UsageFilter {
        since: None,
        bridge: bridge_filter,
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

#[cfg(test)]
mod tests {
    use super::{
        await_service_readiness, claim_daemon_role, claim_service_socket, resolve_config_path,
        resolve_service_socket, resolve_state_dir,
    };
    use chaz_core::config::{Config, ServiceConfig};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[test]
    fn explicit_config_arg_wins() {
        let p = PathBuf::from("/tmp/whatever.yaml");
        let resolved = resolve_config_path(Some(&p)).unwrap();
        assert_eq!(resolved, p);
    }

    #[test]
    fn missing_default_errors_with_path_hint() {
        // Point XDG_CONFIG_HOME at a tmp dir with no chaz/config.yaml so the
        // fallback misses and we get the structured error.
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test process; `dirs::config_dir` reads
        // XDG_CONFIG_HOME without caching.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let err = resolve_config_path(None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("config.yaml") && msg.contains("--config"),
            "unhelpful error: {msg}"
        );
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn resolve_state_dir_expands_tilde_and_preserves_other_paths() {
        let home = dirs::home_dir().expect("home dir in test env");
        let config = Config {
            state_dir: Some("~/.local/state/chaz".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_state_dir(&config),
            Some(home.join(".local/state/chaz"))
        );

        let config = Config {
            state_dir: Some("relative/state".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_state_dir(&config),
            Some(PathBuf::from("relative/state"))
        );

        let config = Config::default();
        assert_eq!(
            resolve_state_dir(&config),
            dirs::state_dir().map(|dir| dir.join("chaz"))
        );
    }

    #[test]
    fn service_socket_is_off_unless_asked_for() {
        let state = PathBuf::from("/var/lib/chaz");

        // No block at all, and an explicitly disabled block, both mean off —
        // including one that names a path, which is a path to nowhere until
        // someone flips `enabled`.
        assert_eq!(
            resolve_service_socket(&Config::default(), Some(&state)),
            None
        );
        let config = Config {
            service: Some(ServiceConfig {
                enabled: false,
                path: Some("/run/chaz.sock".into()),
            }),
            ..Default::default()
        };
        assert_eq!(resolve_service_socket(&config, Some(&state)), None);
    }

    #[test]
    fn service_socket_defaults_beside_the_database() {
        let state = PathBuf::from("/var/lib/chaz");
        let config = Config {
            service: Some(ServiceConfig {
                enabled: true,
                path: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            resolve_service_socket(&config, Some(&state)),
            Some(state.join("eidetica.sock"))
        );

        // No state directory: the socket lands beside the database, which in
        // that case is the working directory.
        assert_eq!(
            resolve_service_socket(&config, None),
            Some(PathBuf::from("eidetica.sock"))
        );
    }

    #[test]
    fn service_socket_override_expands_tilde() {
        let home = dirs::home_dir().expect("home dir in test env");
        let config = Config {
            service: Some(ServiceConfig {
                enabled: true,
                path: Some("~/run/chaz.sock".into()),
            }),
            ..Default::default()
        };
        assert_eq!(
            resolve_service_socket(&config, Some(std::path::Path::new("/var/lib/chaz"))),
            Some(home.join("run/chaz.sock"))
        );
    }

    #[test]
    fn a_second_daemon_cannot_take_the_claim() {
        let tmp = tempfile::tempdir().unwrap();

        let first = claim_daemon_role(Some(tmp.path())).expect("first daemon takes the claim");
        let err = claim_daemon_role(Some(tmp.path()))
            .expect_err("a second daemon must not open the same backend");
        assert!(
            format!("{err}").contains("refusing to start a second opener"),
            "unhelpful error: {err}"
        );

        // The claim is the daemon's lifetime, so releasing it lets the next
        // daemon start — which is what makes a restart work.
        drop(first);
        claim_daemon_role(Some(tmp.path())).expect("the claim is free once the holder is gone");
    }

    #[test]
    fn socket_claim_serializes_two_state_dirs_on_one_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let state_a = tmp.path().join("state-a");
        let state_b = tmp.path().join("state-b");
        std::fs::create_dir_all(&state_a).unwrap();
        std::fs::create_dir_all(&state_b).unwrap();
        let socket = tmp.path().join("shared.sock");

        // Different state directories: the state-dir claim is per-directory,
        // so both daemons hold theirs at once — it cannot be what serializes
        // access to a socket the two of them name in common.
        let daemon_a = claim_daemon_role(Some(&state_a)).expect("state A daemon role");
        let daemon_b = claim_daemon_role(Some(&state_b)).expect("state B daemon role");

        // Race one contender per state directory for the socket claim. Because
        // the claim is keyed to the socket path, exactly one may hold it.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..2 {
            let barrier = barrier.clone();
            let tx = tx.clone();
            let socket = socket.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let _ = tx.send(claim_service_socket(&socket).map_err(|e| e.to_string()));
            });
        }
        drop(tx);

        let mut wins = 0;
        let mut winner = None;
        for result in rx {
            match result {
                Ok(file) => {
                    wins += 1;
                    winner = Some(file);
                }
                Err(e) => assert!(
                    e.contains("refusing to start a second opener"),
                    "unhelpful contention error: {e}"
                ),
            }
        }
        assert_eq!(wins, 1, "exactly one daemon may hold the socket claim");

        // The claim is the daemon's lifetime; dropping the winner frees the
        // socket for the next daemon to start.
        drop(winner);
        claim_service_socket(&socket).expect("the socket claim is free once the holder is gone");

        drop(daemon_a);
        drop(daemon_b);
    }

    #[test]
    fn socket_is_ready_requires_0600_even_while_live() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("eidetica.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        // A server that bound but has not chmodded yet: live, but the mode
        // half of readiness is missing, so it must not count as ready.
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !super::socket_is_ready(&socket),
            "a live socket with mode 0755 must not be ready"
        );

        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            super::socket_is_ready(&socket),
            "a live socket with mode 0600 must be ready"
        );
    }

    #[tokio::test]
    async fn unbindable_socket_fails_startup_with_the_bind_error() {
        let dir = tempfile::tempdir().unwrap();
        // Past the platform's sun_path limit (108 bytes): bind always fails.
        let too_long = dir
            .path()
            .join(format!("eidetica-{}.sock", "x".repeat(120)));
        assert!(too_long.as_os_str().len() > 108);

        let (instance, _) = eidetica::Instance::create_backend(
            Box::new(eidetica::backend::database::InMemory::new()),
            eidetica::NewUser::passwordless("chaz"),
        )
        .await
        .unwrap();
        let (stop, stopped) = tokio::sync::watch::channel(());
        let server = eidetica::service::ServiceServer::new(instance, too_long.clone());
        let task = tokio::spawn(async move { server.run(stopped).await });

        let timeout = Duration::from_secs(10);
        let started = Instant::now();
        let err = await_service_readiness(&too_long, task, stop, timeout)
            .await
            .expect_err("an unbindable socket must fail startup");
        assert!(
            started.elapsed() < timeout,
            "must fail promptly, not wait out the readiness bound"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("failed to start") && !msg.contains("timed out"),
            "the bind error must surface, not a timeout: {msg}"
        );
        assert!(
            !too_long.exists(),
            "a failed start must not leave a socket file"
        );
    }

    #[tokio::test]
    async fn ready_socket_returns_task_and_sender_for_clean_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("eidetica.sock");

        let (instance, _) = eidetica::Instance::create_backend(
            Box::new(eidetica::backend::database::InMemory::new()),
            eidetica::NewUser::passwordless("chaz"),
        )
        .await
        .unwrap();
        let (stop, stopped) = tokio::sync::watch::channel(());
        let server = eidetica::service::ServiceServer::new(instance, socket.clone());
        let task = tokio::spawn(async move { server.run(stopped).await });

        let (stop, task) = await_service_readiness(&socket, task, stop, Duration::from_secs(5))
            .await
            .expect("a bindable socket must come up");
        assert!(
            super::socket_is_ready(&socket),
            "socket must be live and 0600"
        );

        // The returned sender stops the server, whose task then removes the
        // socket file — the daemon's shutdown sequence.
        drop(stop);
        task.await
            .expect("service server task panicked")
            .expect("clean shutdown");
        assert!(!socket.exists(), "shutdown must remove the socket file");
    }
}
