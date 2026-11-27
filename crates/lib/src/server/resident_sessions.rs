//! Local client sessions: a catalog watch discovers roots, and a native
//! watch per root discovers work. Neither a catalog row nor a subscription's
//! initial Snapshot is consent to claim a runtime.

use super::{ProcessingCommand, Server};
use crate::session::{Session, SessionIndex, SessionStatus};
use eidetica::store::Table;
use eidetica::{Database, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore, oneshot, watch};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

const IDLE_STORE: &str = "resident_idle_v1";
const RETRY: Duration = Duration::from_secs(15);

/// Derived, peer-local negative evidence, never execution authority. Bind it
/// to the native frontier and to the roster used to distinguish agent output
/// from user requests. An older binary can ignore this store entirely.
#[derive(Clone, Deserialize, Serialize)]
struct IdleProof {
    frontier: Snapshot,
    legacy: Option<Snapshot>,
    agents: Vec<String>,
    has_routines: bool,
}

struct SessionWatch {
    db: Database,
    updates: watch::Receiver<Snapshot>,
    initial: Snapshot,
    _callback: eidetica::WriteCallback,
}

struct Wake {
    frontier: Snapshot,
    live: bool,
}

struct ProbeLanes {
    historical: Semaphore,
    fresh: Semaphore,
    cache_write: Mutex<()>,
}

impl Default for ProbeLanes {
    fn default() -> Self {
        Self {
            historical: Semaphore::new(2),
            fresh: Semaphore::new(4),
            cache_write: Mutex::new(()),
        }
    }
}

impl Server {
    pub(super) fn start_resident_session_watcher(self: &Arc<Self>) {
        if self.executor_authorized {
            let server = self.clone();
            self.track_task(tokio::spawn(async move {
                server.watch_resident_catalog().await
            }));
        }
    }

    async fn watch_resident_catalog(self: Arc<Self>) {
        let mut catalog = self.registry.catalog_changes();
        let lanes = Arc::new(ProbeLanes::default());
        let mut watches = JoinSet::new();
        let mut started = HashMap::new();
        let mut initial = true;
        loop {
            let rows = match self.registry.resident_session_rows().await {
                Ok(rows) => rows,
                Err(error) => {
                    warn!(%error, "Cannot read resident session catalog; retrying");
                    tokio::time::sleep(RETRY).await;
                    continue;
                }
            };
            debug!(rows = rows.len(), "Reconciled resident session catalog");
            let mut eligible = HashSet::new();
            for row in rows {
                let id = row.session_db_id.clone();
                if !is_resident_candidate(&row) {
                    continue;
                }
                eligible.insert(id.clone());
                if started.contains_key(&id)
                    || self.registry.created_session_locally(&id).await && !is_job(&row)
                {
                    continue;
                }
                let server = self.clone();
                let lanes = lanes.clone();
                let (scope, closed) = watch::channel(());
                watches.spawn(async move {
                    server
                        .watch_resident_session(row, &lanes, initial, closed)
                        .await
                });
                started.insert(id, scope);
            }
            // Removing a row closes its scope. A probe already committing or
            // registering is allowed to finish; do not abort a lease acquisition.
            started.retain(|id, _| eligible.contains(id));
            initial = false;
            loop {
                tokio::select! {
                    changed = catalog.changed() => {
                        if changed.is_err() { return; }
                        break;
                    }
                    result = watches.join_next(), if !watches.is_empty() => {
                        if let Some(Err(error)) = result && !error.is_cancelled() {
                            warn!(%error, "Resident session watcher ended unexpectedly");
                        }
                    }
                }
            }
        }
    }

