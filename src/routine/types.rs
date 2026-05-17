// Step 7 of the cap refactor — types are pure additions; nothing else
// in chaz uses these yet. Step 9 ports heartbeat onto them; step 10
// decommissions the legacy `scheduler.rs` / `heartbeat.rs`.
#![allow(dead_code)]

//! Routine types — the engine-agnostic data model.
//!
//! A [`Routine`] is a unit of work the [`crate::routine::RoutineEngine`]
//! fires on some trigger. Triggers come in two flavors today
//! ([`Trigger`]): recurring `Cron` and single-shot `OneShot`. Both
//! route through the same [`RoutineTarget`] (an extension name plus
//! an opaque JSON payload) so the engine never inspects the work.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Opaque routine identifier. Caller-provided; the engine treats it
/// as a string token. [`generate_id`] is the convenience for tools
/// that don't want to invent a name (`wake_me_up`, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RoutineId(pub String);

impl RoutineId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RoutineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Helper for callers that don't care about the routine id —
/// produces a unique-enough `<prefix>-<epoch-millis>` token.
/// Uniqueness within a scope is the caller's responsibility; chaz's
/// existing `wake_me_up` already uses the same epoch-suffix pattern.
pub fn generate_id(prefix: &str) -> RoutineId {
    let now = Utc::now().timestamp_millis();
    RoutineId(format!("{prefix}-{now}"))
}

/// What schedule a routine fires on.
///
/// `Cron` re-parses on load — `expr` is stored verbatim and a fresh
/// [`cron::Schedule`] is built each time the engine needs the next
/// fire time. Matches today's `HeartbeatRule` shape; adopting the
/// same storage shape lets the heartbeat migration in step 9 be a
/// renaming operation rather than a re-derivation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Cron { expr: String },
    OneShot { fire_at: DateTime<Utc> },
}

impl Trigger {
    /// `true` for recurring triggers; `false` for one-shots.
    pub fn is_recurring(&self) -> bool {
        matches!(self, Self::Cron { .. })
    }
}

/// Which extension handles this routine, plus the opaque payload the
/// engine passes through verbatim. `extension` is the extension's
/// name as it appears in its manifest — the routine dispatcher (step
/// 8) looks up the installed routine handler under that key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineTarget {
    pub extension: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

fn default_enabled() -> bool {
    true
}
fn default_max_failures() -> u32 {
    3
}

/// One routine — work to fire on a schedule against an extension.
///
/// `consecutive_failures` and `last_error` exist for the engine's
/// failure-handling pass (auto-disable after `max_failures` strikes,
/// step 8 wires the dispatch). `max_failures == 0` opts out of the
/// auto-disable behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Routine {
    pub id: RoutineId,
    pub name: String,
    pub trigger: Trigger,
    pub target: RoutineTarget,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Routine {
    /// Construct a recurring cron routine with default failure handling.
    pub fn cron(
        id: RoutineId,
        name: impl Into<String>,
        expr: impl Into<String>,
        target: RoutineTarget,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            trigger: Trigger::Cron { expr: expr.into() },
            target,
            enabled: true,
            max_failures: 3,
            consecutive_failures: 0,
            last_error: None,
        }
    }

    /// Construct a single-shot routine that fires once at `fire_at`.
    pub fn one_shot(
        id: RoutineId,
        name: impl Into<String>,
        fire_at: DateTime<Utc>,
        target: RoutineTarget,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            trigger: Trigger::OneShot { fire_at },
            target,
            enabled: true,
            max_failures: 3,
            consecutive_failures: 0,
            last_error: None,
        }
    }
}

