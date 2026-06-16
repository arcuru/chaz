mod bridge;

use chaz_core::bridge::Bridge;
use chaz_core::config::Config;
use chaz_core::{agent, config, server, session};

use clap::Parser;
use std::sync::Arc;
use std::time::Instant;
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

    /// Run headless: skip the TUI, run only background bridges (Matrix).
    /// Requires Matrix to be configured. Stdout receives logs in this mode
    /// (no TUI to grab it).
    #[arg(long, conflicts_with = "print")]
    no_tui: bool,

    /// Don't spawn the Matrix bridge in the background, even when Matrix is
    /// configured. Useful for local TUI sessions where you don't want
    /// rooms answered. Ignored under `--print`.
    #[arg(long, conflicts_with_all = ["print", "no_tui"])]
    no_matrix: bool,

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

/// Filesystem-safe component for a login's per-login state directory.
/// Matrix MXIDs (`@user:server`) contain `@` and `:` — the latter invalid
/// in path components on some filesystems — so map anything outside
/// `[A-Za-z0-9._-]` to `_`. Collisions are acceptable: `login_id` is the
/// routing key (kept verbatim in channel bindings), this is only its dir name.
fn sanitize_login_id(login_id: &str) -> String {
    login_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = ChazArgs::parse();

    if args.no_tui && args.prompt.is_some() {
        anyhow::bail!(
            "--no-tui does not accept a positional prompt — background bridges receive \
             input from their transport, not the CLI"
        );
    }

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

    // Subcommand short-circuit: read-only utilities open the DB, do their
    // work, and exit — no bridge, scheduler, MCP, or sync setup.
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
    // - --no-tui (headless): logs go to stdout, where systemd / docker / etc.
    //   collect them via their usual mechanisms.
    // - TUI (default, including TUI + background Matrix): stdout belongs to
    //   ratatui, so logs go to a rolling file (the alt-screen buffer gets
    //   corrupted by stray writes).
    // - --print: stdout is reserved for the model's reply so it can be piped
    //   / captured cleanly. Logs go to a rolling file mirroring the TUI path.
    //
    // File-mode rotations: daily, keep the last 7 days. Tail the file in
    // another terminal to follow live.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _file_log_guard = if !args.no_tui {
        let log_dir = state_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        let prefix = if args.print { "chaz-cli" } else { "chaz-tui" };
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
            if args.print { "CLI" } else { "TUI" },
            log_dir.display(),
            prefix,
        );
        Some(guard)
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        None
    };

    info!(
        config = %config_path.display(),
        no_tui = args.no_tui,
        no_matrix = args.no_matrix,
        print = args.print,
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
            enable_sync: !args.print,
            run_routine_engine: !args.print,
            extra_auto_approved_tools,
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
    // - `--print`           : one-shot CLI, no background bridges
    // - `--no-tui`          : Matrix is the foreground; required to be configured
    // - default             : TUI is the foreground; Matrix spawns in the background
    //                         iff configured and `!--no-matrix`
    //
    // Cooperative shutdown: when the foreground bridge returns, `shutdown`
    // is notified and background handles are awaited with a timeout so the
    // process doesn't hang on a stuck sync loop.
    let mode = if args.print {
        "cli"
    } else if args.no_tui {
        "matrix-headless"
    } else {
        "tui"
    };
    info!(mode, "Starting bridge");

    let result = if args.print {
        // One-shot: no background bridges, no shutdown plumbing needed.
        let prompt = args.prompt.clone().expect("--print requires PROMPT");
        let bridge = bridge::cli::CliBridge::new(config, secret_store, prompt, args.session);
        bridge.run(server).await
    } else {
        let shutdown = Arc::new(tokio::sync::Notify::new());

        // Background bridge handles — one per configured Matrix login.
        // Logins are a peer-level resource (`logins:` in config, or a single
        // login synthesized from the legacy top-level fields); multiple
        // agents may share a login and one agent may hold a dedicated login
        // (N:M agents↔logins). The spawn loop runs one bridge per login.
        let mut background_handles: Vec<tokio::task::JoinHandle<anyhow::Result<()>>> = Vec::new();

        let matrix_logins = config.matrix_logins();
        let matrix_configured = !matrix_logins.is_empty();

        if matrix_configured {
            if args.no_matrix {
                info!("Matrix configured but --no-matrix supplied; not spawning");
            } else {
                for login in matrix_logins {
                    let login_id = login.login_id.clone();
                    let owning_agent = login.owning_agent.clone();
                    // Per-login matrix client state dir. Logins declared
                    // under an agent are isolated under `{base}/matrix/{login_id}`
                    // so two logins never share a sync token / crypto store;
                    // the legacy synthesized login keeps its historical
                    // location so existing installs don't re-login.
                    let matrix_state_dir = login.state_dir.clone().or_else(|| {
                        if login.explicit {
                            state_dir.as_ref().map(|base| {
                                base.join("matrix")
                                    .join(sanitize_login_id(&login_id))
                                    .to_string_lossy()
                                    .into_owned()
                            })
                        } else {
                            config.state_dir.clone()
                        }
                    });
                    let matrix_bridge = bridge::matrix::MatrixBridge::new(
                        login,
                        matrix_state_dir,
                        config.clone(),
                        secret_store.clone(),
                        shutdown.clone(),
                    )?;
                    let server_for_matrix = server.clone();
                    background_handles.push(tokio::spawn(async move {
                        matrix_bridge.run(server_for_matrix).await
                    }));
                    info!(login_id = %login_id, agent = %owning_agent, "Matrix bridge spawned in background");
                }
            }
        }

        let fg_result = if args.no_tui {
            if background_handles.is_empty() {
                anyhow::bail!("--no-tui requires at least one Matrix login configured");
            }
            // Headless: the foreground "work" is awaiting every spawned login
            // bridge. They run concurrently as tasks; await all and surface
            // the first error. (`--no-tui` conflicts with `--no-matrix` at the
            // clap layer, so at least one handle is guaranteed present here.)
            let mut headless_result = Ok(());
            for handle in background_handles.drain(..) {
                let res = match handle.await {
                    Ok(res) => res,
                    Err(join_err) => {
                        Err(anyhow::anyhow!("matrix bridge task panicked: {join_err}"))
                    }
                };
                if let Err(e) = res {
                    error!("Matrix bridge error: {e}");
                    if headless_result.is_ok() {
                        headless_result = Err(e);
                    }
                }
            }
            headless_result
        } else {
            let mut tui_bridge = bridge::tui::TuiBridge::new(config, secret_store);
            if let Some(prompt) = args.prompt {
                tui_bridge = tui_bridge.with_initial_prompt(prompt);
            }
            tui_bridge.run(server).await
        };

        // TUI (or headless main) exited — drain background bridges.
        if !background_handles.is_empty() {
            shutdown.notify_waiters();
            let drain = async {
                for handle in background_handles {
                    match handle.await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => error!("Background bridge error: {e}"),
                        Err(join_err) => error!("Background bridge task panicked: {join_err}"),
                    }
                }
            };
            if tokio::time::timeout(std::time::Duration::from_secs(5), drain)
                .await
                .is_err()
            {
                warn!("Background bridges did not drain within 5s; exiting anyway");
            }
        }

        fg_result
    };

    if let Err(e) = result {
        error!("Bridge error: {e}");
    }

    Ok(())
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