    async fn watch_resident_session(
        self: Arc<Self>,
        row: SessionIndex,
        lanes: &ProbeLanes,
        historical: bool,
        mut closed: watch::Receiver<()>,
    ) {
        let id = &row.session_db_id;
        if is_job(&row) {
            loop {
                let settled = {
                    let _permit = lanes.setup(historical).await;
                    if closed.has_changed().is_err() {
                        return;
                    }
                    self.adopt_indexed_job(id).await
                };
                match settled {
                    Ok(true) => return,
                    Ok(false) => debug!(session_db_id = id, "Indexed job is not yet adoptable"),
                    Err(error) => {
                        debug!(session_db_id = id, %error, "Indexed job could not be adopted")
                    }
                }
                tokio::select! {
                    _ = closed.changed() => return,
                    _ = tokio::time::sleep(RETRY) => {},
                }
            }
        }
        let mut historical = historical;
        let mut proof = None;
        let mut routines = Vec::new();
        let (session, mut setup) = loop {
            let permit = lanes.setup(historical).await;
            if closed.has_changed().is_err() {
                return;
            }
            #[cfg(test)]
            if let Some(events) = self.local_adoption_events.get() {
                let _ = events.send(super::LocalAdoptionEvent::Started(id.clone()));
            }
            let result = self.subscribe_resident_session(id).await;
            match result {
                Ok(Some(watch)) => break (watch, Some(permit)),
                // A different key/authority owns this tree. Exposed bridges
                // and Workers have independent admission paths.
                Ok(None) => return,
                Err(error) => {
                    debug!(session_db_id = id, %error, "Cannot start local session watch; retrying this root")
                }
            }
            drop(permit);
            tokio::select! {
                _ = closed.changed() => return,
                _ = tokio::time::sleep(RETRY) => {},
            }
            historical = false;
        };
        let SessionWatch {
            db,
            mut updates,
            initial,
            _callback,
        } = session;
        let mut wake = Wake {
            frontier: initial,
            live: false,
        };
        let mut was_active = false;
        #[cfg(test)]
        let mut first_probe = true;
        'probe: loop {
            // Once the existing runtime watch has taken over, coalesce its
            // burst of transcript writes before updating the idle proof.
            if was_active {
                loop {
                    tokio::select! {
                        _ = closed.changed() => break 'probe,
                        _ = tokio::time::sleep(Duration::from_secs(1)) => break,
                        update = updates.changed() => {
                            if update.is_err() { break 'probe; }
                            wake.frontier = updates.borrow_and_update().clone();
                            wake.live = true;
                        }
                    }
                }
            }
            let result = {
                let _permit = match setup.take() {
                    Some(permit) => permit,
                    None => lanes.setup(false).await,
                };
                if closed.has_changed().is_err() {
                    break;
                }
                self.probe_resident_session(
                    id,
                    &db,
                    &wake,
                    &mut proof,
                    &mut routines,
                    &lanes.cache_write,
                )
                .await
            };
            let delay = match result {
                Ok(active) => {
                    #[cfg(test)]
                    if std::mem::take(&mut first_probe)
                        && let Some(events) = self.local_adoption_events.get()
                    {
                        let _ = events.send(super::LocalAdoptionEvent::InitiallyProbed(id.clone()));
                    }
                    was_active |= active;
                    None
                }
                Err(error) => {
                    warn!(session_db_id = id, %error, "Cannot reconcile local session; retrying this root");
                    Some(RETRY)
                }
            };
            tokio::select! {
                _ = closed.changed() => break,
                update = updates.changed() => if update.is_err() { break; },
                _ = async {
                    match delay {
                        Some(delay) => tokio::time::sleep(delay).await,
                        None => std::future::pending().await,
                    }
                } => continue,
            }
            wake.frontier = updates.borrow_and_update().clone();
            wake.live = true;
        }
        if !routines.is_empty()
            && let Some(engine) = self.routine_engine()
        {
            engine.deregister_session(id).await;
        }
    }

    pub(super) async fn adopt_indexed_job(&self, id: &str) -> anyhow::Result<bool> {
        let (_, db) = match self.registry.open_session(id).await {
            Ok(db) => db,
            Err(_) => self.registry.open_job_session(id).await?,
        };
        self.register_accepted_job(&db).await
    }

    async fn subscribe_resident_session(&self, id: &str) -> anyhow::Result<Option<SessionWatch>> {
        let Some(db) = self.registry.bind_local_client_session(id).await? else {
            return Ok(None);
        };
        let (first, initial) = oneshot::channel();
        let first = std::sync::Mutex::new(Some(first));
        let (tx, updates) = watch::channel(Snapshot::EMPTY);
        let callback = db
            .on_write(move |snapshot, _| {
                // The SDK keeps initial delivery separate from later
                // coalesced writes. Keep that distinction for legacy cutoff.
                if let Some(first) = first.lock().unwrap().take() {
                    let _ = first.send(snapshot.clone());
                } else {
                    tx.send_replace(snapshot.clone());
                }
                std::future::ready(Ok(()))
            })
            .await?;
        debug!(session_db_id = id, "Bound native resident session watch");
        let initial = tokio::time::timeout(RETRY, initial).await??;
        debug!(
            session_db_id = id,
            "Received initial resident session frontier"
        );
        Ok(Some(SessionWatch {
            db,
            updates,
            initial,
            _callback: callback,
        }))
    }

