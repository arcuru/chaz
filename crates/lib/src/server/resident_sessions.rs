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

struct Observation {
    initial: Snapshot,
    observed: Snapshot,
    proof: Option<IdleProof>,
    routines: Vec<crate::routine::Routine>,
}

impl Observation {
    fn new(initial: Snapshot) -> Self {
        Self {
            observed: initial.clone(),
            initial,
            proof: None,
            routines: Vec::new(),
        }
    }
}

struct ProbeLanes {
    historical: Semaphore,
    fresh: Semaphore,
    cache: Mutex<Database>,
}

impl ProbeLanes {
    fn new(cache: Database) -> Self {
        Self {
            historical: Semaphore::new(2),
            fresh: Semaphore::new(4),
            cache: Mutex::new(cache),
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
        let lanes = Arc::new(ProbeLanes::new(self.registry.chaz_peer().clone()));
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
                let task = watches.spawn(async move {
                    server
                        .watch_resident_session(row, &lanes, initial, closed)
                        .await
                });
                started.insert(id, (scope, task.id()));
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
                    result = watches.join_next_with_id(), if !watches.is_empty() => {
                        match result {
                            // No timer for a persistently foreign root. Let a
                            // later catalog event give it another chance.
                            Some(Ok((task, Some(id)))) if started.get(&id).is_some_and(|(_, current)| *current == task) => {
                                started.remove(&id);
                            }
                            Some(Err(error)) if !error.is_cancelled() => {
                                warn!(%error, "Resident session watcher ended unexpectedly");
                            }
                            _ => {}
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
    ) -> Option<String> {
        let id = &row.session_db_id;
        if is_job(&row) {
            loop {
                let settled = {
                    let _permit = lanes.setup(historical).await;
                    if closed.has_changed().is_err() {
                        return None;
                    }
                    self.adopt_indexed_job(id).await
                };
                match settled {
                    Ok(true) => return None,
                    Ok(false) => debug!(session_db_id = id, "Indexed job is not yet adoptable"),
                    Err(error) => {
                        debug!(session_db_id = id, %error, "Indexed job could not be adopted")
                    }
                }
                tokio::select! {
                    _ = closed.changed() => return None,
                    _ = tokio::time::sleep(RETRY) => {},
                }
            }
        }
        let mut historical = historical;
        let mut retried_unwritable = false;
        let (session, mut setup) = loop {
            let permit = lanes.setup(historical).await;
            if closed.has_changed().is_err() {
                return None;
            }
            #[cfg(test)]
            if let Some(events) = self.local_adoption_events.get() {
                let _ = events.send(super::LocalAdoptionEvent::Started(id.clone()));
            }
            let result = async {
                if retried_unwritable && !self.registry.local_resident_row_is_active(id).await? {
                    return Ok(None);
                }
                self.subscribe_resident_session(id).await
            }
            .await;
            match result {
                Ok(Some(watch)) => break (watch, Some(permit)),
                Ok(None) if retried_unwritable => return Some(id.clone()),
                Ok(None) => {
                    // The row can arrive ahead of its writable identity.
                    // Recheck this root once, without polling foreign trees.
                    retried_unwritable = true;
                }
                Err(error) => {
                    debug!(session_db_id = id, %error, "Cannot start local session watch; retrying this root")
                }
            }
            drop(permit);
            tokio::select! {
                _ = closed.changed() => return None,
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
        let mut observation = Observation::new(initial.clone());
        let mut frontier = initial;
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
                            frontier = updates.borrow_and_update().clone();
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
                self.probe_resident_session(id, &db, &frontier, &mut observation, lanes)
                    .await
            };
            let delay = match result {
                Ok(active) => {
                    observation.observed.clone_from(&frontier);
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
            frontier = updates.borrow_and_update().clone();
        }
        if !observation.routines.is_empty()
            && let Some(engine) = self.routine_engine()
        {
            engine.deregister_session(id).await;
        }
        None
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
        at: &Snapshot,
        observation: &mut Observation,
        lanes: &ProbeLanes,
    ) -> anyhow::Result<bool> {
        let Observation {
            initial,
            observed,
            proof,
            routines,
        } = observation;
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
            // The first subscription delivery is the in-memory cutoff until
            // a durable legacy proof exists, even if the cache write fails.
            proof
                .as_ref()
                .and_then(|p| p.legacy.as_ref())
                .or(Some(initial)),
            (observed != at).then_some(&*observed),
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
        // Classification succeeded even if persisting it does not. Retain
        // the observation for mention comparison, not as an exact idle proof.
        observed.clone_from(at);
        let cache = lanes.cache.lock().await;
        let txn = cache.new_transaction().await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{EntryType, SessionEntry};
    use crate::types::ConversationId;
    use eidetica::auth::types::{AuthKey, Permission};
    use eidetica::crdt::Doc;

    async fn fixture() -> (
        eidetica::Instance,
        Arc<Server>,
        Arc<crate::session::SessionRegistry>,
    ) {
        super::super::tests::server_fixture_backend(
            Arc::new(crate::tool_host::NativeToolHost::new()),
            Arc::new(crate::test_support::context_hub().await),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn resident_failed_idle_proof_does_not_absorb_later_prompt() {
        let (instance, server, registry) = fixture().await;
        let db = {
            // Create the legacy shape without the registry's turn-schema init.
            let mut user = registry.user_lock().await;
            let key = user.get_default_key().unwrap();
            user.create_database(Doc::new(), &key).await.unwrap()
        };
        let id = db.root_id().to_string();
        registry
            .upsert_session_catalog(&id, Some("tui"), None)
            .await
            .unwrap();
        let mut session = Session::new(ConversationId(id.clone()), db.clone()).await;
        let mut entry = SessionEntry {
            sender: "user".into(),
            content: "old history".into(),
            timestamp: chrono::Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        };
        session.add_entry(entry.clone()).await.unwrap();
        let initial = db.snapshot().await.unwrap();
        let mut observation = Observation::new(initial.clone());
        let peer = registry.chaz_peer();
        let read_only = Database::open(&instance, peer.root_id()).await.unwrap();
        let lanes = ProbeLanes::new(read_only);
        server
            .probe_resident_session(&id, &db, &initial, &mut observation, &lanes)
            .await
            .unwrap_err();
        assert!(observation.proof.is_none());
        let txn = peer.new_transaction().await.unwrap();
        let cached = txn.get_store::<Table<IdleProof>>(IDLE_STORE).await.unwrap();
        assert!(
            cached
                .get(&id)
                .await
                .is_err_and(|error| error.is_not_found())
        );

        entry.content = "new prompt".into();
        let request = session.add_entry(entry).await.unwrap();
        *lanes.cache.lock().await = peer.clone();
        let changed = db.snapshot().await.unwrap();
        assert!(
            server
                .probe_resident_session(&id, &db, &changed, &mut observation, &lanes)
                .await
                .unwrap()
        );
        assert!(server.is_watching_session(&id).await);
        let requests = session
            .turn_requests(|_| false, &HashSet::new())
            .await
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].id, request);
        server.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn resident_rechecks_missing_writer_without_catalog_change() {
        let (_instance, server, registry) = fixture().await;
        let (db, default) = {
            let mut user = registry.user_lock().await;
            let default = user.get_default_key().unwrap();
            let other = user.add_private_key(Some("fixture-owner")).await.unwrap();
            (
                user.create_database(Doc::new(), &other).await.unwrap(),
                default,
            )
        };
        let id = db.root_id().to_string();
        registry
            .upsert_session_catalog(&id, Some("tui"), None)
            .await
            .unwrap();
        let row = registry
            .resident_session_rows()
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.session_db_id == id)
            .unwrap();
        let before = registry.chaz_group().snapshot().await.unwrap();
        assert!(
            registry
                .bind_local_client_session(&id)
                .await
                .unwrap()
                .is_none()
        );

        let lanes = Arc::new(ProbeLanes::new(registry.chaz_peer().clone()));
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        server.local_adoption_events.set(events).unwrap();
        let (scope, closed) = watch::channel(());
        let watch = tokio::spawn({
            let server = server.clone();
            let lanes = lanes.clone();
            async move {
                server
                    .watch_resident_session(row, &lanes, true, closed)
                    .await
            }
        });
        assert!(
            matches!(received.recv().await, Some(super::super::LocalAdoptionEvent::Started(found)) if found == id)
        );
        // Wait for the first bind to release its permit before publishing auth.
        let first_finished = lanes.historical.acquire_many(2).await.unwrap();
        let txn = db.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(&default, AuthKey::active(None, Permission::Write(10)))
            .await
            .unwrap();
        txn.commit().await.unwrap();
        drop(first_finished);
        assert_eq!(registry.chaz_group().snapshot().await.unwrap(), before);
        tokio::time::timeout(RETRY * 2, async {
            loop {
                match received.recv().await.unwrap() {
                    super::super::LocalAdoptionEvent::InitiallyProbed(found) if found == id => {
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("new writer should be watched without another catalog write");
        assert!(
            !server.is_watching_session(&id).await,
            "idle proof must not claim a runtime"
        );
        drop(scope);
        assert!(watch.await.unwrap().is_none());
        server.shutdown().await;
    }
}
