//! Heartbeat support helpers.
//!
//! Per-session heartbeat rules (legacy) live as [`crate::routine::Routine`]
//! rows under the session DB's `routines` table. New schedules live in the
//! agent's own DB (`schedules` store) — agent-owned, not session-scoped.
//!
//! This module retains a single narrow surface: [`sweep_for_agent`],
//! used by `agent_delete` to drop legacy session routines left orphaned
//! when their target agent is unregistered. Agent-owned schedules don't
//! need this — they die with the agent DB.

use crate::extensions::schedule::HeartbeatPayload;
use crate::routine::{list_session_routines, remove_session_routine};
use crate::server::Server;
use std::sync::Arc;

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