    async fn probe_resident_session(
        &self,
        id: &str,
        db: &Database,
        wake: &Wake,
        proof: &mut Option<IdleProof>,
        routines: &mut Vec<crate::routine::Routine>,
        cache_write: &Mutex<()>,
    ) -> anyhow::Result<bool> {
        let at = &wake.frontier;
        if proof.is_none() {
            let txn = self.registry.chaz_peer().new_transaction().await?;
            let store = txn.get_store::<Table<IdleProof>>(IDLE_STORE).await?;
            match store.get(id).await {
                Ok(found) => *proof = Some(found),
                Err(error) if error.is_not_found() => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut agents = self.agents.names();
        agents.sort_unstable();
        let exact = proof
            .as_ref()
            .filter(|p| p.frontier == *at && p.agents == agents);
        if exact.is_none_or(|p| p.has_routines) {
            let rows = crate::routine::list_session_routines_at(db, at).await?;
            let rows: Vec<_> = rows.into_iter().filter(|r| r.enabled).collect();
            if *routines != rows {
                if let Some(engine) = self.routine_engine() {
                    engine.deregister_session(id).await;
                    engine.register_session_routines(id, rows.clone()).await;
                }
                *routines = rows;
            }
        }
        if exact.is_some() {
            debug!(session_db_id = id, "Reused proven idle session frontier");
            return Ok(false);
        }
        debug!(
            session_db_id = id,
            "Reading resident work at changed or unindexed frontier"
        );
        let (work, legacy) = Session::resident_work_at(
            db,
            at,
            proof.as_ref().and_then(|p| p.legacy.as_ref()),
            wake.live,
            |name| agents.binary_search_by(|a| a.as_str().cmp(name)).is_ok(),
        )
        .await?;
        if work {
            if self.runtime_mode() == crate::config::RuntimeMode::Never {
                return Ok(false);
            }
            let lock = self.registration_lock(id).await;
            let _registration = lock.lock().await;
            if !self.is_watching_session(id).await
                && self.registry.local_resident_row_is_active(id).await?
            {
                // Passive binding alone does not publish a writer handle or
                // a runtime. The fresh key check also catches revocation.
                let Some(writer) = self.registry.bind_local_client_session(id).await? else {
                    return Ok(false);
                };
                anyhow::ensure!(
                    !crate::session::jobs::is_job_session(&writer).await,
                    "job session requires executor admission"
                );
                Session::ensure_turn_schema_baseline_from(&writer, legacy.as_ref()).await?;
                self.registry.retain_local_client_session(id, &writer).await;
                self.claim_and_watch_session(&writer, self.default_backend.clone(), None, None)
                    .await?;
                info!(
                    session_db_id = id,
                    "Resident executor registered local client session"
                );
                // Existing catch-up handles ordinary durable requests. The
                // extra wake covers e.g. an Agent mention at the initial tip.
                let _ = self
                    .notify_tx
                    .send(ProcessingCommand::Wake(id.to_owned()))
                    .await;
            }
            return Ok(true);
        }
        let idle = IdleProof {
            frontier: at.clone(),
            legacy,
            agents,
            has_routines: !routines.is_empty(),
        };
        let _write = cache_write.lock().await;
        let txn = self.registry.chaz_peer().new_transaction().await?;
        txn.get_store::<Table<IdleProof>>(IDLE_STORE)
            .await?
            .set(id, idle.clone())
            .await?;
        txn.commit().await?;
        debug!(session_db_id = id, "Recorded idle session frontier");
        *proof = Some(idle);
        Ok(false)
    }
}

impl ProbeLanes {
    async fn setup(&self, historical: bool) -> tokio::sync::SemaphorePermit<'_> {
        // Reserve independent capacity for new discoveries / live wakes so a
        // populated history cannot occupy every probing slot at startup.
        let lane = if historical {
            &self.historical
        } else {
            &self.fresh
        };
        lane.acquire()
            .await
            .expect("resident probe lanes never close")
    }
}

fn is_job(row: &SessionIndex) -> bool {
    row.source
        .as_deref()
        .is_some_and(|source| source.starts_with("job-stage:"))
}

pub(super) fn is_resident_candidate(row: &SessionIndex) -> bool {
    row.status != SessionStatus::Closed
        && !row
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("spawn:"))
}
