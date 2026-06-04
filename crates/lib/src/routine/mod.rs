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
    }
    Ok(removed)
}
