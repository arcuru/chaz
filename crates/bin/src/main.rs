mod bridge;

use chaz_core::bridge::Bridge;
use chaz_core::config::Config;
use chaz_core::{agent, config, server, session};

use clap::Parser;
use std::time::Instant;
use std::{fs::File, io::Read, path::PathBuf};
use tracing::{error, info};

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = ChazArgs::parse();

    let config_path = resolve_config_path(args.config.as_deref())?;

    let mut file = File::open(&config_path)?;
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
        // No bridge: the server is already running sync, schedules, the
        // routine engine, and the agent loop. Hold the process open so that
        // work continues, and let the runtime shut down through the same
        // `Drop` path any other mode uses.
        info!("chaz daemon ready; waiting for shutdown signal");
        wait_for_shutdown().await;
        info!("Shutdown signal received; stopping");
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
    use super::resolve_config_path;
    use std::path::PathBuf;

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
}