/// Where this routine lives — the global engine table on the peer DB
/// or one specific session's `rules` table. Used as the in-memory
/// scope tag so removal can wipe routines tied to a session that's
/// going away.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RoutineScope {
    Global,
    Session(String),
    /// Owned by an agent (its DB root id). The routine's authoritative
    /// row lives in that agent's `timers` store, not a session/peer
    /// table — see [`crate::agent_db::Timer`]. Persistence flows
    /// through `AgentDb` + `RoutineEngine::reload_agent`, never the
    /// engine's session/global store path.
    Agent(String),
}

/// Extension name the engine dispatches agent-owned timer fires to.
/// The handler loads the owning agent and invokes it intrinsically.
pub const AGENT_TIMER_EXTENSION: &str = "agent_timer";

/// In-engine payload for an agent-owned timer fire. Carries everything
/// the `agent_timer` routine handler needs to resolve the target,
/// audit the fire, and invoke the owning agent — without re-reading
/// the agent DB.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTimerPayload {
    pub owner_agent_db_id: String,
    pub timer_id: String,
    pub prompt: String,
    /// `crate::agent_db::TimerTarget`, carried as JSON so this type
    /// doesn't depend on the agent_db module.
    pub target: serde_json::Value,
    /// One-shots are removed from the owning agent's `timers` store
    /// after a successful fire (the engine drops the in-memory entry;
    /// the handler must clear the persisted row).
    pub one_shot: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_constructor_carries_default_failure_settings() {
        let r = Routine::cron(
            RoutineId::new("daily"),
            "daily brief",
            "0 0 9 * * *",
            RoutineTarget {
                extension: "heartbeat".into(),
                payload: serde_json::json!({"task": "summarize"}),
            },
        );
        assert!(r.enabled);
        assert_eq!(r.max_failures, 3);
        assert_eq!(r.consecutive_failures, 0);
        assert!(r.last_error.is_none());
        assert!(r.trigger.is_recurring());
    }

    #[test]
    fn one_shot_constructor_marks_non_recurring() {
        let r = Routine::one_shot(
            RoutineId::new("wakeup-1"),
            "wake me",
            Utc::now() + chrono::Duration::seconds(30),
            RoutineTarget {
                extension: "heartbeat".into(),
                payload: serde_json::json!({"task": "check build"}),
            },
        );
        assert!(!r.trigger.is_recurring());
    }

    #[test]
    fn trigger_serde_tags_kind_snake_case() {
        let cron = Trigger::Cron {
            expr: "0 * * * * *".into(),
        };
        let s = serde_json::to_string(&cron).unwrap();
        assert!(s.contains("\"kind\":\"cron\""), "got: {s}");
        assert_eq!(serde_json::from_str::<Trigger>(&s).unwrap(), cron);

        let ts = Utc::now();
        let one = Trigger::OneShot { fire_at: ts };
        let s = serde_json::to_string(&one).unwrap();
        assert!(s.contains("\"kind\":\"one_shot\""), "got: {s}");
        let round: Trigger = serde_json::from_str(&s).unwrap();
        match round {
            Trigger::OneShot { fire_at } => assert_eq!(fire_at, ts),
            other => panic!("expected OneShot, got {other:?}"),
        }
    }

    #[test]
    fn routine_serde_round_trips_with_defaults_skipped() {
        let r = Routine::cron(
            RoutineId::new("r-1"),
            "first",
            "0 * * * * *",
            RoutineTarget {
                extension: "heartbeat".into(),
                payload: serde_json::json!({"x": 1}),
            },
        );
        let s = serde_json::to_string(&r).unwrap();
        // `last_error: None` is skipped on the wire.
        assert!(!s.contains("\"last_error\""), "got: {s}");
        let round: Routine = serde_json::from_str(&s).unwrap();
        assert_eq!(round, r);
    }

    #[test]
    fn generate_id_uses_prefix() {
        let id = generate_id("wakeup");
        assert!(id.as_str().starts_with("wakeup-"));
    }

    #[test]
    fn routine_id_display_returns_inner_string() {
        let id = RoutineId::new("hello");
        assert_eq!(id.to_string(), "hello");
    }
}
