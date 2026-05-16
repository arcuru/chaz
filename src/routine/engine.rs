// Step 7 of the cap refactor — engine skeleton + storage. The
// `run` loop is here but the dispatch path is a no-op TODO that step
// 8 fills in by calling `ExtensionHub::dispatch_routine`.
#![allow(dead_code)]

//! Routine engine — sleep-until-next driver, persistence, and the
//! in-memory min-heap that drives fire ordering.
//!
//! Replaces today's poll-based `HeartbeatRunner` (per-30s tick) and
//! `Scheduler` (separate cron driver) with one engine handling both
//! recurring cron rules and one-shot timers, scoped globally
//! (`chaz_peer.routines`) or per-session (`session_db.rules`).
//!
//! # Phasing
//!
//! Step 7 (this file) covers types, storage, in-memory state,
//! mutators, the sleep-until-next loop, and a dispatch hook that
//! today just records the fire in the failure-handling fields.
//! Step 8 plugs `ExtensionHub::dispatch_routine` into [`fire_due`]'s
//! TODO. Step 9 ports the heartbeat extension. Step 10 deletes
//! `scheduler.rs` + `heartbeat.rs`.
//!
//! # Cross-peer (out of scope — D10)
//!
//! Each peer fires routines independently for now. When session sync
//! lands, routines will need a `fire_on: Identity` field so only one
//! peer fires a given routine on a synced session. See
//! [[scheduling-primitives]] hint #10.

use super::types::{Routine, RoutineId, RoutineScope, Trigger};
use crate::extension::ExtensionHub;
use chrono::{DateTime, Utc};
use cron::Schedule;
use eidetica::Database;
use eidetica::store::{DocStore, Table};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tracing::{error, warn};

/// Eidetica `Table` name on `chaz_peer` that holds global routines.
pub const GLOBAL_ROUTINES_STORE: &str = "routines";
/// Eidetica `Table` name on each session DB that holds per-session
/// routines. Replaces today's `heartbeat_rules` shape.
pub const SESSION_ROUTINES_STORE: &str = "routines";
/// Eidetica `DocStore` name on `chaz_peer` where last-fire timestamps
/// are kept per routine id (RFC-3339 strings). Peer-local — see the
/// cross-peer caveat above.
pub const LAST_FIRED_STORE: &str = "routine_last_fired";

/// Maximum length of `last_error` stored on the routine (longer
/// errors get truncated to keep the eidetica row small).
const MAX_LAST_ERROR_LEN: usize = 256;

/// Hard ceiling on the engine's idle sleep. With monotonic
/// `tokio::time::sleep` a wall-clock jump forward could leave a
/// routine "due now" sleeping for hours. Capping at 5 minutes means
/// we never wait more than 5 min past a real fire time even on a
/// clock jump, at the cost of one wake every 5 min when idle.
const MAX_SLEEP: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// In-memory tracking of one routine — its definition plus where it
/// came from so removal can clean up the right DB.
#[derive(Debug, Clone)]
struct RoutineEntry {
    routine: Routine,
    scope: RoutineScope,
    /// Computed next-fire time. Refreshed after every fire.
    next_fire: DateTime<Utc>,
}

/// Mutable engine state guarded by a single mutex. Heap and routine
/// map move together — heap entries reference routines by id.
struct EngineState {
    heap: BinaryHeap<Reverse<(DateTime<Utc>, RoutineId)>>,
    routines: HashMap<RoutineId, RoutineEntry>,
}

impl EngineState {
    fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            routines: HashMap::new(),
        }
    }

    fn push(&mut self, entry: RoutineEntry) {
        self.heap
            .push(Reverse((entry.next_fire, entry.routine.id.clone())));
        self.routines.insert(entry.routine.id.clone(), entry);
    }

    fn remove(&mut self, id: &RoutineId) -> Option<RoutineEntry> {
        // Heap removal is implicit — `fire_due` skips stale entries
        // by checking against `routines`.
        self.routines.remove(id)
    }

    fn peek_next_fire(&self) -> Option<DateTime<Utc>> {
        self.heap.peek().map(|Reverse((when, _))| *when)
    }
}

