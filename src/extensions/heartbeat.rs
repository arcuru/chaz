//! Heartbeat extension — per-session cron-driven directives.
//!
//! Wires the heartbeat surface (4 tools + the `/heartbeat` slash command)
//! into the extension hub. Rule storage primitives stay in
//! [`crate::heartbeat`] because they're shared with the background runner
//! (started from `main.rs`) and with `agent_delete`'s cross-session sweep.
//! The extension owns the *user-facing* layer only.

use crate::extension::caps::{
    CapabilityRequest, CommandDescriptor, ExtensionCaps, SessionEntryDraft,
};
use crate::extension::handler::{HandlerFuture, InstalledExtension, RoutineHandler};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{
    Extension, ExtensionCommand, ExtensionCommandOutcome, ExtensionRef, HookContext, HookKind,
};
use crate::hosted_index::HostedIndex;
use crate::routine::{
    Routine, RoutineId, RoutineTarget, Trigger, list_session_routines, remove_session_routine,
    upsert_session_routine,
};
use crate::tools::{HeartbeatAdd, HeartbeatList, HeartbeatModify, HeartbeatRemove, WakeMeUp};
use chrono::Utc;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use tracing::debug;

/// Routine payload for heartbeat fires.
///
/// Carried verbatim inside `Routine.target.payload` by the routine
/// engine; the engine never inspects it. The handler reads
/// `target_agent_db_id` to decide whether this peer hosts the target
/// agent (silently skipping if not), formats the directive body, and
/// appends through `caps.session_write`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatPayload {
    /// Human-friendly name shown in the directive body ("Heartbeat
    /// '{rule_name}' at …"). The routine's own `id`/`name` lives on
    /// the `Routine`, not in the payload.
    pub rule_name: String,
    /// Eidetica entry id of the agent this fire targets. The handler
    /// silently skips fires whose target isn't hosted on this peer.
    pub target_agent_db_id: String,
    /// Task text appended to the directive body.
    pub task: String,
    /// `true` for one-shot wakeups (wording: "Wakeup …") and `false`
    /// for recurring cron rules (wording: "Heartbeat …"). One-shot
    /// row deletion is the engine's concern; this field is purely
    /// presentational.
    #[serde(default)]
    pub is_one_shot: bool,
}

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

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![HookKind::Tool, HookKind::Command],
            required_capabilities: vec![
                CapabilityRequest::ToolRegistration,
                CapabilityRequest::CommandRegistration,
                CapabilityRequest::SessionWrite,
            ],
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn install<'a>(
        &'a self,
        caps: ExtensionCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<InstalledExtension>> + Send + 'a>> {
        Box::pin(async move {
            let tool_reg = caps.tool_registration.as_ref().ok_or_else(|| {
                anyhow::anyhow!("heartbeat install requires ToolRegistration cap")
            })?;
            let cmd_reg = caps.command_registration.as_ref().ok_or_else(|| {
                anyhow::anyhow!("heartbeat install requires CommandRegistration cap")
            })?;

            let tools: Vec<Arc<dyn crate::tool::Tool>> = vec![
                Arc::new(HeartbeatAdd::new(self.agent_index.clone())),
                Arc::new(HeartbeatModify::new(self.agent_index.clone())),
                Arc::new(HeartbeatRemove),
                Arc::new(HeartbeatList::new(self.agent_index.clone())),
                Arc::new(WakeMeUp::new(self.agent_index.clone())),
            ];
            for t in tools {
                let d = t.descriptor();
                tool_reg.register(d, t).await?;
            }

            cmd_reg
                .register(
                    CommandDescriptor {
                        name: "heartbeat".into(),
                        description: "Manage heartbeat rules on this session — add | remove | list"
                            .into(),
                    },
                    Box::new(HeartbeatCommand {
                        agent_index: self.agent_index.clone(),
                    }),
                )
                .await?;

            let mut installed = InstalledExtension::empty();
            installed.routine_handler = Some(Box::new(HeartbeatRoutineHandler {
                agent_index: self.agent_index.clone(),
            }));
            Ok(installed)
        })
    }
}

/// Routine handler for cron + one-shot heartbeat fires.
///
/// The engine times the fire and reschedules / drops the routine; this
/// handler's job is just: (a) decide whether this peer hosts the
/// targeted agent (silently skip otherwise), (b) format the directive
/// body, (c) append it through the per-session SessionWrite cap.
pub struct HeartbeatRoutineHandler {
    agent_index: HostedIndex,
}

