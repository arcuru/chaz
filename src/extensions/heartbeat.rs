//! Heartbeat extension — per-session cron-driven directives.
//!
//! Wires the heartbeat surface (4 tools + the `/heartbeat` slash command)
//! into the extension hub. Rule storage primitives stay in
//! [`crate::heartbeat`] because they're shared with the background runner
//! (started from `main.rs`) and with `agent_delete`'s cross-session sweep.
//! The extension owns the *user-facing* layer only.

use crate::extension::{
    Extension, ExtensionCommand, ExtensionCommandOutcome, ExtensionHub, HookContext, HookKind,
};
use crate::heartbeat::{HeartbeatRule, list_rules, remove_rule, upsert_rule};
use crate::hosted_index::HostedIndex;
use crate::tools::{HeartbeatAdd, HeartbeatList, HeartbeatModify, HeartbeatRemove};
use cron::Schedule;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

pub struct HeartbeatExtension {
    agent_index: HostedIndex,
}

impl HeartbeatExtension {
    pub fn new(agent_index: HostedIndex) -> Self {
        Self { agent_index }
    }
}

impl Extension for HeartbeatExtension {
    fn name(&self) -> &'static str {
        "heartbeat"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool, HookKind::Command]
    }

    fn register(self: Arc<Self>, hub: &mut ExtensionHub) {
        hub.register_tool(Arc::new(HeartbeatAdd::new(self.agent_index.clone())));
        hub.register_tool(Arc::new(HeartbeatModify::new(self.agent_index.clone())));
        hub.register_tool(Arc::new(HeartbeatRemove));
        hub.register_tool(Arc::new(HeartbeatList::new(self.agent_index.clone())));
        hub.register_command(
            "heartbeat",
            Box::new(HeartbeatCommand {
                agent_index: self.agent_index.clone(),
            }),
        );
    }
}

struct HeartbeatCommand {
    agent_index: HostedIndex,
}

impl ExtensionCommand for HeartbeatCommand {
    fn description(&self) -> &'static str {
        "Manage heartbeat rules on this session — add | remove | list"
    }

    fn invoke<'a>(
        &'a self,
        args: &'a str,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
        Box::pin(async move {
            let trimmed = args.trim();
            let (sub, rest) = match trimmed.split_once(char::is_whitespace) {
                Some((s, r)) => (s.trim(), r.trim()),
                None => (trimmed, ""),
            };
            match sub {
                "" | "list" => list_cmd(ctx).await,
                "remove" | "rm" => {
                    if rest.is_empty() {
                        ExtensionCommandOutcome::Error("Usage: /heartbeat remove <id>".into())
                    } else {
                        remove_cmd(rest, ctx).await
                    }
                }
                "add" => add_cmd(rest, &self.agent_index, ctx).await,
                other => ExtensionCommandOutcome::Error(format!(
                    "Unknown subcommand '/heartbeat {other}'. Use: add | remove | list"
                )),
            }
        })
    }
}

async fn list_cmd(ctx: &HookContext) -> ExtensionCommandOutcome {
    let session = ctx.session.lock().await;
    let db = session.database();
    match list_rules(db).await {
        Ok(rules) if rules.is_empty() => {
            ExtensionCommandOutcome::Text("No heartbeat rules on this session".into())
        }
        Ok(rules) => {
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
            ExtensionCommandOutcome::Text(format!("Heartbeat rules:\n{}", lines.join("\n")))
        }
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to list rules: {e}")),
    }
}

async fn remove_cmd(id: &str, ctx: &HookContext) -> ExtensionCommandOutcome {
    let session = ctx.session.lock().await;
    let db = session.database();
    match remove_rule(db, id).await {
        Ok(true) => ExtensionCommandOutcome::Text(format!("Removed heartbeat rule '{id}'")),
        Ok(false) => ExtensionCommandOutcome::Error(format!("No heartbeat rule with id '{id}'")),
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to remove rule: {e}")),
    }
}

