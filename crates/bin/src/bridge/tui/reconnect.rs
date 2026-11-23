//! Replace a dead service generation without replaying UI actions. The existing
//! connector/build path owns capability checks, identity and reconciliation.
use super::*;
use chaz_core::{
    instance,
    server::{self, BuiltServer},
};
use std::{collections::hash_map::RandomState, hash::BuildHasher, path::PathBuf, time::Duration};

pub(super) const PROBE_INTERVAL: Duration = Duration::from_secs(5);

pub(super) enum ProbeFailure {
    Deadline,
    Disconnected(String),
    Other(String),
}

pub(super) struct Reopened {
    pub id: String,
    pub incarnation: u64,
    pub result: Result<ReopenedState, String>,
}
pub(super) struct ReopenedState {
    db: eidetica::Database,
    observation: Result<Observation, String>,
}
pub(super) enum ResultMessage {
    Probe(Result<(), ProbeFailure>),
    Rebuilt(Result<Box<BuiltServer>, (String, bool)>),
    Tab {
        ticket: u64,
        reopened: Box<Reopened>,
    },
}

/// View intent survives service generations; database kind alone cannot
/// distinguish an ordinary parent opened for inspection from a composer tab.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Session,
    Job,
    Inspection,
}
impl Mode {
    fn of(tab: &Tab) -> Self {
        if tab.job.is_some() {
            Self::Job
        } else if tab.live.worker_observer {
            Self::Inspection
        } else {
            Self::Session
        }
    }
}

async fn reopen(
    server: Arc<Server>,
    backend: BackendManager,
    id: String,
    incarnation: u64,
    mode: Mode,
    approval_tx: mpsc::Sender<TaggedApproval>,
    notify_tx: presentation::Sender,
) -> Reopened {
    let opener = server.clone();
    let target = id.clone();
    let result = supervised::work(async move {
        let opened = if mode == Mode::Job {
            opener.registry().open_job_session(&target).await
        } else {
            opener.registry().open_session_for_inspection(&target).await
        };
        opened.map(|(_, db)| db).map_err(|e| e.to_string())
    })
    .await;
    let result = match result {
        Ok(db) => {
            // Installing a fresh handle must not depend on hydration or even
            // observation setup succeeding. Presentation reads have own slots.
            let observed = db.clone();
            // Observation installation can register/reconcile runtime state,
            // like construction; a read deadline is not rollback authority.
            // Keep the large registration future off the worker's stack.
            let observation = supervised::work(Box::pin(async move {
                if mode != Mode::Session
                    || chaz_core::session::workers::is_worker_session(&observed)
                        .await
                        .map_err(|e| e.to_string())?
                {
                    jobs::observe(&observed, notify_tx)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    setup_session(&server, &observed, backend, approval_tx, notify_tx)
                        .await
                        .map_err(|e| e.to_string())
                }
            }))
            .await;
            Ok(ReopenedState { db, observation })
        }
        Err(error) => Err(error),
    };
    Reopened {
        id,
        incarnation,
        result,
    }
}

pub(super) fn spawn_rebuild(
    old: Option<BuiltServer>,
    config: Config,
    path: PathBuf,
    tx: &mpsc::Sender<ResultMessage>,
) -> supervised::Task<()> {
    let tx = tx.clone();
    supervised::Task(tokio::spawn(async move {
        let result =
            supervised::work(async move { Ok(rebuild(old, config, path).await.map(Box::new)) })
                .await
                .unwrap_or_else(|e| Err((e, false)));
        if let Err(e) = tx.send(ResultMessage::Rebuilt(result)).await
            && let ResultMessage::Rebuilt(Ok(replacement)) = e.0
        {
            replacement.shutdown().await;
        }
    }))
}

async fn probe(instance: &eidetica::Instance) -> Result<(), ProbeFailure> {
    match tokio::time::timeout(supervised::READ_TIMEOUT, instance::probe_service(instance)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) if e.is_transient_service() => Err(ProbeFailure::Disconnected(e.to_string())),
        Ok(Err(e)) => Err(ProbeFailure::Other(e.to_string())),
        Err(_) => Err(ProbeFailure::Deadline),
    }
}

pub(super) fn spawn_probe(
    server: &Server,
    tx: &mpsc::Sender<ResultMessage>,
) -> supervised::Task<()> {
    let instance = server.registry().instance().clone();
    let tx = tx.clone();
    supervised::Task(tokio::spawn(async move {
        let result = supervised::work(async { Ok(probe(&instance).await) })
            .await
            .unwrap_or_else(|e| Err(ProbeFailure::Other(e)));
        let _ = tx.send(ResultMessage::Probe(result)).await;
    }))
}

pub(super) fn retry_delay(attempt: u32) -> Duration {
    // Use the existing service backoff, jittered below its cap so clients
    // that lost the same daemon don't all retry on the same beat.
    let percent = 75 + (RandomState::new().hash_one(attempt) % 26) as u32;
    instance::service_reconnect_delay(attempt) * percent / 100
}

