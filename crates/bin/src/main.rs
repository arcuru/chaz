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

/// Wait until the spawned eidetica service server is actually accepting
/// connections on `path`, or fail the daemon's startup.
///
/// Fail-closed: the daemon must not log that its socket is serving — or
/// enter `wait_for_shutdown` — while nothing is listening on it. The server
/// task races the readiness probe, and whichever resolves first decides:
///
/// - the task ends first: it never came up (a bind error, a panic, an early
///   exit), so its error is propagated as a startup error;
/// - the probe succeeds first: the socket accepts connections, and the task
///   — with its shutdown sender — is returned for the clean-shutdown path;
/// - the bound expires first: the server is stopped and awaited so its
///   socket file is cleaned up, then the timeout is propagated.
///
/// The sender is passed by value and returned on success so it is the *only*
/// sender while this function runs: dropping it on the timeout path is what
/// tells the server to stop, and handing it back is what lets the caller
/// stop the server on shutdown.
async fn await_service_readiness(
    path: &std::path::Path,
    task: tokio::task::JoinHandle<eidetica::Result<()>>,
    stop: tokio::sync::watch::Sender<()>,
    timeout: Duration,
    poll: Duration,
) -> anyhow::Result<(
    tokio::sync::watch::Sender<()>,
    tokio::task::JoinHandle<eidetica::Result<()>>,
)> {
    let deadline = Instant::now() + timeout;
    let mut task = Some(task);
    loop {
        // The server task ended before accepting a connection. However it
        // ended, the socket never served, so startup fails with its error.
        if let Some(t) = &task
            && t.is_finished()
        {
            let t = task.take().expect("task is present");
            match t.await {
                Ok(Ok(())) => anyhow::bail!(
                    "eidetica service server exited before accepting a connection on {}",
                    path.display()
                ),
                Ok(Err(e)) => anyhow::bail!(
                    "eidetica service server failed to start on {}: {e}",
                    path.display()
                ),
                Err(e) => anyhow::bail!(
                    "eidetica service server task failed before accepting a connection on {}: {e}",
                    path.display()
                ),
            }
        }
        if service_client::socket_is_live(path) {
            return Ok((stop, task.take().expect("task is present")));
        }
        if Instant::now() >= deadline {
            // Gave up: stop the server — dropping the last sender is its
            // shutdown signal — and await the task so its socket file is
            // gone before startup fails.
            drop(stop);
            if let Some(t) = task.take() {
                let _ = t.await;
            }
            anyhow::bail!(
                "timed out after {timeout:?} waiting for the eidetica service socket at {} to \
                 accept connections — the service server never came up",
                path.display()
            );
        }
        tokio::time::sleep(poll).await;
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
    // Both checks happen before the backend is opened, because refusing to
    // start is the whole point: a daemon that discovered it was second only
    // after opening the database would already be the second opener.
    let _daemon_claim = match &service_socket {
        Some(_) => Some(claim_daemon_role(state_dir.as_deref())?),
        None => None,
    };
    // The claim covers two chaz daemons. The socket probe covers whatever else
    // might be bound at a configured path — and eidetica's service server
    // unlinks any socket it finds rather than checking, so without this a
    // second server would silently steal a live one's.
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
                    service_client::DEFAULT_POLL_INTERVAL,
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
        await_service_readiness, claim_daemon_role, resolve_config_path, resolve_service_socket,
        resolve_state_dir,
    };
    use chaz_core::config::{Config, ServiceConfig};
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

    #[tokio::test]
    async fn unbindable_service_socket_fails_startup_instead_of_advertising_ready() {
        let dir = tempfile::tempdir().unwrap();
        // A Unix socket path past the platform's sun_path limit (108 bytes
        // on Linux) can never bind — deterministically, and with a fresh
        // tempdir nothing else can make the bind fail. This is the
        // regression: the daemon used to log "serving" and enter the
        // shutdown wait while the socket was never actually live.
        let too_long = dir
            .path()
            .join(format!("eidetica-{}.sock", "x".repeat(120)));
        assert!(
            too_long.as_os_str().len() > 108,
            "test path must exceed the Unix socket path limit"
        );

        // A real instance and a real server, so the failing bind is the
        // real one rather than a mocked task.
        let (instance, _admin) = eidetica::Instance::create_backend(
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
        let err = match await_service_readiness(
            &too_long,
            task,
            stop,
            timeout,
            Duration::from_millis(10),
        )
        .await
        {
            Ok(_) => panic!("an unbindable socket must fail startup, not advertise ready"),
            Err(e) => e,
        };

        assert!(
            started.elapsed() < timeout,
            "startup must fail promptly rather than wait out the readiness bound: {:?}",
            started.elapsed()
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("failed to start"),
            "the bind failure must be the surfaced error: {msg}"
        );
        assert!(
            !msg.contains("timed out"),
            "the bind error must surface, not the readiness timeout: {msg}"
        );
        assert!(
            !too_long.exists(),
            "a failed start must not leave a socket file behind"
        );
    }

    #[tokio::test]
    async fn ready_service_socket_keeps_task_and_sender_for_clean_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("eidetica.sock");

        let (instance, _admin) = eidetica::Instance::create_backend(
            Box::new(eidetica::backend::database::InMemory::new()),
            eidetica::NewUser::passwordless("chaz"),
        )
        .await
        .unwrap();
        let (stop, stopped) = tokio::sync::watch::channel(());
        let server = eidetica::service::ServiceServer::new(instance, socket.clone());
        let task = tokio::spawn(async move { server.run(stopped).await });

        // Readiness succeeds: the socket accepts connections, and the task
        // and its shutdown sender come back for the clean-shutdown path.
        let (stop, task) = await_service_readiness(
            &socket,
            task,
            stop,
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await
        .expect("a bindable socket must come up");
        assert!(super::service_client::socket_is_live(&socket));

        // The returned sender still stops the server, and the returned task
        // completes and removes the socket file — exactly the shutdown
        // sequence the daemon's shutdown path runs.
        drop(stop);
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("service server error on shutdown: {e}"),
            Err(e) => panic!("service server task failed on shutdown: {e}"),
        }
        assert!(!socket.exists(), "shutdown must remove the socket file");
    }
}