/// Parse and execute `/heartbeat add <id> <sec> <min> <hour> <dom> <mon> <dow> <agent_ref> <task...>`.
/// Cron is six whitespace-separated tokens because that's what the `cron`
/// crate expects; the agent_ref resolves against the hosted index by display
/// name, falling back to DB id parsing.
async fn add_cmd(
    rest: &str,
    agent_index: &HostedIndex,
    ctx: &HookContext,
) -> ExtensionCommandOutcome {
    let mut tokens = rest.split_whitespace();
    let id = tokens.next();
    let c1 = tokens.next();
    let c2 = tokens.next();
    let c3 = tokens.next();
    let c4 = tokens.next();
    let c5 = tokens.next();
    let c6 = tokens.next();
    let agent_ref = tokens.next();
    let task: String = tokens.collect::<Vec<_>>().join(" ");
    let (id, c1, c2, c3, c4, c5, c6, agent_ref) = match (id, c1, c2, c3, c4, c5, c6, agent_ref) {
        (Some(id), Some(a), Some(b), Some(c), Some(d), Some(e), Some(f), Some(ar))
            if !task.is_empty() =>
        {
            (id, a, b, c, d, e, f, ar)
        }
        _ => {
            return ExtensionCommandOutcome::Error(
                "Usage: /heartbeat add <id> <sec> <min> <hour> <dom> <mon> <dow> <agent> <task...>"
                    .into(),
            );
        }
    };
    let cron = format!("{c1} {c2} {c3} {c4} {c5} {c6}");
    if let Err(e) = Schedule::from_str(&cron) {
        return ExtensionCommandOutcome::Error(format!("Invalid cron '{cron}': {e}"));
    }
    let entry = if let Some(e) = agent_index.find_by_name(agent_ref) {
        e
    } else if let Ok(parsed) = eidetica::entry::ID::parse(agent_ref) {
        match agent_index.find_by_id(&parsed) {
            Some(e) => e,
            None => {
                return ExtensionCommandOutcome::Error(format!(
                    "No hosted agent matches '{agent_ref}'"
                ));
            }
        }
    } else {
        return ExtensionCommandOutcome::Error(format!("No hosted agent matches '{agent_ref}'"));
    };
    let rule = HeartbeatRule {
        id: id.to_string(),
        name: id.to_string(),
        cron: cron.clone(),
        task: task.clone(),
        target_agent_db_id: entry.db_id.to_string(),
        enabled: true,
    };
    let session = ctx.session.lock().await;
    let db = session.database();
    match upsert_rule(db, rule).await {
        Ok(()) => ExtensionCommandOutcome::Text(format!(
            "Heartbeat rule '{id}' set: cron='{cron}' → {} — {task}",
            entry.display_name
        )),
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to save rule: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
    use crate::hosted_index::DbEntry;
    use crate::session::{Session, SessionRegistry};
    use crate::types::ConversationId;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use tokio::sync::Mutex;

    async fn fixture() -> (Instance, HostedIndex, HookContext) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents_reg = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(
            SessionRegistry::new(instance.clone(), user, agents_reg)
                .await
                .unwrap(),
        );
        let index = HostedIndex::empty("agent");

        let (agent_db, pubkey) = {
            let mut user = registry.user_for_tests().await;
            create_agent_db(
                &mut user,
                "alpha",
                &AgentDbConfig::default(),
                &AgentMeta {
                    display_name: Some("alpha".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        };
        index.register(DbEntry {
            db_id: agent_db.id(),
            display_name: "alpha".into(),
            pubkey,
        });

        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session =
            Session::new(ConversationId(session_db.root_id().to_string()), session_db).await;
        let ctx = HookContext {
            agent_name: "alpha".into(),
            model: None,
            call_depth: 0,
            session: Arc::new(Mutex::new(session)),
            active_extensions: std::collections::HashSet::new(),
        };
        (instance, index, ctx)
    }

    fn cmd(index: HostedIndex) -> HeartbeatCommand {
        HeartbeatCommand { agent_index: index }
    }

    #[tokio::test]
    async fn add_then_list_round_trips() {
        let (_i, index, ctx) = fixture().await;
        let c = cmd(index);
        let added = c
            .invoke("add five-min 0 */5 * * * * alpha do a thing", &ctx)
            .await;
        match added {
            ExtensionCommandOutcome::Text(s) => assert!(s.contains("five-min"), "got: {s}"),
            ExtensionCommandOutcome::Error(e) => panic!("add failed: {e}"),
        }
        let listed = c.invoke("list", &ctx).await;
        let out = match listed {
            ExtensionCommandOutcome::Text(s) => s,
            ExtensionCommandOutcome::Error(e) => panic!("list failed: {e}"),
        };
        assert!(out.contains("five-min"), "list missing id: {out}");
        assert!(out.contains("0 */5 * * * *"), "list missing cron: {out}");
    }

    #[tokio::test]
    async fn add_rejects_invalid_cron() {
        let (_i, index, ctx) = fixture().await;
        let c = cmd(index);
        let out = c
            .invoke("add bad not a cron at all really alpha do thing", &ctx)
            .await;
        match out {
            ExtensionCommandOutcome::Error(e) => assert!(e.contains("Invalid cron"), "got: {e}"),
            ExtensionCommandOutcome::Text(s) => panic!("expected error, got text: {s}"),
        }
    }

    #[tokio::test]
    async fn empty_args_lists() {
        let (_i, index, ctx) = fixture().await;
        let c = cmd(index);
        match c.invoke("", &ctx).await {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.contains("No heartbeat rules"), "got: {s}")
            }
            ExtensionCommandOutcome::Error(e) => panic!("expected text, got error: {e}"),
        }
    }

    #[tokio::test]
    async fn remove_without_id_is_usage_error() {
        let (_i, index, ctx) = fixture().await;
        let c = cmd(index);
        match c.invoke("remove", &ctx).await {
            ExtensionCommandOutcome::Error(e) => {
                assert!(e.contains("Usage: /heartbeat remove"), "got: {e}")
            }
            ExtensionCommandOutcome::Text(s) => panic!("expected error, got text: {s}"),
        }
    }

    #[tokio::test]
    async fn add_then_remove_then_list_empty() {
        let (_i, index, ctx) = fixture().await;
        let c = cmd(index);
        let _ = c.invoke("add x 0 * * * * * alpha hello", &ctx).await;
        let _ = c.invoke("remove x", &ctx).await;
        match c.invoke("list", &ctx).await {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.contains("No heartbeat rules"), "got: {s}")
            }
            ExtensionCommandOutcome::Error(e) => panic!("list errored: {e}"),
        }
    }

    #[tokio::test]
    async fn unknown_subcommand_is_error() {
        let (_i, index, ctx) = fixture().await;
        let c = cmd(index);
        match c.invoke("frobnicate foo", &ctx).await {
            ExtensionCommandOutcome::Error(e) => {
                assert!(e.contains("Unknown subcommand"), "got: {e}")
            }
            ExtensionCommandOutcome::Text(s) => panic!("expected error, got text: {s}"),
        }
    }
}