/// The routine engine. Constructed at chaz startup with handles to
/// the peer DB (global routines + last-fired state); session-scoped
/// routines flow in via [`Self::register_session`] as sessions open.
pub struct RoutineEngine {
    state: Mutex<EngineState>,
    notify: Arc<Notify>,
    chaz_peer: Database,
    /// Hub the engine dispatches routine fires through. Optional so
    /// step-7 tests can exercise the engine in isolation; production
    /// builds always pass a real hub.
    hub: Option<Arc<ExtensionHub>>,
}

impl RoutineEngine {
    /// Build an engine. Loads global routines from `chaz_peer.routines`,
    /// computes each routine's next fire time, and seeds the heap.
    /// Session routines are added later via [`Self::register_session`].
    ///
    /// `hub` is the extension hub the engine dispatches routine fires
    /// through. Pass `None` only in tests that exercise the engine in
    /// isolation; without a hub, `fire_due` becomes a no-op (the
    /// routine is treated as having succeeded for rescheduling /
    /// one-shot cleanup purposes).
    pub async fn new(
        chaz_peer: Database,
        hub: Option<Arc<ExtensionHub>>,
    ) -> anyhow::Result<Arc<Self>> {
        let engine = Arc::new(Self {
            state: Mutex::new(EngineState::new()),
            notify: Arc::new(Notify::new()),
            chaz_peer,
            hub,
        });
        engine.load_globals().await?;
        Ok(engine)
    }

