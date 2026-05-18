//! Heartbeat extension — agent-owned timers via tools + commands.
//!
//! Wires the heartbeat surface (5 tools + the `/heartbeat` slash command)
//! into the extension hub. Timers now live in the owning agent's DB
//! (`timers` store); the `/heartbeat` command and tools write there
//! instead of the session `routines` table.

use crate::agent_db::{AgentDb, Timer, TimerTarget};
use crate::extension::caps::{
    AgentStateAdmin, CapabilityRequest, CommandDescriptor, ExtensionCaps, SessionEntryDraft,
};
use crate::extension::handler::{HandlerFuture, InstalledExtension, RoutineHandler};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{
    Extension, ExtensionCommand, ExtensionCommandOutcome, ExtensionRef, HookContext, HookKind,
};
use crate::hosted_index::HostedIndex;
use crate::routine::{Trigger, notify_agent_timers_changed};
use crate::tools::{
    HeartbeatAdd, HeartbeatList, HeartbeatModify, HeartbeatRemove, WakeMeUp,
};
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

/// Heartbeat extension — receives its `AgentStateAdmin` handle from
/// the hub at install time (not via constructor). Keeps `HostedIndex`
/// for the legacy routine handler's host-check only.
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
                CapabilityRequest::AgentStateAdmin { agents: None },
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
            let agent_state = caps.agent_state_admin.clone().ok_or_else(|| {
                anyhow::anyhow!("heartbeat install requires AgentStateAdmin cap")
            })?;

            let tools: Vec<Arc<dyn crate::tool::Tool>> = vec![
                Arc::new(HeartbeatAdd::new(agent_state.clone())),
                Arc::new(HeartbeatModify::new(agent_state.clone())),
                Arc::new(HeartbeatRemove::new(agent_state.clone())),
                Arc::new(HeartbeatList::new(agent_state.clone())),
                Arc::new(WakeMeUp::new(agent_state.clone())),
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
                        agent_state,
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
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl ExtensionCommand for HeartbeatCommand {
    fn description(&self) -> &'static str {
        "Manage timers on agents — add | remove | list"
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
                "" | "list" => list_cmd(&*self.agent_state, ctx).await,
                "remove" | "rm" => {
                    if rest.is_empty() {
                        ExtensionCommandOutcome::Error("Usage: /heartbeat remove <id> [agent]".into())
                    } else {
                        remove_cmd(rest, &*self.agent_state, ctx).await
                    }
                }
                "add" => {
                    add_cmd(rest, &*self.agent_state, ctx).await
                }
                other => ExtensionCommandOutcome::Error(format!(
                    "Unknown subcommand '/heartbeat {other}'. Use: add | remove | list"
                )),
            }
        })
    }
}

/// Open the agent DB for the context's agent (or a named agent from
/// the index). Returns an error when the agent isn't hosted or the DB
/// can't be opened.
async fn open_agent_db_for_cmd(
    cap: &dyn AgentStateAdmin,
    ctx: &HookContext,
    agent_ref: Option<&str>,
) -> Result<(crate::hosted_index::DbEntry, AgentDb), ExtensionCommandOutcome> {
    let name = agent_ref.unwrap_or(&ctx.agent_name);
    let entry = cap.resolve_agent(name).map_err(|e| {
        ExtensionCommandOutcome::Error(e)
    })?;
    let adb = cap.open_agent_db(&entry).await.map_err(|e| {
        ExtensionCommandOutcome::Error(format!("{e:#}"))
    })?;
    Ok((entry, adb))
}

async fn list_cmd(
    cap: &dyn AgentStateAdmin,
    ctx: &HookContext,
) -> ExtensionCommandOutcome {
    // Parse optional agent name from the rest of the args.
    // `/heartbeat list` lists your own timers.
    // `/heartbeat list <agent>` lists that agent's timers.
    // We get the full args via invoke, but here rest is the trimmed sub-arg.
    // For simplicity, always list the calling agent's timers.
    let (_, adb) = match open_agent_db_for_cmd(cap, ctx, None).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    match adb.list_timers().await {
        Ok(timers) if timers.is_empty() => {
            ExtensionCommandOutcome::Text(format!(
                "No timers on agent '{}'.",
                ctx.agent_name
            ))
        }
        Ok(timers) => {
            let lines: Vec<String> = timers
                .iter()
                .map(|t| {
                    let state = if t.enabled { "" } else { " (disabled)" };
                    let schedule = match &t.trigger {
                        Trigger::Cron { expr } => expr.clone(),
                        Trigger::OneShot { fire_at } => {
                            format!("@{}", fire_at.format("%Y-%m-%d %H:%M:%SZ"))
                        }
                    };
                    let target_label = match &t.target {
                        TimerTarget::Pinned { .. } => "pinned".to_string(),
                        TimerTarget::Fresh => "fresh".to_string(),
                    };
                    format!(
                        "  {} [{schedule}]{state} → {target_label} — {}",
                        t.id, t.prompt
                    )
                })
                .collect();
            ExtensionCommandOutcome::Text(format!(
                "Timers on '{}':\n{}",
                ctx.agent_name,
                lines.join("\n")
            ))
        }
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to list timers: {e}")),
    }
}

