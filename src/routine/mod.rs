// Step 7 of the cap refactor — routine module is a pure addition.
// Nothing in chaz's main yet wires the engine onto a tokio task;
// step 10 (decommission scheduler.rs + heartbeat.rs) is where the
// engine actually starts firing. Allow the un-consumed re-exports
// until then.
#![allow(dead_code, unused_imports)]

//! Routine engine — recurring + one-shot work dispatched against
//! cap-aware extensions.
//!
//! Replaces today's split `scheduler.rs` (global cron) and
//! `heartbeat.rs` (per-session cron + one-shot) runners with a
//! single sleep-until-next engine over a `Trigger` enum. Storage
//! mirrors today's pattern:
//! * Global routines: `chaz_peer.routines` (eidetica `Table<Routine>`).
//! * Per-session routines: each session DB's `routines` table.
//! * Last-fired per routine: `chaz_peer.routine_last_fired` (DocStore).
//!
//! Routes through `Extension::install`'s `routine_handler` slot
//! (added in step 5); step 8 wires the dispatch.

pub mod engine;
pub mod types;

pub use engine::{GLOBAL_ROUTINES_STORE, LAST_FIRED_STORE, RoutineEngine, SESSION_ROUTINES_STORE};
pub use types::{Routine, RoutineId, RoutineScope, RoutineTarget, Trigger, generate_id};
