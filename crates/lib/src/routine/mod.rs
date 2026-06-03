#![allow(dead_code, unused_imports)]

//! Routine engine — recurring + one-shot work dispatched against
//! cap-aware extensions.
//!
//! Single sleep-until-next engine over a `Trigger` enum, covering both
//! global cron and per-session cron + one-shot. Storage:
//! * Global routines: `chaz_peer.routines` (eidetica `Table<Routine>`).
//! * Per-session routines: each session DB's `routines` table.
//! * Last-fired per routine: `chaz_peer.routine_last_fired` (DocStore).
//!
//! Routes through `Extension::install`'s `routine_handler` slot;
//! dispatch goes via `ExtensionHub::dispatch_routine`.

pub mod engine;
pub mod types;

pub use engine::{GLOBAL_ROUTINES_STORE, LAST_FIRED_STORE, RoutineEngine, SESSION_ROUTINES_STORE};
pub use types::{
    AGENT_SCHEDULE_EXTENSION, AgentSchedulePayload, Routine, RoutineId, RoutineScope,
    RoutineTarget, Trigger, generate_id,
};

use eidetica::Database;
use eidetica::store::Table;
use std::sync::{Arc, OnceLock, Weak};
use tracing::warn;

/// Process-global handle to the running [`RoutineEngine`], set once at
/// startup (`main.rs`, non-`--print`). Stored as a `Weak` so the engine's
/// lifetime stays owned by its run task; the helpers below upgrade on
/// demand and no-op when there is no engine (`--print`, tests, shutdown).
///
/// This is the seam that makes scheduling live: every routine mutation
/// funnels through [`upsert_session_routine`] / [`remove_session_routine`]
/// (tools, the `/schedule` command, `schedule_once`, `agent_delete`'s
/// sweep), so resyncing the engine here covers all of them — including
/// future callers — without threading an engine handle through every
/// tool/command context.
static ENGINE: OnceLock<Weak<RoutineEngine>> = OnceLock::new();

/// Register the running engine so the storage helpers can push live
/// changes into its in-memory schedule. Idempotent — first call wins
/// (one engine per process).
pub fn set_engine(engine: &Arc<RoutineEngine>) {
    let _ = ENGINE.set(Arc::downgrade(engine));
}

fn engine() -> Option<Arc<RoutineEngine>> {
    ENGINE.get().and_then(Weak::upgrade)
}

/// Resync a session's in-memory schedule after a committed change to
/// its `routines` table. Best-effort: a reload failure is logged, not
/// propagated — the durable DB write already succeeded.
async fn notify_session_routines_changed(session_db: &Database) {
    let Some(engine) = engine() else {
        return;
    };
    let session_db_id = session_db.root_id().to_string();
    if let Err(e) = engine.reload_session(&session_db_id, session_db).await {
        warn!(session = %session_db_id, "routine engine reload after change failed: {e}");
    }
}

/// Drop a closed session's routines from the running engine's heap.
/// Called from `Server::deregister_session`.
pub async fn notify_session_closed(session_db_id: &str) {
    if let Some(engine) = engine() {
        engine.deregister_session(session_db_id).await;
    }
}

/// Resync one agent's schedules from its DB into the running engine's
/// heap after a committed change (add/remove/edit). Best-effort:
/// a reload failure is logged, not propagated — the durable DB write
/// already succeeded.
pub async fn notify_agent_schedules_changed(
    agent_db_id: &str,
    agent_db: &crate::agent_db::AgentDb,
) {
    let Some(engine) = engine() else {
        return;
    };
    if let Err(e) = engine.reload_agent(agent_db_id, agent_db).await {
        warn!(agent = %agent_db_id, "routine engine reload_agent after schedule change failed: {e}");
    }
}

/// List every routine row in a session DB's `routines` table.
///
/// Returns `Ok(Vec::new())` when the table doesn't exist yet (a session
/// that's never had a routine written to it).
pub async fn list_session_routines(session_db: &Database) -> anyhow::Result<Vec<Routine>> {
    let Ok(txn) = session_db.new_transaction().await else {
        return Ok(Vec::new());
    };
    let Ok(store) = txn
        .get_store::<Table<Routine>>(SESSION_ROUTINES_STORE)
        .await
    else {
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

/// Upsert a routine into a session DB's `routines` table, keyed by id.
pub async fn upsert_session_routine(
    session_db: &Database,
    routine: &Routine,
) -> anyhow::Result<()> {
    let txn = session_db.new_transaction().await?;
    let store = txn
        .get_store::<Table<Routine>>(SESSION_ROUTINES_STORE)
        .await?;
    let existing = store.search(|r| r.id == routine.id).await?;
    if let Some((key, _)) = existing.into_iter().next() {
        store.set(&key, routine.clone()).await?;
    } else {
        store.insert(routine.clone()).await?;
    }
    txn.commit().await?;
    notify_session_routines_changed(session_db).await;
    Ok(())
}

/// Delete a routine row from a session DB's `routines` table.
/// Returns `Ok(true)` when a matching row was removed, `Ok(false)`
/// when no such id exists.
pub async fn remove_session_routine(session_db: &Database, id: &RoutineId) -> anyhow::Result<bool> {
    let txn = session_db.new_transaction().await?;
    let store = txn
        .get_store::<Table<Routine>>(SESSION_ROUTINES_STORE)
        .await?;
    let matches = store.search(|r| r.id == *id).await?;
    let mut removed = false;
    for (key, _) in matches {
        if store.delete(&key).await? {
            removed = true;
        }
    }
    if removed {
        txn.commit().await?;
        notify_session_routines_changed(session_db).await;
    }
    Ok(removed)
}