    async fn load_globals(self: &Arc<Self>) -> anyhow::Result<()> {
        let routines = load_routines_table(&self.chaz_peer, GLOBAL_ROUTINES_STORE).await?;
        let last_fired = load_last_fired(&self.chaz_peer).await;
        let mut state = self.state.lock().await;
        for r in routines {
            if !r.enabled {
                state.routines.insert(
                    r.id.clone(),
                    RoutineEntry {
                        routine: r,
                        scope: RoutineScope::Global,
                        next_fire: DateTime::<Utc>::MAX_UTC,
                    },
                );
                continue;
            }
            let last = last_fired.get(r.id.as_str()).copied();
            match next_fire_time(&r.trigger, last) {
                Some(when) => {
                    let entry = RoutineEntry {
                        routine: r,
                        scope: RoutineScope::Global,
                        next_fire: when,
                    };
                    state.push(entry);
                }
                None => {
                    // Cron expr didn't parse or one-shot is in the past
                    // without a re-fire opportunity. Keep the routine
                    // present but disabled so admin tools can see it.
                    let mut r = r;
                    r.enabled = false;
                    r.last_error = Some(truncate_error(
                        "next_fire_time returned None at startup".into(),
                    ));
                    state.routines.insert(
                        r.id.clone(),
                        RoutineEntry {
                            routine: r,
                            scope: RoutineScope::Global,
                            next_fire: DateTime::<Utc>::MAX_UTC,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Register a session's routines with the engine. Loads the
    /// session DB's `routines` table and inserts every enabled
    /// routine into the heap.
    pub async fn register_session(
        self: &Arc<Self>,
        session_db_id: &str,
        session_db: &Database,
    ) -> anyhow::Result<()> {
        let routines = load_routines_table(session_db, SESSION_ROUTINES_STORE).await?;
        let last_fired = load_last_fired(&self.chaz_peer).await;
        let mut state = self.state.lock().await;
        for r in routines {
            if !r.enabled {
                continue;
            }
            let last = last_fired.get(r.id.as_str()).copied();
            if let Some(when) = next_fire_time(&r.trigger, last) {
                state.push(RoutineEntry {
                    routine: r,
                    scope: RoutineScope::Session(session_db_id.to_string()),
                    next_fire: when,
                });
            }
        }
        drop(state);
        self.notify.notify_one();
        Ok(())
    }

    /// Drop every in-memory routine tied to `session_db_id`. The
    /// session's DB rows are gone with the session — this just
    /// prunes the heap-side state.
    pub async fn deregister_session(self: &Arc<Self>, session_db_id: &str) {
        let scope = RoutineScope::Session(session_db_id.to_string());
        let mut state = self.state.lock().await;
        let to_remove: Vec<RoutineId> = state
            .routines
            .iter()
            .filter(|(_, e)| e.scope == scope)
            .map(|(id, _)| id.clone())
            .collect();
        for id in to_remove {
            state.routines.remove(&id);
        }
        drop(state);
        self.notify.notify_one();
    }

    /// Insert a new routine. Persists to the appropriate DB then
    /// updates in-memory state and wakes the run loop.
    pub async fn add_routine(
        self: &Arc<Self>,
        routine: Routine,
        scope: RoutineScope,
        session_db: Option<&Database>,
    ) -> anyhow::Result<()> {
        let id = routine.id.clone();
        let trigger = routine.trigger.clone();
        let target_db = match &scope {
            RoutineScope::Global => &self.chaz_peer,
            RoutineScope::Session(_) => session_db.ok_or_else(|| {
                anyhow::anyhow!("session-scoped add_routine requires a session DB handle")
            })?,
        };
        let store_name = match scope {
            RoutineScope::Global => GLOBAL_ROUTINES_STORE,
            RoutineScope::Session(_) => SESSION_ROUTINES_STORE,
        };
        upsert_routine(target_db, store_name, &routine).await?;

        let Some(when) = next_fire_time(&trigger, None) else {
            return Err(anyhow::anyhow!(
                "routine {id} has no computable next fire time"
            ));
        };

        let mut state = self.state.lock().await;
        state.push(RoutineEntry {
            routine,
            scope,
            next_fire: when,
        });
        drop(state);
        self.notify.notify_one();
        Ok(())
    }

    /// Drop a routine by id from both memory and the appropriate
    /// store. No-op if the id is unknown.
    pub async fn remove_routine(
        self: &Arc<Self>,
        id: &RoutineId,
        session_db: Option<&Database>,
    ) -> anyhow::Result<()> {
        let entry = {
            let mut state = self.state.lock().await;
            state.remove(id)
        };
        let Some(entry) = entry else {
            return Ok(());
        };
        let (target_db, store_name) = match &entry.scope {
            RoutineScope::Global => (&self.chaz_peer, GLOBAL_ROUTINES_STORE),
            RoutineScope::Session(_) => {
                let Some(db) = session_db else {
                    return Err(anyhow::anyhow!(
                        "session-scoped remove_routine requires a session DB handle"
                    ));
                };
                (db, SESSION_ROUTINES_STORE)
            }
        };
        delete_routine_row(target_db, store_name, id).await?;
        self.notify.notify_one();
        Ok(())
    }

    /// Snapshot of every routine the engine currently knows about,
    /// keyed by scope. Sorted by id for deterministic iteration.
    pub async fn list_routines(self: &Arc<Self>) -> Vec<(RoutineScope, Routine)> {
        let state = self.state.lock().await;
        let mut out: Vec<(RoutineScope, Routine)> = state
            .routines
            .values()
            .map(|e| (e.scope.clone(), e.routine.clone()))
            .collect();
        out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        out
    }

    /// Snapshot of one routine by id, if present.
    pub async fn get(self: &Arc<Self>, id: &RoutineId) -> Option<Routine> {
        let state = self.state.lock().await;
        state.routines.get(id).map(|e| e.routine.clone())
    }

    /// Single iteration of the run loop. Public for tests; the
    /// `run` task drives this in an infinite loop.
    pub async fn tick(self: &Arc<Self>) {
        let target = {
            let state = self.state.lock().await;
            state.peek_next_fire()
        };
        match target {
            Some(when) => {
                let now = Utc::now();
                let delta = (when - now)
                    .to_std()
                    .unwrap_or(std::time::Duration::from_secs(0));
                let cap = delta.min(MAX_SLEEP);
                tokio::select! {
                    _ = tokio::time::sleep(cap) => {
                        self.fire_due().await;
                    }
                    _ = self.notify.notified() => {
                        // Re-evaluate next iteration.
                    }
                }
            }
            None => {
                self.notify.notified().await;
            }
        }
    }

    /// Long-running task: tick forever until the spawning task is
    /// dropped or aborted. Wire this onto a `tokio::spawn` at chaz
    /// startup (step 10).
    pub async fn run(self: Arc<Self>) {
        loop {
            self.tick().await;
        }
    }

    /// Fire every routine whose `next_fire` is `<= now`. Today this
    /// records the fire and either reschedules (Cron) or removes
    /// (OneShot); step 8 wires the actual dispatch through
    /// `ExtensionHub::dispatch_routine`.
    async fn fire_due(self: &Arc<Self>) {
        let now = Utc::now();
        let mut to_fire: Vec<RoutineId> = Vec::new();
        {
            let mut state = self.state.lock().await;
            while let Some(Reverse((when, id))) = state.heap.peek() {
                if *when > now {
                    break;
                }
                let when = *when;
                let id = id.clone();
                state.heap.pop();
                // Skip if the routine was removed or its next-fire
                // moved (we treat the heap entry as stale).
                if let Some(entry) = state.routines.get(&id)
                    && entry.next_fire == when
                {
                    to_fire.push(id);
                }
            }
        }

        for id in to_fire {
            self.fire_one(&id).await;
        }
    }

    async fn fire_one(self: &Arc<Self>, id: &RoutineId) {
        // Snapshot under lock; release before "dispatching".
        let snapshot = {
            let state = self.state.lock().await;
            state.routines.get(id).cloned()
        };
        let Some(entry) = snapshot else {
            return;
        };

        let dispatch_result: anyhow::Result<()> = match &self.hub {
            Some(hub) => {
                hub.dispatch_routine(
                    &entry.routine.target.extension,
                    &entry.scope,
                    entry.routine.target.payload.clone(),
                )
                .await
            }
            None => Ok(()),
        };

        match dispatch_result {
            Ok(()) => self.on_fire_success(id, &entry).await,
            Err(e) => self.on_fire_failure(id, &entry, e).await,
        }
    }

    async fn on_fire_success(self: &Arc<Self>, id: &RoutineId, entry: &RoutineEntry) {
        let now = Utc::now();
        // Persist last_fired for cron recurrence so a restart picks up
        // from "after the last fire" rather than re-firing immediately.
        if entry.routine.trigger.is_recurring()
            && let Err(e) = save_last_fired(&self.chaz_peer, id, now).await
        {
            error!(routine = %id, "failed to persist last_fired: {e}");
        }

        let mut state = self.state.lock().await;
        match entry.routine.trigger {
            Trigger::Cron { .. } => {
                if let Some(when) = next_fire_time(&entry.routine.trigger, Some(now)) {
                    if let Some(e) = state.routines.get_mut(id) {
                        e.routine.consecutive_failures = 0;
                        e.routine.last_error = None;
                        e.next_fire = when;
                    }
                    state.heap.push(Reverse((when, id.clone())));
                } else {
                    // Cron stopped producing future fires — disable
                    // and surface for admin inspection.
                    if let Some(e) = state.routines.get_mut(id) {
                        e.routine.enabled = false;
                    }
                }
            }
            Trigger::OneShot { .. } => {
                // Drop the routine row + in-memory state.
                drop(state);
                let scope = entry.scope.clone();
                if let RoutineScope::Global = scope
                    && let Err(e) =
                        delete_routine_row(&self.chaz_peer, GLOBAL_ROUTINES_STORE, id).await
                {
                    error!(routine = %id, "failed to delete one-shot row: {e}");
                }
                // Session-scoped one-shot row deletion needs the
                // session DB handle — engine doesn't hold one here.
                // Step 8/9 wires that via the hub's session registry.
                let mut state = self.state.lock().await;
                state.routines.remove(id);
            }
        }
    }

    async fn on_fire_failure(
        self: &Arc<Self>,
        id: &RoutineId,
        entry: &RoutineEntry,
        err: anyhow::Error,
    ) {
        let err_string = truncate_error(err.to_string());
        let now = Utc::now();
        let mut state = self.state.lock().await;
        let Some(e) = state.routines.get_mut(id) else {
            return;
        };
        e.routine.consecutive_failures = e.routine.consecutive_failures.saturating_add(1);
        e.routine.last_error = Some(err_string);

        let auto_disable =
            e.routine.max_failures > 0 && e.routine.consecutive_failures >= e.routine.max_failures;

        match entry.routine.trigger {
            Trigger::Cron { .. } if !auto_disable => {
                if let Some(when) = next_fire_time(&entry.routine.trigger, Some(now)) {
                    e.next_fire = when;
                    state.heap.push(Reverse((when, id.clone())));
                }
            }
            Trigger::Cron { .. } => {
                e.routine.enabled = false;
                warn!(
                    routine = %id,
                    failures = e.routine.consecutive_failures,
                    "routine auto-disabled after consecutive failures"
                );
            }
            Trigger::OneShot { .. } => {
                // D21: one-shot failure drops the routine; no retry.
                drop(state);
                if let RoutineScope::Global = entry.scope
                    && let Err(e) =
                        delete_routine_row(&self.chaz_peer, GLOBAL_ROUTINES_STORE, id).await
                {
                    error!(routine = %id, "failed to delete failed one-shot row: {e}");
                }
                let mut state = self.state.lock().await;
                state.routines.remove(id);
            }
        }
    }
}

/// Compute when a routine should next fire.
///
/// * `Cron` — `last` is "the time the routine last fired" (or `None`
///   for never). Returns the next cron time strictly after that
///   anchor, or `None` if the expression is invalid.
/// * `OneShot` — the `fire_at` is returned directly, even if it's in
///   the past; the engine handles already-due routines on the next
///   tick.
fn next_fire_time(trigger: &Trigger, last: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match trigger {
        Trigger::Cron { expr } => {
            let schedule = Schedule::from_str(expr).ok()?;
            match last {
                Some(anchor) => schedule.after(&anchor).next(),
                None => schedule.upcoming(Utc).next(),
            }
        }
        Trigger::OneShot { fire_at } => Some(*fire_at),
    }
}

fn truncate_error(mut s: String) -> String {
    if s.len() > MAX_LAST_ERROR_LEN {
        s.truncate(MAX_LAST_ERROR_LEN);
        s.push_str("...");
    }
    s
}

// =========================================================================
// Storage helpers
// =========================================================================

async fn load_routines_table(db: &Database, store: &str) -> anyhow::Result<Vec<Routine>> {
    let Ok(txn) = db.new_transaction().await else {
        return Ok(Vec::new());
    };
    let Ok(store) = txn.get_store::<Table<Routine>>(store).await else {
        return Ok(Vec::new());
    };
    Ok(store
        .search(|_| true)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(_, r)| r)
        .collect())
}

async fn upsert_routine(db: &Database, store: &str, routine: &Routine) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<Table<Routine>>(store).await?;
    let existing = store.search(|r| r.id == routine.id).await?;
    if let Some((key, _)) = existing.into_iter().next() {
        store.set(&key, routine.clone()).await?;
    } else {
        store.insert(routine.clone()).await?;
    }
    txn.commit().await?;
    Ok(())
}

async fn delete_routine_row(db: &Database, store: &str, id: &RoutineId) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<Table<Routine>>(store).await?;
    let matches = store.search(|r| r.id == *id).await?;
    // Eidetica's Table doesn't expose a delete-by-key today; matching
    // the existing scheduler.rs pattern, mark the row as disabled
    // instead of removing it. Engine load_globals will treat disabled
    // routines as inert.
    if let Some((key, mut row)) = matches.into_iter().next() {
        row.enabled = false;
        store.set(&key, row).await?;
        txn.commit().await?;
    }
    Ok(())
}

async fn load_last_fired(db: &Database) -> HashMap<String, DateTime<Utc>> {
    let out = HashMap::new();
    let Ok(txn) = db.new_transaction().await else {
        return out;
    };
    let Ok(store) = txn.get_store::<DocStore>(LAST_FIRED_STORE).await else {
        return out;
    };
    // DocStore doesn't expose a list/iter today; per-id last_fired
    // lookup happens through callers that hold the id. Returning an
    // empty map seeds the engine on startup with "no prior fires"
    // semantics, which matches today's `heartbeat.rs:248` bootstrap
    // (set last_fired = now on first sight). When DocStore gains an
    // iter, expand this to populate the map up front.
    let _ = store;
    out
}

async fn save_last_fired(db: &Database, id: &RoutineId, when: DateTime<Utc>) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(LAST_FIRED_STORE).await?;
    store.set_string(id.as_str(), when.to_rfc3339()).await?;
    txn.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::types::RoutineTarget;
    use super::*;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;

    async fn fixture_db() -> (Instance, Database) {
        let instance = Instance::open(Box::new(InMemory::new())).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = Doc::new();
        s.set("name", "peer");
        let db = user.create_database(s, &key).await.unwrap();
        (instance, db)
    }

    fn target(ext: &str) -> RoutineTarget {
        RoutineTarget {
            extension: ext.into(),
            payload: serde_json::json!({"task": "ping"}),
        }
    }

    #[tokio::test]
    async fn engine_loads_global_routines_on_startup() {
        let (_inst, peer) = fixture_db().await;
        // Pre-populate one routine, then build the engine.
        let r = Routine::cron(
            RoutineId::new("daily"),
            "daily",
            "0 0 9 * * *",
            target("heartbeat"),
        );
        upsert_routine(&peer, GLOBAL_ROUTINES_STORE, &r)
            .await
            .unwrap();

        let engine = RoutineEngine::new(peer, None).await.unwrap();
        let listed = engine.list_routines().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.id, RoutineId::new("daily"));
    }

    #[tokio::test]
    async fn add_routine_round_trips_through_storage() {
        let (_inst, peer) = fixture_db().await;
        let engine = RoutineEngine::new(peer.clone(), None).await.unwrap();

        let r = Routine::cron(
            RoutineId::new("r-1"),
            "r-1",
            "0 0 9 * * *",
            target("heartbeat"),
        );
        engine
            .add_routine(r.clone(), RoutineScope::Global, None)
            .await
            .unwrap();

        // In-memory state present.
        assert_eq!(engine.list_routines().await.len(), 1);

        // A second engine over the same DB should see it.
        let engine2 = RoutineEngine::new(peer, None).await.unwrap();
        let listed = engine2.list_routines().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.id, RoutineId::new("r-1"));
    }

