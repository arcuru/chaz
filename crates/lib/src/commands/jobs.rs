//! Read-only inventory of the running daemon's routine engine.

use super::{CommandContext, CommandOutcome};
use crate::routine::{AgentSchedulePayload, Routine, RoutineScope, Trigger};

pub(super) async fn list(ctx: &CommandContext<'_>) -> CommandOutcome {
    let Some(engine) = ctx.server.routine_engine() else {
        return CommandOutcome::Error(
            "Schedule engine unavailable: run /jobs on the executor after schedule startup. Client and one-shot processes do not run an engine.".into(),
        );
    };
    CommandOutcome::Text(render(&engine.list_routines().await))
}

fn render(routines: &[(RoutineScope, Routine)]) -> String {
    let mut lines = vec![format!("Scheduled jobs ({}):", routines.len())];
    for (scope, routine) in routines {
        let scope = match scope {
            RoutineScope::Global => "global".into(),
            RoutineScope::Session(id) => format!("session:{id}"),
            RoutineScope::Agent(id) => format!("agent:{id}"),
        };
        let trigger = match &routine.trigger {
            Trigger::Cron { expr } => format!("cron {expr}"),
            Trigger::Interval { period } => format!("every {period:?}"),
            Trigger::OneShot { fire_at } => format!("@{}", fire_at.to_rfc3339()),
        };
        let target = serde_json::from_value::<AgentSchedulePayload>(routine.target.payload.clone())
            .ok()
            .and_then(|payload| {
                serde_json::from_value::<crate::agent_db::ScheduleTarget>(payload.target).ok()
            })
            .map(|target| match target {
                crate::agent_db::ScheduleTarget::Pinned { session_db_id } => {
                    format!("pinned:{session_db_id}")
                }
                crate::agent_db::ScheduleTarget::Fresh => "fresh".into(),
            })
            .unwrap_or_else(|| routine.target.payload.to_string());
        lines.push(format!(
            "- {} scope={scope} [{trigger}] enabled={} permanent={} extension={} target={target}",
            routine.id, routine.enabled, routine.permanent, routine.target.extension,
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::{RoutineId, RoutineTarget};
    use std::time::Duration;

    #[test]
    fn jobs_view_shows_all_scopes_and_flags() {
        let mut rows = Vec::new();
        for (scope, id, permanent, enabled) in [
            (RoutineScope::Global, "global-job", false, true),
            (
                RoutineScope::Session("session".into()),
                "session-job",
                true,
                false,
            ),
            (
                RoutineScope::Agent("owner".into()),
                "agent:owner:poll",
                true,
                true,
            ),
        ] {
            let mut routine = Routine::interval(
                RoutineId::new(id),
                "poll",
                Duration::from_secs(300),
                RoutineTarget {
                    extension: "agent_schedule".into(),
                    payload: serde_json::json!({
                        "owner_agent_db_id": "owner", "schedule_id": "poll", "prompt": "check",
                        "target": {"kind": "pinned", "session_db_id": "target"}, "one_shot": false,
                    }),
                },
            );
            routine.permanent = permanent;
            routine.enabled = enabled;
            rows.push((scope, routine));
        }
        assert_eq!(
            render(&rows),
            "Scheduled jobs (3):\n- global-job scope=global [every 300s] enabled=true permanent=false extension=agent_schedule target=pinned:target\n- session-job scope=session:session [every 300s] enabled=false permanent=true extension=agent_schedule target=pinned:target\n- agent:owner:poll scope=agent:owner [every 300s] enabled=true permanent=true extension=agent_schedule target=pinned:target"
        );
        assert_eq!(render(&[]), "Scheduled jobs (0):");
    }

    #[test]
    fn jobs_is_a_builtin_command() {
        assert!(matches!(
            super::super::parse("/jobs"),
            super::super::Parsed::Command(super::super::Command::Jobs)
        ));
        assert!(super::super::BUILTIN_COMMAND_NAMES.contains(&"jobs"));
    }
}
