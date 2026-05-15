//! Heartbeat support helpers.
//!
//! Per-session heartbeat rules now live as [`crate::routine::Routine`]
//! rows under the session DB's `routines` table; firing goes through
//! [`crate::routine::RoutineEngine`] once it's spawned (commit E).
//!
//! This module retains two narrow surfaces during the migration:
//!
//! * [`sweep_for_agent`] — used by `agent_delete` to drop routines left
//!   orphaned when their target agent is unregistered. Now operates on
//!   Routine rows whose payload deserializes to [`HeartbeatPayload`].
//! * [`HeartbeatRunner`] — a no-op stub whose `start()` survives only
//!   to keep `main.rs`'s call site compiling until commit F deletes it.

#![allow(dead_code)]

use crate::extensions::heartbeat::HeartbeatPayload;
use crate::routine::{list_session_routines, remove_session_routine};
use crate::server::Server;
use eidetica::Database;
use std::sync::Arc;

/// Per-session heartbeat runner — gutted for the cap-refactor migration
/// window. Routine firing moves to [`crate::routine::RoutineEngine`]
/// once chaz's `main` spawns it (commit E). The struct + its `start()`
/// method survive this commit only so `main.rs`'s call site keeps
/// compiling. Commit F deletes them.
pub struct HeartbeatRunner;

impl HeartbeatRunner {
    pub fn new(_server: Arc<Server>, _chaz_peer: Database) -> Arc<Self> {
        Arc::new(Self)
    }

    pub fn start(self: &Arc<Self>) {
        // Intentionally no-op: per-session heartbeats fire through the
        // routine engine once it's spawned. Rules added between this
        // commit and commit E sit in the routines table until the
        // engine picks them up.
    }
}

/// Walk every known session and remove heartbeat routine rows whose
/// payload targets `target_db_id`. Returns the number removed.
///
/// Used by `agent_delete` to clean up routines left orphaned when their
/// target agent is unregistered. Rows whose payload doesn't deserialize
/// as a heartbeat payload (e.g. some other extension wrote into the
/// session's `routines` table) are skipped, not removed.
pub async fn sweep_for_agent(server: &Arc<Server>, target_db_id: &str) -> usize {
    let sessions = match server.registry().list_sessions().await {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let mut removed = 0usize;
    for idx in &sessions {
        let Ok((_conv, sdb)) = server.registry().open_session(&idx.session_db_id).await else {
            continue;
        };
        let routines = match list_session_routines(&sdb).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        for r in routines {
            let Ok(payload): Result<HeartbeatPayload, _> =
                serde_json::from_value(r.target.payload.clone())
            else {
                continue;
            };
            if payload.target_agent_db_id != target_db_id {
                continue;
            }
            if let Ok(true) = remove_session_routine(&sdb, &r.id).await {
                removed += 1;
            }
        }
    }
    removed
}
