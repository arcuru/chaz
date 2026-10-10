//! Replace a dead service generation without replaying UI actions. The existing
//! connector/build path owns capability checks, identity and reconciliation.
use super::*;
use chaz_core::{
    instance,
    server::{self, BuiltServer},
};
use std::path::PathBuf;

pub(super) struct Reopened {
    pub id: String,
    pub incarnation: u64,
    pub result: Result<ReopenedState, String>,
}
pub(super) struct ReopenedState {
    db: eidetica::Database,
    observation: Result<Observation, String>,
}
pub(super) struct Replacement {
    pub built: BuiltServer,
    pub tabs: Vec<Reopened>,
}
pub(super) enum ResultMessage {
    Retry,
    Probe(Result<(), (String, bool)>),
    Rebuilt(Result<Box<Replacement>, (String, bool)>),
    Tab {
        ticket: u64,
        reopened: Box<Reopened>,
    },
}

async fn reopen(
    server: Arc<Server>,
    backend: BackendManager,
    id: String,
    incarnation: u64,
    approval_tx: mpsc::Sender<TaggedApproval>,
    notify_tx: mpsc::Sender<String>,
) -> Reopened {
    let opener = server.clone();
    let target = id.clone();
    let result = supervised::read(async move {
        opener
            .registry()
            .open_session(&target)
            .await
            .map(|(_, db)| db)
            .map_err(|e| e.to_string())
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
                setup_session(&server, &observed, backend, approval_tx, notify_tx)
                    .await
                    .map_err(|e| e.to_string())
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

pub(super) async fn rebuild(
    old: Option<BuiltServer>,
    mut config: Config,
    path: PathBuf,
    tabs: Vec<(String, u64)>,
    approval_tx: mpsc::Sender<TaggedApproval>,
    notify_tx: mpsc::Sender<String>,
) -> Result<Box<Replacement>, (String, bool)> {
    if let Some(old) = old {
        old.shutdown().await;
    }
    // Connecting/logging in is a cancellable read, construction is not.
    let connected = tokio::time::timeout(supervised::READ_TIMEOUT, instance::connect(&config))
        .await
        .map_err(|_| ("Service reconnect timed out".into(), true))?
        .map_err(|e| (e.to_string(), e.is_transient_service()))?;
    let options = server::BuildOptions::local(
        path,
        connected.capabilities,
        Vec::new(),
        server::McpReadiness::Deferred,
    );
    let built = server::build(&mut config, connected.instance, connected.user, options)
        .await
        .map_err(|e| {
            let transient = e.chain().any(|e| {
                e.downcast_ref::<eidetica::Error>()
                    .is_some_and(|e| e.is_io_error())
            });
            (e.to_string(), transient)
        })?;
    let backend = BackendManager::new(&config.backends, built.secret_store.clone());
    let mut reopened = Vec::new();
    for (id, incarnation) in tabs {
        reopened.push(
            reopen(
                built.server.clone(),
                backend.clone(),
                id,
                incarnation,
                approval_tx.clone(),
                notify_tx.clone(),
            )
            .await,
        );
    }
    Ok(Box::new(Replacement {
        built,
        tabs: reopened,
    }))
}

pub(super) fn disconnect(app: &mut App) {
    app.backend_available = false;
    app.cancel_session_fill();
    app.model_catalog_ticket += 1;
    app.model_catalog_task = None;
    app.model_picker_loading = false;
    for tab in &mut app.tabs {
        tab.live.cancel_reads();
        tab.live.observation = None;
        tab.live.rebind_needed = true;
        tab.live.unbound = true;
    }
    app.operations.notice = Some("Backend disconnected — cached data is stale; reconnecting after accepted actions settle. No writes replayed".into());
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
                Err(error) => Some(error),
            }
        }
        Err(error) => Some(error),
    };
    tab.live.binding_error = error;
    tab.live.rebind_needed = tab.live.binding_error.is_some();
    refresh::sync_error(tab);
    (!tab.live.unbound).then_some(reopened.id)
}

pub(super) fn apply(app: &mut App, replacement: &mut Replacement) {
    for reopened in replacement.tabs.drain(..) {
        apply_tab(app, reopened);
    }
    app.backend_available = true;
    // Recovery is a lifecycle transition, not an indefinite status override.
    // Unresolved write uncertainty belongs to Operations, independently.
    app.operations.notice = None;
    app.session_list_fresh = false;
}

/// Retry only missing per-tab handles/observations on the healthy generation.
/// One owned read per tab, driven by the existing five-second tick; no runtime
/// rebuild and no accepted write is replayed.
pub(super) fn request_tabs(
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
    tx: &mpsc::Sender<ResultMessage>,
) {
    if !app.backend_available {
        return;
    }
    for tab in &mut app.tabs {
        if !tab.live.rebind_needed || tab.live.rebind_task.is_some() {
            continue;
        }
        app.refresh_ticket += 1;
        let ticket = app.refresh_ticket;
        tab.live.rebind_ticket = Some(ticket);
        let id = tab.session_db_id.clone();
        let incarnation = tab.live.incarnation;
        let server = server.clone();
        let backend = backend.clone();
        let approval_tx = approval_tx.clone();
        let notify_tx = notify_tx.clone();
        let tx = tx.clone();
        tab.live.rebind_task = Some(supervised::Task(tokio::spawn(async move {
            let reopened = reopen(server, backend, id, incarnation, approval_tx, notify_tx).await;
            let _ = tx
                .send(ResultMessage::Tab {
                    ticket,
                    reopened: Box::new(reopened),
                })
                .await;
        })));
    }
}

pub(super) fn apply_retry(app: &mut App, ticket: u64, reopened: Reopened) -> Option<String> {
    let i = app.tab_index_for(&reopened.id).filter(|&i| {
        app.tabs[i].live.incarnation == reopened.incarnation
            && app.tabs[i].live.rebind_ticket == Some(ticket)
    })?;
    app.tabs[i].live.rebind_task = None;
    app.tabs[i].live.rebind_ticket = None;
    apply_tab(app, reopened)
}
