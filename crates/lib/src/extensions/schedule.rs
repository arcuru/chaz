//! Schedule extension — agent-owned schedules via tools + commands.
//!
//! Wires the schedule surface (5 tools + the `/schedule` slash command)
//! into the extension hub. Schedules live in the owning agent's DB
//! (`schedules` store); the `/schedule` command and tools write there
//! instead of the session `routines` table.

use crate::agent_db::{AgentDb, Schedule, ScheduleTarget};
use crate::extension::agent_state::{ScopedAgentStateAdmin, deny_all_warning};
use crate::extension::caps::AgentStateAdmin;
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{
    Extension, ExtensionCommand, ExtensionCommandOutcome, ExtensionRef, HookContext, HookKind,
};
use crate::routine::Trigger;
use crate::tools::{ScheduleAdd, ScheduleList, ScheduleModify, ScheduleOnce, ScheduleRemove};
use cron::Schedule as CronSchedule;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

/// Schedule extension. Stateless — each instance builds a scoped
/// `AgentStateAdmin` from the peer handles at instantiate time.
/// Agent-owned schedule fires run through the separate `agent_schedule`
/// extension's standalone path.
pub struct ScheduleExtension;

impl ScheduleExtension {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ScheduleExtension {
    fn default() -> Self {
        Self::new()
    }
}

impl Extension for ScheduleExtension {
    fn name(&self) -> &'static str {
        "schedule"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool, HookKind::Command]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![HookKind::Tool, HookKind::Command],
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn instantiate<'a>(&'a self, scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        let peer = scope_ctx.peer();
        let allowlist = peer.agent_state_allowlist.get("schedule").cloned();
        // An empty list otherwise makes every requested agent look missing.
        if let Some(warning) = deny_all_warning("schedule", allowlist.as_deref()) {
            tracing::warn!("{warning}");
        }
        let agent_state: Arc<dyn AgentStateAdmin> = Arc::new(ScopedAgentStateAdmin::new(
            peer.registry.clone(),
            peer.agent_index.clone(),
            allowlist,
        ));
        Box::pin(async move {
            Ok(Arc::new(ScheduleInstance {
                manifest,
                agent_state,
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct ScheduleInstance {
    manifest: ExtensionManifest,
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl ExtensionInstance for ScheduleInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn tools(&self) -> Vec<Arc<dyn crate::tool::Tool>> {
        vec![
            Arc::new(ScheduleAdd::new(self.agent_state.clone())),
            Arc::new(ScheduleModify::new(self.agent_state.clone())),
            Arc::new(ScheduleRemove::new(self.agent_state.clone())),
            Arc::new(ScheduleList::new(self.agent_state.clone())),
            Arc::new(ScheduleOnce::new(self.agent_state.clone())),
        ]
    }

    fn commands(&self) -> Vec<(String, Arc<dyn ExtensionCommand>)> {
        vec![(
            "schedule".into(),
            Arc::new(ScheduleCommand {
                agent_state: self.agent_state.clone(),
            }),
        )]
    }
}

struct ScheduleCommand {
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl ExtensionCommand for ScheduleCommand {
    fn description(&self) -> &'static str {
        "Manage schedules on agents — add | modify | remove | list"
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
                "" | "list" => list_cmd(rest, &*self.agent_state, ctx).await,
                "remove" | "rm" => {
                    if rest.is_empty() {
                        ExtensionCommandOutcome::Error(
                            "Usage: /schedule remove <id> [agent]".into(),
                        )
                    } else {
                        remove_cmd(rest, &*self.agent_state, ctx).await
                    }
                }
                "add" => add_cmd(rest, &*self.agent_state, ctx).await,
                "modify" => modify_cmd(rest, &*self.agent_state, ctx).await,
                other => ExtensionCommandOutcome::Error(format!(
                    "Unknown subcommand '/schedule {other}'. Use: add | modify | remove | list"
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
    let entry = cap
        .resolve_agent(name)
        .map_err(ExtensionCommandOutcome::Error)?;
    let adb = cap
        .open_agent_db(&entry)
        .await
        .map_err(|e| ExtensionCommandOutcome::Error(format!("{e:#}")))?;
    Ok((entry, adb))
}

/// Format one schedule row.
fn fmt_schedule_line(t: &Schedule) -> String {
    let state = if t.enabled { "" } else { " (disabled)" };
    let when = match &t.trigger {
        Trigger::Cron { expr } => expr.clone(),
        Trigger::Interval { period } => format!("every {period:?}"),
        Trigger::OneShot { fire_at } => format!("@{}", fire_at.format("%Y-%m-%d %H:%M:%SZ")),
    };
    let target_label = match &t.target {
        ScheduleTarget::Pinned { .. } => "pinned",
        ScheduleTarget::Fresh => "fresh",
    };
    let mut bounds = Vec::new();
    if let Some(n) = t.max_fires {
        bounds.push(format!("max {n} fires"));
    }
    if let Some(exp) = t.expires_at {
        bounds.push(format!("until {}", exp.format("%Y-%m-%d %H:%M:%SZ")));
    }
    let bounds = if bounds.is_empty() {
        String::new()
    } else {
        format!(" ({})", bounds.join(", "))
    };
    let fired = if t.fire_count > 0 {
        format!(" [fired {}×]", t.fire_count)
    } else {
        String::new()
    };
    format!(
        "  {} [{when}]{state}{bounds}{fired} → {target_label} — {}",
        t.id, t.prompt
    )
}

/// Render one agent's block: a header line (with a `*host*` marker when
/// applicable) followed by its schedule rows. A scope denial or open
/// failure degrades to a single explanatory line so one inaccessible
/// agent never aborts the whole session-wide listing.
async fn agent_block(cap: &dyn AgentStateAdmin, name: &str, host: bool) -> String {
    let marker = if host { " *host*" } else { "" };
    let entry = match cap.resolve_agent(name) {
        Ok(e) => e,
        Err(e) => return format!("{name}{marker}: not accessible — {e}"),
    };
    let adb = match cap.open_agent_db(&entry).await {
        Ok(a) => a,
        Err(e) => return format!("{name}{marker}: failed to open agent DB — {e:#}"),
    };
    match adb.list_schedules().await {
        Ok(s) if s.is_empty() => format!("{name}{marker}: (no schedules)"),
        Ok(s) => {
            let lines: Vec<String> = s.iter().map(fmt_schedule_line).collect();
            format!("{name}{marker}:\n{}", lines.join("\n"))
        }
        Err(e) => format!("{name}{marker}: failed to list — {e}"),
    }
}

/// `/schedule list` — session-wide inventory across every agent attached
/// to the current session (host marked). `/schedule list <agent>`
/// narrows to one agent. A session with no Living Agents attached lists
/// the calling agent.
async fn list_cmd(
    rest: &str,
    cap: &dyn AgentStateAdmin,
    ctx: &HookContext,
) -> ExtensionCommandOutcome {
    let rest = rest.trim();
    if !rest.is_empty() {
        return ExtensionCommandOutcome::Text(agent_block(cap, rest, false).await);
    }
    let meta = ctx.session.lock().await.read_meta().await;
    if meta.agents.is_empty() {
        return ExtensionCommandOutcome::Text(agent_block(cap, &ctx.agent_name, false).await);
    }
    let host = meta.host_agent_db_id.as_deref();
    let mut blocks = Vec::with_capacity(meta.agents.len());
    for a in &meta.agents {
        let is_host = host == Some(a.db_id.as_str());
        blocks.push(agent_block(cap, &a.display_name, is_host).await);
    }
    ExtensionCommandOutcome::Text(format!(
        "Schedules across {} agent(s) on this session:\n\n{}",
        meta.agents.len(),
        blocks.join("\n\n")
    ))
}

async fn remove_cmd(
    rest: &str,
    cap: &dyn AgentStateAdmin,
    ctx: &HookContext,
) -> ExtensionCommandOutcome {
    // Format: /schedule remove <id> [agent]
    let (schedule_id, agent_ref) = match rest.split_once(char::is_whitespace) {
        Some((id, agent)) => (id.trim(), Some(agent.trim())),
        None => (rest.trim(), None),
    };
    let (_, adb) = match open_agent_db_for_cmd(cap, ctx, agent_ref).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    match adb.remove_schedule(schedule_id).await {
        Ok(true) => {
            if let Some(engine) = ctx.routine_engine.as_ref()
                && let Err(e) = engine.reload_agent(&adb.id().to_string(), &adb).await
            {
                tracing::warn!(agent = %adb.id(), "engine.reload_agent after remove failed: {e}");
            }
            ExtensionCommandOutcome::Text(format!("Removed schedule '{schedule_id}'"))
        }
        Ok(false) => ExtensionCommandOutcome::Error(format!("No schedule with id '{schedule_id}'")),
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to remove schedule: {e}")),
    }
}

/// Parse and execute `/schedule modify <id> cron <6 fields> [agent]` or
/// `/schedule modify <id> interval <seconds> [agent]`.
async fn modify_cmd(
    rest: &str,
    cap: &dyn AgentStateAdmin,
    ctx: &HookContext,
) -> ExtensionCommandOutcome {
    let mut tokens = rest.split_whitespace();
    let (id, kind) = match (tokens.next(), tokens.next()) {
        (Some(id), Some(kind)) => (id, kind),
        _ => {
            return ExtensionCommandOutcome::Error(
                "Usage: /schedule modify <id> cron <sec> <min> <hour> <dom> <mon> <dow> [agent] | interval <seconds> [agent]".into(),
            );
        }
    };
    let (trigger, agent_ref, description) = match kind {
        "interval" => {
            let seconds = match tokens.next().and_then(|value| value.parse::<u64>().ok()) {
                Some(seconds) if seconds > 0 => seconds,
                _ => {
                    return ExtensionCommandOutcome::Error(
                        "Interval seconds must be greater than zero".into(),
                    );
                }
            };
            let period = std::time::Duration::from_secs(seconds);
            if chrono::Duration::from_std(period).is_err() {
                return ExtensionCommandOutcome::Error(
                    "Interval seconds cannot be represented by chrono".into(),
                );
            }
            (
                Trigger::Interval { period },
                tokens.next(),
                format!("every {seconds}s"),
            )
        }
        "cron" => {
            let fields: Vec<_> = (&mut tokens).take(6).collect();
            if fields.len() != 6 {
                return ExtensionCommandOutcome::Error(
                    "Usage: /schedule modify <id> cron <sec> <min> <hour> <dom> <mon> <dow> [agent]".into(),
                );
            }
            let expr = fields.join(" ");
            if let Err(e) = CronSchedule::from_str(&expr) {
                return ExtensionCommandOutcome::Error(format!("Invalid cron '{expr}': {e}"));
            }
            (
                Trigger::Cron { expr: expr.clone() },
                tokens.next(),
                format!("cron='{expr}'"),
            )
        }
        _ => return ExtensionCommandOutcome::Error("Trigger must be cron or interval".into()),
    };
    if tokens.next().is_some() {
        return ExtensionCommandOutcome::Error("Too many arguments to /schedule modify".into());
    }
    let (_, adb) = match open_agent_db_for_cmd(cap, ctx, agent_ref).await {
        Ok(value) => value,
        Err(error) => return error,
    };
    let Some(mut schedule) = (match adb.find_schedule(id).await {
        Ok(schedule) => schedule,
        Err(e) => return ExtensionCommandOutcome::Error(format!("Failed to read schedule: {e}")),
    }) else {
        return ExtensionCommandOutcome::Error(format!("No schedule with id '{id}'"));
    };
    schedule.trigger = trigger;
    match adb.upsert_schedule(schedule).await {
        Ok(()) => {
            if let Some(engine) = ctx.routine_engine.as_ref()
                && let Err(e) = engine.reload_agent(&adb.id().to_string(), &adb).await
            {
                tracing::warn!(agent = %adb.id(), "engine.reload_agent after modify failed: {e}");
            }
            ExtensionCommandOutcome::Text(format!("Modified schedule '{id}': {description}"))
        }
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to save schedule: {e}")),
    }
}

/// Parse and execute `/schedule add <id> <sec> <min> <hour> <dom> <mon> <dow> <agent_ref> <task...>`
/// or `/schedule add interval <id> <seconds> <agent_ref> <task...>`.
/// Writes an agent-owned Schedule with target = Pinned(current session).
async fn add_cmd(
    rest: &str,
    cap: &dyn AgentStateAdmin,
    ctx: &HookContext,
) -> ExtensionCommandOutcome {
    let mut tokens = rest.split_whitespace();
    if tokens.next() == Some("interval") {
        let (id, seconds, agent_ref) = match (tokens.next(), tokens.next(), tokens.next()) {
            (Some(id), Some(seconds), Some(agent_ref)) => (id, seconds, agent_ref),
            _ => {
                return ExtensionCommandOutcome::Error(
                    "Usage: /schedule add interval <id> <seconds> <agent> <task...>".into(),
                );
            }
        };
        let task = tokens.collect::<Vec<_>>().join(" ");
        if task.is_empty() {
            return ExtensionCommandOutcome::Error(
                "Usage: /schedule add interval <id> <seconds> <agent> <task...>".into(),
            );
        }
        let seconds = match seconds.parse::<u64>() {
            Ok(seconds) if seconds > 0 => seconds,
            _ => {
                return ExtensionCommandOutcome::Error(
                    "Interval seconds must be greater than zero".into(),
                );
            }
        };
        let period = std::time::Duration::from_secs(seconds);
        if chrono::Duration::from_std(period).is_err() {
            return ExtensionCommandOutcome::Error(
                "Interval seconds cannot be represented by chrono".into(),
            );
        }
        let entry = match cap.resolve_agent(agent_ref) {
            Ok(entry) => entry,
            Err(e) => return ExtensionCommandOutcome::Error(e),
        };
        let adb = match cap.open_agent_db(&entry).await {
            Ok(adb) => adb,
            Err(e) => return ExtensionCommandOutcome::Error(format!("{e:#}")),
        };
        let session_db_id = {
            let s = ctx.session.lock().await;
            s.database().root_id().to_string()
        };
        let schedule = Schedule::new(
            id.to_string(),
            Trigger::Interval { period },
            task.clone(),
            ScheduleTarget::Pinned { session_db_id },
        );
        return match adb.upsert_schedule(schedule).await {
            Ok(()) => {
                if let Some(engine) = ctx.routine_engine.as_ref()
                    && let Err(e) = engine.reload_agent(&entry.db_id.to_string(), &adb).await
                {
                    tracing::warn!(agent = %entry.db_id, "engine.reload_agent after add failed: {e}");
                }
                ExtensionCommandOutcome::Text(format!(
                    "Schedule '{id}' on agent '{}': every {seconds}s → this session — {task}",
                    entry.display_name
                ))
            }
            Err(e) => ExtensionCommandOutcome::Error(format!("Failed to save schedule: {e}")),
        };
    }
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
                "Usage: /schedule add <id> <sec> <min> <hour> <dom> <mon> <dow> <agent> <task...>"
                    .into(),
            );
        }
    };
    let cron = format!("{c1} {c2} {c3} {c4} {c5} {c6}");
    if let Err(e) = CronSchedule::from_str(&cron) {
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

    let schedule = Schedule::new(
        id.to_string(),
        Trigger::Cron { expr: cron.clone() },
        task.clone(),
        ScheduleTarget::Pinned { session_db_id },
    );
    match adb.upsert_schedule(schedule).await {
        Ok(()) => {
            if let Some(engine) = ctx.routine_engine.as_ref()
                && let Err(e) = engine.reload_agent(&entry.db_id.to_string(), &adb).await
            {
                tracing::warn!(agent = %entry.db_id, "engine.reload_agent after add failed: {e}");
            }
            ExtensionCommandOutcome::Text(format!(
                "Schedule '{id}' on agent '{}': cron='{cron}' → this session — {task}",
                entry.display_name
            ))
        }
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to save schedule: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
    use crate::hosted_index::{DbEntry, HostedIndex};
    use crate::session::{Session, SessionRegistry};
    use crate::types::ConversationId;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};
    use tokio::sync::Mutex;

    async fn fixture() -> (Instance, HostedIndex, Arc<SessionRegistry>, HookContext) {
        let backend = InMemory::new();
        let (instance, user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
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
            routine_engine: None,
        };
        (instance, index, registry, ctx)
    }

    fn cmd(registry: Arc<crate::session::SessionRegistry>, index: HostedIndex) -> ScheduleCommand {
        // Build a scoped handle for tests — unrestricted (all agents visible).
        let scoped =
            crate::extension::agent_state::ScopedAgentStateAdmin::new(registry, index, None);
        ScheduleCommand {
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
    async fn interval_add_then_list_round_trips() {
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
        let added = c
            .invoke("add interval five-min 300 alpha do a thing", &ctx)
            .await;
        match added {
            ExtensionCommandOutcome::Text(s) => assert!(s.contains("every 300s"), "got: {s}"),
            ExtensionCommandOutcome::Error(e) => panic!("add failed: {e}"),
        }
        let listed = c.invoke("list", &ctx).await;
        let out = match listed {
            ExtensionCommandOutcome::Text(s) => s,
            ExtensionCommandOutcome::Error(e) => panic!("list failed: {e}"),
        };
        assert!(out.contains("five-min"), "list missing id: {out}");
        assert!(out.contains("every 300s"), "list missing interval: {out}");
    }

    #[tokio::test]
    async fn modify_switches_a_schedule_to_interval() {
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
        let _ = c.invoke("add tick 0 * * * * * alpha check", &ctx).await;
        match c.invoke("modify tick interval 300 alpha", &ctx).await {
            ExtensionCommandOutcome::Text(s) => assert!(s.contains("every 300s"), "got: {s}"),
            ExtensionCommandOutcome::Error(e) => panic!("modify failed: {e}"),
        }
        match c.invoke("list", &ctx).await {
            ExtensionCommandOutcome::Text(s) => assert!(s.contains("every 300s"), "got: {s}"),
            ExtensionCommandOutcome::Error(e) => panic!("list failed: {e}"),
        }
    }

    #[tokio::test]
    async fn list_with_agent_arg_narrows_to_that_agent() {
        let (_i, index, registry, ctx) = fixture().await;
        let c = cmd(registry, index);
        let _ = c
            .invoke("add nine-am 0 0 9 * * * alpha morning brief", &ctx)
            .await;
        // `/schedule list alpha` resolves the named agent (the arg used
        // to be silently ignored before the session-wide rework).
        match c.invoke("list alpha", &ctx).await {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.starts_with("alpha"), "header missing: {s}");
                assert!(s.contains("nine-am"), "schedule missing: {s}");
            }
            ExtensionCommandOutcome::Error(e) => panic!("list errored: {e}"),
        }
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
                assert!(s.contains("(no schedules)"), "got: {s}")
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
                assert!(e.contains("Usage: /schedule remove"), "got: {e}")
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
                assert!(s.contains("(no schedules)"), "got: {s}")
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
}