pub(super) async fn rebuild(
    old: Option<BuiltServer>,
    mut config: Config,
    path: PathBuf,
) -> Result<BuiltServer, (String, bool)> {
    if let Some(old) = old {
        old.shutdown().await;
    }
    // Connecting/logging in is a cancellable read, construction is not.
    let connected = tokio::time::timeout(supervised::READ_TIMEOUT, instance::connect(&config))
        .await
        .map_err(|_| ("Service reconnect timed out".into(), true))?
        .map_err(|e| {
            let transient = e.is_transient_service()
                || matches!(&e, instance::ConnectError::Login { source, .. } if source.is_io_error());
            (e.to_string(), transient)
        })?;
    let options = server::BuildOptions::local(
        path,
        connected.capabilities,
        Vec::new(),
        server::McpReadiness::Deferred,
    );
    // Construction may have side effects. If IPC stops answering during
    // construction, retire the candidate socket and join, not abort or
    // overlap construction. A slow build on a responsive socket is fine.
    let instance = connected.instance.clone();
    let build = server::build(&mut config, connected.instance, connected.user, options);
    tokio::pin!(build);
    let lost_service = async {
        loop {
            tokio::time::sleep(PROBE_INTERVAL).await;
            if matches!(
                probe(&instance).await,
                Err(ProbeFailure::Deadline | ProbeFailure::Disconnected(_))
            ) {
                break;
            }
        }
    };
    tokio::select! {
        result = &mut build => result.map_err(|e| {
            let transient = e.chain().any(|e| {
                e.downcast_ref::<eidetica::Error>()
                    .is_some_and(|e| e.is_io_error())
            });
            (e.to_string(), transient)
        }),
        _ = lost_service => {
            if let Some(connection) = instance.remote_connection() {
                connection.retire();
            }
            if let Ok(built) = build.await {
                built.shutdown().await;
            }
            Err(("Service stopped answering during reconnect".into(), true))
        }
    }
}

pub(super) fn disconnect(app: &mut App, reason: &str) {
    app.backend_available = false;
    app.backend_notice = Some(format!("{reason} · cached data stale"));
    operations::cancel_unstarted(app);
    jobs::cancel(app);
    app.jobs.error = Some("Retained snapshot is stale".into());
    peer_pages::suspend(app);
    session_pages::suspend(app);
    app.cancel_session_fill();
    app.model_catalog_ticket += 1;
    app.model_catalog_task = None;
    app.model_picker_loading = false;
    for tab in &mut app.tabs {
        suspend_tab(tab);
    }
}

/// A successful open can already be waiting in the operation channel when
/// the generation is retired. Keep the view, never admit work on its old DB.
pub(super) fn suspend_tab(tab: &mut Tab) {
    tab.live.cancel_reads();
    tab.live.observation = None;
    tab.live.rebind_needed = true;
    tab.live.unbound = true;
    tab.pending_approval = None;
}

fn apply_tab(app: &mut App, reopened: Reopened) -> Option<String> {
    let i = app
        .tab_index_for(&reopened.id)
        .filter(|&i| app.tabs[i].live.incarnation == reopened.incarnation)?;
    let tab = &mut app.tabs[i];
    tab.live.cancel_reads();
    let error = match reopened.result {
        Ok(ReopenedState { db, observation }) => {
            tab.session_db = db;
            tab.live.unbound = false;
            match observation {
                Ok(observation) => {
                    tab.live.observation = Some(observation);
                    tab.live.rebind_needed = false;
                    None
                }
                Err(error) => {
                    tab.live.observation = None;
                    Some(error)
                }
            }
        }
        Err(error) => Some(error),
    };
    tab.live.binding_error = error;
    tab.live.rebind_needed = tab.live.binding_error.is_some();
    refresh::sync_error(tab);
    (!tab.live.unbound).then_some(reopened.id)
}

pub(super) fn connected(app: &mut App) {
    app.backend_available = true;
    app.backend_notice = None;
    app.operations.notice = None;
    app.session_list_fresh = false;
}

/// Stagger rebinding on the existing tick. Give the active tab the first
/// slot, but let never-tried/older tabs proceed even if it repeatedly fails.
pub(super) fn request_tabs(
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &presentation::Sender,
    tx: &mpsc::Sender<ResultMessage>,
) {
    if !app.backend_available
        || app
            .tabs
            .iter()
            .filter(|t| t.live.rebind_task.is_some())
            .count()
            >= 2
    {
        return;
    }
    let Some((_, tab)) = app
        .tabs
        .iter_mut()
        .enumerate()
        .filter(|(_, tab)| {
            (tab.live.rebind_needed || tab.live.error.is_some()) && !tab.live.pending()
        })
        .min_by_key(|(i, tab)| (tab.live.rebind_ticket, *i != app.active_tab))
    else {
        return;
    };
    app.refresh_ticket += 1;
    let ticket = app.refresh_ticket;
    tab.live.rebind_ticket = Some(ticket);
    let id = tab.session_db_id.clone();
    let incarnation = tab.live.incarnation;
    let mode = Mode::of(tab);
    let server = server.clone();
    let backend = backend.clone();
    let approval_tx = approval_tx.clone();
    let notify_tx = notify_tx.clone();
    let tx = tx.clone();
    tab.live.rebind_task = Some(supervised::Task(tokio::spawn(async move {
        let fallback_id = id.clone();
        let reopened = notify_tx
            .clone()
            .prepare(Box::pin(async move {
                Ok(reopen(
                    server,
                    backend,
                    id,
                    incarnation,
                    mode,
                    approval_tx,
                    notify_tx,
                )
                .await)
            }))
            .await
            .unwrap_or_else(|error| Reopened {
                id: fallback_id,
                incarnation,
                result: Err(error),
            });
        let _ = tx
            .send(ResultMessage::Tab {
                ticket,
                reopened: Box::new(reopened),
            })
            .await;
    })));
}

pub(super) fn apply_retry(app: &mut App, ticket: u64, reopened: Reopened) -> Option<String> {
    let i = app.tab_index_for(&reopened.id).filter(|&i| {
        app.tabs[i].live.incarnation == reopened.incarnation
            && app.tabs[i].live.rebind_ticket == Some(ticket)
            && app.tabs[i].live.rebind_task.is_some()
    })?;
    let bound = apply_tab(app, reopened);
    if app.tabs[i].live.rebind_needed {
        app.tabs[i].live.rebind_ticket = Some(ticket);
    }
    bound
}
