//! First frame and local input do not depend on the connector or session DB.
//! Connection reads may be cancelled; construction/launch may write and are joined.
use super::*;
use chaz_core::{
    instance,
    server::{self, BuiltServer},
};
use std::{path::PathBuf, time::Instant};
use tokio::sync::watch;

struct Initialized {
    config: Config,
    built: BuiltServer,
    tab: Option<Tab>,
}

impl TuiBridge {
    pub async fn start(
        config: Config,
        path: PathBuf,
        session: Option<String>,
        prompt: Option<String>,
        started: Instant,
    ) -> anyhow::Result<()> {
        supervised::install_panic_hook();
        let terminal_owner = supervised::TerminalGuard;
        let mut terminal = init_terminal()?;
        let mut events = EventStream::new();
        let mut app = App::hub(HashSet::new());
        app.backend_available = false;
        app.session_catalog_loading = true;
        app.input = prompt.clone().unwrap_or_default();
        app.cursor = app.input.len();
        let label = session
            .clone()
            .or_else(|| prompt.as_ref().map(|_| "New session".into()));
        draw(&mut terminal, &mut app, label.as_deref(), "Connecting…")?;
        tracing::info!(
            first_frame_ms = started.elapsed().as_millis() as u64,
            "TUI first frame drawn"
        );

        let (approval_tx, approval_rx) = mpsc::channel::<TaggedApproval>(8);
        let (notify_tx, notify_rx) = mpsc::channel::<String>(64);
        let (stage_tx, mut stage_rx) = watch::channel("Connecting…");
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let worker_path = path.clone();
        let worker_approval = approval_tx.clone();
        let worker_notify = notify_tx.clone();
        let mut task = Some(supervised::Task(tokio::spawn(supervised::work(Box::pin(
            async move {
                initialize(
                    config,
                    worker_path,
                    session,
                    prompt.is_some(),
                    cancel_rx,
                    stage_tx,
                    worker_approval,
                    worker_notify,
                    started,
                )
                .await
            },
        )))));
        let mut error = None;
        let result = async {
            loop {
                tokio::select! {
                    joined = async { (&mut task.as_mut().unwrap().0).await }, if task.is_some() => {
                        task.take();
                        match joined.unwrap_or_else(|e| Err(format!("Startup worker failed: {e}"))) {
                            Ok(Some(ready)) => return Ok(Some(ready)),
                            Ok(None) => return Ok(None),
                            Err(e) => {
                                tracing::warn!("TUI startup failed: {e}");
                                error = Some(format!("Startup failed: {e}"));
                            }
                        }
                    }
                    Ok(()) = stage_rx.changed() => {}
                    Some(event) = events.next() => {
                        match event? {
                            Event::Key(key) if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL) => {
                                return match &error {
                                    Some(e) => Err(anyhow::anyhow!(e.clone())),
                                    None => Ok(None),
                                };
                            }
                            Event::Key(key) => {
                                if !matches!(input::handle_overlay_key(&mut app, key), input::OverlayKey::NotConsumed) {
                                    // Help remains local even before a runtime exists.
                                } else if key.code == KeyCode::F(1) {
                                    app.overlay = Some(Overlay::Help { scroll: 0 });
                                } else if label.is_some() && key.code == KeyCode::Esc {
                                    return match &error {
                                        Some(e) => Err(anyhow::anyhow!(e.clone())),
                                        None => Ok(None),
                                    };
                                } else if label.is_some() && !key.modifiers.contains(KeyModifiers::CONTROL) {
                                    // Editing is local. Enter cannot submit before a session exists.
                                    input::edit_draft(&mut app, key);
                                    app.completion = None;
                                } else if label.is_none() && matches!(key.code, KeyCode::Enter | KeyCode::Char('n' | 's' | 'r')) {
                                    app.operations.notice = Some("Backend initializing — action not accepted; Ctrl+C quit".into());
                                }
                            }
                            Event::Mouse(_) | Event::Resize(..) => {}
                            _ => continue,
                        }
                    }
                }
                let status = error.as_deref().unwrap_or(*stage_rx.borrow());
                draw(&mut terminal, &mut app, label.as_deref(), status)?;
            }
        }.await;
        if let Ok(Some(ready)) = result {
            app.backend_available = true;
            app.session_catalog_loading = false;
            app.hub_notice = None;
            app.operations.notice = None;
            app.agent_names = ready.built.server.agents().names().into_iter().collect();
            if let Some(tab) = ready.tab {
                let draft = (std::mem::take(&mut app.input), app.cursor);
                app.push_tab(tab);
                app.mode = TuiMode::Chat;
                (app.input, app.cursor) = draft;
                input::recompute_completion(&mut app);
            }
            return TuiBridge {
                config: ready.config,
                secrets: ready.built.secret_store.clone(),
            }
            .run_ready(
                ready.built,
                path,
                &mut terminal,
                &mut events,
                app,
                terminal_owner,
                approval_tx,
                approval_rx,
                notify_tx,
                notify_rx,
            )
            .await;
        }
        // Restore before joining write-capable work, including on draw/input errors.
        drop(terminal);
        drop(terminal_owner);
        cancel_tx.send_replace(true);
        if let Some(mut task) = task {
            match (&mut task.0).await {
                Ok(Ok(Some(ready))) => ready.built.shutdown().await,
                Ok(Err(e)) => tracing::warn!("Startup settled after exit: {e}"),
                _ => {}
            }
        }
        result.map(|_| ())
    }
}

