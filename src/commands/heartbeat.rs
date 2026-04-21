//! Heartbeat rule handlers + the `sweep_heartbeat_rules_for_agent`
//! helper used by `agent_delete` (in `super::agent`).

use super::agent::resolve_agent_ref;
use super::{CommandContext, CommandOutcome};

pub(super) async fn heartbeat_add(
    id: &str,
    cron: &str,
    agent_ref: &str,
    task: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    use cron::Schedule;
    use std::str::FromStr;

    if let Err(e) = Schedule::from_str(cron) {
        return CommandOutcome::Error(format!("Invalid cron '{cron}': {e}"));
    }
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    let rule = crate::heartbeat::HeartbeatRule {
        id: id.to_string(),
        name: id.to_string(),
        cron: cron.to_string(),
        task: task.to_string(),
        target_agent_db_id: entry.db_id.to_string(),
        enabled: true,
    };
    match crate::heartbeat::upsert_rule(ctx.session_db, rule).await {
        Ok(()) => CommandOutcome::Text(format!(
            "Heartbeat rule '{id}' set: cron='{cron}' → {} — {task}",
            entry.display_name
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to save rule: {e}")),
    }
}

pub(super) async fn heartbeat_remove(id: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    match crate::heartbeat::remove_rule(ctx.session_db, id).await {
        Ok(true) => CommandOutcome::Text(format!("Removed heartbeat rule '{id}'")),
        Ok(false) => CommandOutcome::Error(format!("No heartbeat rule with id '{id}'")),
        Err(e) => CommandOutcome::Error(format!("Failed to remove rule: {e}")),
    }
}

pub(super) async fn heartbeat_list(ctx: &CommandContext<'_>) -> CommandOutcome {
    let rules = match crate::heartbeat::list_rules(ctx.session_db).await {
        Ok(r) => r,
        Err(e) => return CommandOutcome::Error(format!("Failed to list rules: {e}")),
    };
    if rules.is_empty() {
        return CommandOutcome::Text("No heartbeat rules on this session".to_string());
    }
    let lines: Vec<String> = rules
        .iter()
        .map(|r| {
            let state = if r.enabled { "" } else { " (disabled)" };
            format!(
                "  {} [{}]{state} → {} — {}",
                r.id, r.cron, r.target_agent_db_id, r.task
            )
        })
        .collect();
    CommandOutcome::Text(format!("Heartbeat rules:\n{}", lines.join("\n")))
}

/// Walk every known session and remove heartbeat rules whose target matches
/// `target_db_id`. Returns the number of rules removed. Best-effort per
/// session — errors are logged and skipped.
pub(super) async fn sweep_heartbeat_rules_for_agent(
    ctx: &CommandContext<'_>,
    target_db_id: &str,
) -> usize {
    let sessions = match ctx.server.registry().list_sessions().await {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let mut removed = 0usize;
    for idx in &sessions {
        let Ok((_conv, sdb)) = ctx.server.registry().open_session(&idx.session_db_id).await else {
            continue;
        };
        let rules = match crate::heartbeat::list_rules(&sdb).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        for rule in rules
            .iter()
            .filter(|r| r.target_agent_db_id == target_db_id)
        {
            if let Ok(true) = crate::heartbeat::remove_rule(&sdb, &rule.id).await {
                removed += 1;
            }
        }
    }
    removed
}