async fn remove_cmd(
    rest: &str,
    cap: &dyn AgentStateAdmin,
    ctx: &HookContext,
) -> ExtensionCommandOutcome {
    // Format: /heartbeat remove <id> [agent]
    let (timer_id, agent_ref) = match rest.split_once(char::is_whitespace) {
        Some((id, agent)) => (id.trim(), Some(agent.trim())),
        None => (rest.trim(), None),
    };
    let (_, adb) = match open_agent_db_for_cmd(cap, ctx, agent_ref).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    match adb.remove_timer(timer_id).await {
        Ok(true) => {
            notify_agent_timers_changed(
                &adb.id().to_string(),
                &adb,
            )
            .await;
            ExtensionCommandOutcome::Text(format!("Removed timer '{timer_id}'"))
        }
        Ok(false) => {
            ExtensionCommandOutcome::Error(format!("No timer with id '{timer_id}'"))
        }
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to remove timer: {e}")),
    }
}

/// Parse and execute `/heartbeat add <id> <sec> <min> <hour> <dom> <mon> <dow> <agent_ref> <task...>`.
/// Writes an agent-owned Timer with target = Pinned(current session).
async fn add_cmd(
    rest: &str,
    cap: &dyn AgentStateAdmin,
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
    let entry = match cap.resolve_agent(agent_ref) {
        Ok(e) => e,
        Err(e) => return ExtensionCommandOutcome::Error(e),
    };

    // Open the target agent's DB via the scoped capability.
    let adb = match cap.open_agent_db(&entry).await {
        Ok(adb) => adb,
        Err(e) => return ExtensionCommandOutcome::Error(format!("{e:#}")),
    };

    // Get current session's DB id for Pinned target.
    let session_db_id = {
        let s = ctx.session.lock().await;
        s.database().root_id().to_string()
    };

    let timer = Timer::new(
        id.to_string(),
        Trigger::Cron {
            expr: cron.clone(),
        },
        task.clone(),
        TimerTarget::Pinned {
            session_db_id,
        },
    );
    match adb.upsert_timer(timer).await {
        Ok(()) => {
            notify_agent_timers_changed(&entry.db_id.to_string(), &adb).await;
            ExtensionCommandOutcome::Text(format!(
                "Timer '{id}' on agent '{}': cron='{cron}' → this session — {task}",
                entry.display_name
            ))
        }
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to save timer: {e}")),
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

    async fn fixture() -> (Instance, HostedIndex, Arc<SessionRegistry>, HookContext) {
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
        (instance, index, registry, ctx)
    }

    fn cmd(registry: Arc<crate::session::SessionRegistry>, index: HostedIndex) -> HeartbeatCommand {
        // Build a scoped handle for tests — unrestricted (all agents visible).
        let scoped = crate::extension::agent_state::ScopedAgentStateAdmin::new(
            registry,
            index,
            None,
        );
        HeartbeatCommand {
            agent_state: Arc::new(scoped),
        }
    }

    #[tokio::test]
    async fn add_then_list_round_trips() {
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
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
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
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
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
        match c.invoke("", &ctx).await {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.contains("No timers"), "got: {s}")
            }
            ExtensionCommandOutcome::Error(e) => panic!("expected text, got error: {e}"),
        }
    }

    #[tokio::test]
    async fn remove_without_id_is_usage_error() {
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
        match c.invoke("remove", &ctx).await {
            ExtensionCommandOutcome::Error(e) => {
                assert!(e.contains("Usage: /heartbeat remove"), "got: {e}")
            }
            ExtensionCommandOutcome::Text(s) => panic!("expected error, got text: {s}"),
        }
    }

    #[tokio::test]
    async fn add_then_remove_then_list_empty() {
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
        let _ = c.invoke("add x 0 * * * * * alpha hello", &ctx).await;
        let _ = c.invoke("remove x", &ctx).await;
        match c.invoke("list", &ctx).await {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.contains("No timers"), "got: {s}")
            }
            ExtensionCommandOutcome::Error(e) => panic!("list errored: {e}"),
        }
    }

    #[tokio::test]
    async fn unknown_subcommand_is_error() {
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
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