    #[tokio::test]
    async fn one_shot_in_the_past_fires_on_next_tick() {
        let (_inst, peer) = fixture_db().await;
        let engine = RoutineEngine::new(peer, None).await.unwrap();
        let r = Routine::one_shot(
            RoutineId::new("now"),
            "fire-soon",
            Utc::now() - chrono::Duration::seconds(1),
            target("heartbeat"),
        );
        engine
            .add_routine(r, RoutineScope::Global, None)
            .await
            .unwrap();

        engine.fire_due().await;
        // OneShot fires + drops.
        assert!(engine.get(&RoutineId::new("now")).await.is_none());
    }

    #[tokio::test]
    async fn cron_reschedules_after_fire() {
        let (_inst, peer) = fixture_db().await;
        let engine = RoutineEngine::new(peer, None).await.unwrap();
        // Cron every second so a tick fires it on the test's machine.
        let r = Routine::cron(
            RoutineId::new("tick"),
            "every-sec",
            "* * * * * *",
            target("heartbeat"),
        );
        engine
            .add_routine(r, RoutineScope::Global, None)
            .await
            .unwrap();

        engine.fire_due().await;
        // Still present after firing because cron reschedules.
        assert!(engine.get(&RoutineId::new("tick")).await.is_some());
    }

    #[tokio::test]
    async fn remove_routine_drops_it_from_state() {
        let (_inst, peer) = fixture_db().await;
        let engine = RoutineEngine::new(peer, None).await.unwrap();
        let id = RoutineId::new("doomed");
        engine
            .add_routine(
                Routine::cron(id.clone(), "doomed", "0 * * * * *", target("heartbeat")),
                RoutineScope::Global,
                None,
            )
            .await
            .unwrap();
        engine.remove_routine(&id, None).await.unwrap();
        assert!(engine.get(&id).await.is_none());
    }