fn draw(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    app: &mut App,
    label: Option<&str>,
    status: &str,
) -> anyhow::Result<()> {
    terminal.draw(|f| {
        app.click_regions.clear();
        if let Some(label) = label {
            view::ui_loading(f, app, label, status);
        } else {
            app.hub_notice = Some(format!("{status} · Ctrl+C quit"));
            view::ui_picker(f, app);
        }
        if app.overlay.is_some() {
            view::ui_overlay(f, app);
        }
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn initialize(
    mut config: Config,
    path: PathBuf,
    session: Option<String>,
    prompt: bool,
    mut cancel: watch::Receiver<bool>,
    stage: watch::Sender<&'static str>,
    approval_tx: mpsc::Sender<TaggedApproval>,
    notify_tx: mpsc::Sender<String>,
    started: Instant,
) -> Result<Option<Initialized>, String> {
    let t = Instant::now();
    let connected = tokio::select! {
        result = instance::connect(&config) => result.map_err(|e| format!("{e:#}"))?,
        _ = cancel.changed() => return Ok(None),
    };
    tracing::info!(
        elapsed_ms = t.elapsed().as_millis() as u64,
        "eidetica opened"
    );
    if *cancel.borrow() {
        return Ok(None);
    }
    stage.send_replace("Initializing runtime…");
    let t = Instant::now();
    let mut options = server::BuildOptions::local(
        path,
        connected.capabilities,
        Vec::new(),
        server::McpReadiness::Deferred,
    );
    if options.targeted_session.is_some() {
        options.targeted_session = Some(session.clone().unwrap_or_default());
    }
    let built = server::build(&mut config, connected.instance, connected.user, options)
        .await
        .map_err(|e| format!("{e:#}"))?;
    tracing::info!(
        build_ms = t.elapsed().as_millis() as u64,
        time_to_gateway_ms = started.elapsed().as_millis() as u64,
        "Server built; handing off to gateway"
    );
    if *cancel.borrow() {
        built.shutdown().await;
        return Ok(None);
    }
    stage.send_replace("Loading session…");
    // Retain the BuiltServer outside the panic boundary so launch failures shut it down.
    let backend = BackendManager::new(&config.backends, built.secret_store.clone());
    let tab = supervised::work(Box::pin(async {
        let Some(db) = launch_session(&built.server, session.as_deref(), prompt)
            .await
            .map_err(|e| format!("{e:#}"))?
        else {
            return Ok(None);
        };
        if chaz_core::session::jobs::is_job_session(&db).await {
            return jobs::open_tab(
                &built.server,
                &backend,
                &db.root_id().to_string(),
                notify_tx,
            )
            .await
            .map(Some);
        }
        let observation =
            setup_session(&built.server, &db, backend.clone(), approval_tx, notify_tx)
                .await
                .map_err(|e| format!("{e:#}"))?;
        let id = db.root_id().to_string();
        let server = built.server.clone();
        let mut tab =
            supervised::read(async move { build_tab(&server, &backend, db, id).await }).await?;
        tab.live.observation = Some(observation);
        Ok(Some(tab))
    }))
    .await;
    match tab {
        Ok(tab) => {
            tracing::info!(
                usable_session_ms = started.elapsed().as_millis() as u64,
                has_session = tab.is_some(),
                "TUI launch ready"
            );
            Ok(Some(Initialized { config, built, tab }))
        }
        Err(e) => {
            built.shutdown().await;
            Err(e)
        }
    }
}