impl RoutineHandler for HeartbeatRoutineHandler {
    fn on_fire<'a>(
        &'a self,
        caps: &'a ExtensionCaps,
        payload: serde_json::Value,
    ) -> HandlerFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let payload: HeartbeatPayload = serde_json::from_value(payload)
                .map_err(|e| anyhow::anyhow!("invalid heartbeat payload: {e}"))?;

            // Silently skip if this peer doesn't host the target agent —
            // matches today's `HeartbeatRunner::maybe_fire` behavior so
            // multi-peer setups don't double-fire (the rule's owning
            // peer will write the directive).
            let Ok(id) = eidetica::entry::ID::parse(&payload.target_agent_db_id) else {
                debug!(
                    target = %payload.target_agent_db_id,
                    "heartbeat target_agent_db_id unparseable; skipping"
                );
                return Ok(());
            };
            if self.agent_index.find_by_id(&id).is_none() {
                return Ok(());
            }

            let writer = caps.session_write.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "heartbeat routine fire without session_write cap — \
                     dispatcher must build a session-scoped bundle"
                )
            })?;
            let now = Utc::now();
            let verb = if payload.is_one_shot {
                "Wakeup"
            } else {
                "Heartbeat"
            };
            let content = format!(
                "{verb} '{}' at {}.\n\n{}",
                payload.rule_name,
                now.format("%Y-%m-%d %H:%M:%S UTC"),
                payload.task,
            );
            writer
                .append(SessionEntryDraft {
                    kind: "directive".into(),
                    data: serde_json::Value::String(content),
                })
                .await?;
            Ok(())
        })
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
    match list_session_routines(db).await {
        Ok(rs) if rs.is_empty() => {
            ExtensionCommandOutcome::Text("No heartbeat rules on this session".into())
        }
        Ok(routines) => {
            let lines: Vec<String> = routines
                .iter()
                .map(|r| {
                    let p: HeartbeatPayload = serde_json::from_value(r.target.payload.clone())
                        .unwrap_or(HeartbeatPayload {
                            rule_name: r.name.clone(),
                            target_agent_db_id: String::new(),
                            task: String::new(),
                            is_one_shot: matches!(r.trigger, Trigger::OneShot { .. }),
                        });
                    let state = if r.enabled { "" } else { " (disabled)" };
                    let schedule = match &r.trigger {
                        Trigger::Cron { expr } => expr.clone(),
                        Trigger::OneShot { fire_at } => {
                            format!("@{}", fire_at.format("%Y-%m-%d %H:%M:%SZ"))
                        }
                    };
                    format!(
                        "  {} [{schedule}]{state} → {} — {}",
                        r.id, p.target_agent_db_id, p.task
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
    match remove_session_routine(db, &RoutineId::new(id)).await {
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
    let payload = HeartbeatPayload {
        rule_name: id.to_string(),
        target_agent_db_id: entry.db_id.to_string(),
        task: task.clone(),
        is_one_shot: false,
    };
    let payload_value = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            return ExtensionCommandOutcome::Error(format!("Failed to encode payload: {e}"));
        }
    };
    let routine = Routine::cron(
        RoutineId::new(id),
        id,
        cron.clone(),
        RoutineTarget {
            extension: "heartbeat".into(),
            payload: payload_value,
        },
    );
    let session = ctx.session.lock().await;
    let db = session.database();
    match upsert_session_routine(db, &routine).await {
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

    // ---- RoutineHandler coverage (cap refactor step 9) ------------------

    /// Bring up a registry + one agent + one session, then build an
    /// extension caps bundle whose `session_write` points at that
    /// session. Returns the wiring the handler tests need.
    async fn handler_fixture() -> (
        Instance,
        HostedIndex,
        Arc<SessionRegistry>,
        String, // session_db_id
        ExtensionCaps,
        eidetica::entry::ID, // hosted agent id
    ) {
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
        let agent_id = agent_db.id();
        index.register(DbEntry {
            db_id: agent_id.clone(),
            display_name: "alpha".into(),
            pubkey,
        });

        let (conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_db_id = session_db.root_id().to_string();
        let session = Arc::new(Mutex::new(Session::new(conv, session_db).await));
        let mut caps = ExtensionCaps::empty();
        caps.session_write = Some(Arc::new(
            crate::extension::caps_inproc::InProcSessionWrite::new(session.clone(), "heartbeat"),
        ));
        (instance, index, registry, session_db_id, caps, agent_id)
    }

    #[tokio::test]
    async fn routine_handler_writes_directive_for_hosted_agent() {
        let (_i, index, registry, session_db_id, caps, agent_id) = handler_fixture().await;
        let handler = HeartbeatRoutineHandler { agent_index: index };
        let payload = serde_json::to_value(HeartbeatPayload {
            rule_name: "morning-brief".into(),
            target_agent_db_id: agent_id.to_string(),
            task: "summarize overnight".into(),
            is_one_shot: false,
        })
        .unwrap();
        handler.on_fire(&caps, payload).await.unwrap();

        // Re-open the session and verify the directive landed.
        let (conv, db) = registry.open_session(&session_db_id).await.unwrap();
        let s = Session::new(conv, db).await;
        let entries = s.entries();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(matches!(e.entry_type, crate::session::EntryType::Directive));
        assert_eq!(e.sender, "heartbeat");
        assert!(
            e.content.starts_with("Heartbeat 'morning-brief' at "),
            "got: {}",
            e.content
        );
        assert!(
            e.content.contains("summarize overnight"),
            "got: {}",
            e.content
        );
    }

    #[tokio::test]
    async fn routine_handler_one_shot_uses_wakeup_verb() {
        let (_i, index, registry, session_db_id, caps, agent_id) = handler_fixture().await;
        let handler = HeartbeatRoutineHandler { agent_index: index };
        let payload = serde_json::to_value(HeartbeatPayload {
            rule_name: "ping-build".into(),
            target_agent_db_id: agent_id.to_string(),
            task: "check the build".into(),
            is_one_shot: true,
        })
        .unwrap();
        handler.on_fire(&caps, payload).await.unwrap();
        let (conv, db) = registry.open_session(&session_db_id).await.unwrap();
        let s = Session::new(conv, db).await;
        let e = &s.entries()[0];
        assert!(
            e.content.starts_with("Wakeup 'ping-build' at "),
            "got: {}",
            e.content
        );
    }

    #[tokio::test]
    async fn routine_handler_silently_skips_non_hosted_agent() {
        let (_i, index, registry, session_db_id, caps, _agent_id) = handler_fixture().await;
        let handler = HeartbeatRoutineHandler { agent_index: index };
        // Use a different (well-formed but not hosted) agent id.
        let other = crate::agent_db::AgentDbConfig::default();
        let _ = other; // silence unused — we just need a parseable id below
        let payload = serde_json::to_value(HeartbeatPayload {
            rule_name: "ghost".into(),
            // Reuse the session id as a parseable but non-agent id —
            // it's a valid eidetica entry id that find_by_id will miss.
            target_agent_db_id: session_db_id.clone(),
            task: "do nothing".into(),
            is_one_shot: false,
        })
        .unwrap();
        handler.on_fire(&caps, payload).await.unwrap();
        let (conv, db) = registry.open_session(&session_db_id).await.unwrap();
        let s = Session::new(conv, db).await;
        assert!(
            s.entries().is_empty(),
            "non-hosted fire should not write: {:?}",
            s.entries()
        );
    }

    #[tokio::test]
    async fn routine_handler_rejects_payload_without_session_write() {
        let (_i, index, _registry, _id, _caps, agent_id) = handler_fixture().await;
        let handler = HeartbeatRoutineHandler { agent_index: index };
        let payload = serde_json::to_value(HeartbeatPayload {
            rule_name: "x".into(),
            target_agent_db_id: agent_id.to_string(),
            task: "x".into(),
            is_one_shot: false,
        })
        .unwrap();
        let empty_caps = ExtensionCaps::empty();
        let err = handler.on_fire(&empty_caps, payload).await.unwrap_err();
        assert!(err.to_string().contains("session_write"), "got: {err}");
    }

    #[tokio::test]
    async fn routine_handler_rejects_malformed_payload() {
        let (_i, index, _registry, _id, caps, _agent_id) = handler_fixture().await;
        let handler = HeartbeatRoutineHandler { agent_index: index };
        let err = handler
            .on_fire(&caps, serde_json::json!({"not": "a payload"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid heartbeat payload"),
            "got: {err}"
        );
    }
}