    #[tokio::test]
    async fn register_session_loads_session_scoped_routines() {
        let (_inst_peer, peer) = fixture_db().await;
        let (_inst_sess, sess) = fixture_db().await;

        // Seed a session-scoped routine directly via the storage helper.
        upsert_routine(
            &sess,
            SESSION_ROUTINES_STORE,
            &Routine::cron(
                RoutineId::new("session-cron"),
                "in-session",
                "0 * * * * *",
                target("heartbeat"),
            ),
        )
        .await
        .unwrap();

        let engine = RoutineEngine::new(peer, None).await.unwrap();
        engine.register_session("sess-1", &sess).await.unwrap();
        let listed = engine.list_routines().await;
        assert_eq!(listed.len(), 1);
        assert!(matches!(listed[0].0, RoutineScope::Session(ref s) if s == "sess-1"));
    }

    #[tokio::test]
    async fn deregister_session_drops_its_routines() {
        let (_inst_peer, peer) = fixture_db().await;
        let (_inst_sess, sess) = fixture_db().await;
        upsert_routine(
            &sess,
            SESSION_ROUTINES_STORE,
            &Routine::cron(
                RoutineId::new("sr"),
                "sr",
                "0 * * * * *",
                target("heartbeat"),
            ),
        )
        .await
        .unwrap();

        let engine = RoutineEngine::new(peer, None).await.unwrap();
        engine.register_session("sess-x", &sess).await.unwrap();
        assert_eq!(engine.list_routines().await.len(), 1);
        engine.deregister_session("sess-x").await;
        assert!(engine.list_routines().await.is_empty());
    }

