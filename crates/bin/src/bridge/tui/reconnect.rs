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
    snapshot: refresh::SessionSnapshot,
    status: Result<Vec<String>, String>,
    observation: Observation,
}
pub(super) struct Replacement {
    pub built: BuiltServer,
    pub tabs: Vec<Reopened>,
}
pub(super) enum ResultMessage {
    Retry,
    Probe(Result<(), (String, bool)>),
    Rebuilt(Result<Box<Replacement>, (String, bool)>),
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
        let registry = built.registry.clone();
        let server = built.server.clone();
        let backend = backend.clone();
        let approval_tx = approval_tx.clone();
        let notify_tx = notify_tx.clone();
        let target = id.clone();
        let result = supervised::read(async move {
            let (_, db) = registry
                .open_session(&target)
                .await
                .map_err(|e| e.to_string())?;
            let snapshot = refresh::load_session_snapshot(&target, db.clone()).await?;
            let status = refresh::load_status_segments(&db).await;
            Ok((db, snapshot, status))
        })
        .await;
        let result = match result {
            Ok((db, snapshot, status)) => {
                match setup_session(&server, &db, backend, approval_tx, notify_tx).await {
                    Ok(observation) => Ok(ReopenedState {
                        db,
                        snapshot,
                        status,
                        observation,
                    }),
                    Err(e) => Err(e.to_string()),
                }
            }
            Err(e) => Err(e),
        };
        reopened.push(Reopened {
            id,
            incarnation,
            result,
        });
    }
    Ok(Box::new(Replacement {
        built,
        tabs: reopened,
    }))
}

pub(super) fn disconnect(app: &mut App) {
    app.backend_available = false;
    app.cancel_session_fill();
    app.model_catalog_task = None;
    app.model_picker_loading = false;
    for tab in &mut app.tabs {
        tab.live.cancel_reads();
        tab.live.observation = None;
    }
    app.operations.notice = Some("Backend disconnected — cached data is stale; reconnecting after accepted actions settle. No writes replayed".into());
}

pub(super) fn apply(app: &mut App, replacement: &mut Replacement, backend: &BackendManager) {
    for reopened in replacement.tabs.drain(..) {
        let Some(i) = app
            .tab_index_for(&reopened.id)
            .filter(|&i| app.tabs[i].live.incarnation == reopened.incarnation)
        else {
            continue;
        };
        match reopened.result {
            Ok(ReopenedState {
                db,
                snapshot,
                status,
                observation,
            }) => {
                app.tabs[i].session_db = db;
                app.tabs[i].live.observation = Some(observation);
                refresh::replace_snapshot(app, i, snapshot, |agent, meta| {
                    refresh::resolve(&replacement.built.server, backend, agent, meta)
                });
                refresh::replace_status(&mut app.tabs[i], status);
            }
            Err(error) => {
                app.tabs[i].live.error = Some(error);
            }
        }
    }
    app.backend_available = true;
    app.operations.notice = Some("Backend reconnected; writes were not replayed".into());
    app.session_list_fresh = false;
}