    #[tokio::test]
    async fn next_fire_time_for_one_shot_returns_the_target_time() {
        let when = Utc::now() + chrono::Duration::seconds(60);
        let next = next_fire_time(&Trigger::OneShot { fire_at: when }, None).unwrap();
        assert_eq!(next, when);
    }

    #[tokio::test]
    async fn next_fire_time_for_invalid_cron_returns_none() {
        let next = next_fire_time(
            &Trigger::Cron {
                expr: "not a cron".into(),
            },
            None,
        );
        assert!(next.is_none());
    }

    #[tokio::test]
    async fn fire_due_dispatches_through_hub() {
        // End-to-end: one-shot routine + an installed extension whose
        // routine handler records the payload. After `fire_due`, the
        // routine is gone and the payload was observed.
        use crate::extension::{ExtensionHub, HookKind, caps, handler};
        use std::sync::Mutex as StdMutex;

        struct EchoExt {
            seen: Arc<StdMutex<Vec<serde_json::Value>>>,
        }
        impl crate::extension::Extension for EchoExt {
            fn name(&self) -> &'static str {
                "echo"
            }
            fn supported_hooks(&self) -> &[HookKind] {
                &[]
            }
            fn install<'a>(
                &'a self,
                _caps: caps::ExtensionCaps,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = anyhow::Result<handler::InstalledExtension>>
                        + Send
                        + 'a,
                >,
            > {
                let seen = self.seen.clone();
                Box::pin(async move {
                    struct Echo {
                        seen: Arc<StdMutex<Vec<serde_json::Value>>>,
                    }
                    impl handler::RoutineHandler for Echo {
                        fn on_fire<'a>(
                            &'a self,
                            _caps: &'a caps::ExtensionCaps,
                            payload: serde_json::Value,
                        ) -> handler::HandlerFuture<'a, anyhow::Result<()>>
                        {
                            let seen = self.seen.clone();
                            Box::pin(async move {
                                seen.lock().unwrap().push(payload);
                                Ok(())
                            })
                        }
                    }
                    let mut i = handler::InstalledExtension::empty();
                    i.routine_handler = Some(Box::new(Echo { seen }));
                    Ok(i)
                })
            }
        }

        let mut hub = ExtensionHub::new();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        hub.install_all(vec![Arc::new(EchoExt { seen: seen.clone() })])
            .await
            .unwrap();
        let hub = Arc::new(hub);

        let (_inst, peer) = fixture_db().await;
        let engine = RoutineEngine::new(peer, Some(hub)).await.unwrap();

        let r = Routine::one_shot(
            RoutineId::new("now"),
            "now",
            Utc::now() - chrono::Duration::seconds(1),
            RoutineTarget {
                extension: "echo".into(),
                payload: serde_json::json!({"task": "echo me"}),
            },
        );
        engine
            .add_routine(r, RoutineScope::Global, None)
            .await
            .unwrap();

        engine.fire_due().await;
        // Drop the lock before any subsequent await to keep clippy's
        // `await_holding_lock` happy.
        let recorded = seen.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], serde_json::json!({"task": "echo me"}));
        // One-shot dropped after firing.
        assert!(engine.get(&RoutineId::new("now")).await.is_none());
    }

    #[test]
    fn truncate_error_caps_at_max_len() {
        let long = "x".repeat(MAX_LAST_ERROR_LEN + 100);
        let out = truncate_error(long);
        assert!(out.len() <= MAX_LAST_ERROR_LEN + 3);
        assert!(out.ends_with("..."));
    }
}
