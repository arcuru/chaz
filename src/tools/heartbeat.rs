//! Heartbeat tools — let agents schedule recurring work via agent-owned
//! timers.
//!
//! Timers live in the owning agent's DB (`timers` store), not in the
//! session DB. When a timer fires, the agent is woken in the target
//! session (Pinned) or a fresh session created for it (Fresh). This
//! supersedes the session-scoped heartbeat-routine model.
//!
//! Five tools, matching the prior `/heartbeat` CRUD pattern:
//!   - `heartbeat_add`    — create a timer owned by the target agent
//!   - `heartbeat_modify` — partial update of an existing timer
//!   - `heartbeat_remove` — delete a timer by id
//!   - `heartbeat_list`   — list timers owned by an agent
//!   - `wake_me_up`       — one-shot timer targeting current session
//!
//! Tools receive a scoped [`crate::extension::caps::AgentStateAdmin`]
//! handle — they can resolve and open agent DBs within the operator's
//! configured allowlist, but cannot enumerate hosts or access agents
//! outside that set.

use crate::agent_db::{AgentDb, Timer, TimerTarget};
use crate::extension::caps::AgentStateAdmin;
use crate::hosted_index::DbEntry;
use crate::routine::{Trigger, notify_agent_timers_changed};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy};
use cron::Schedule;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

/// Resolve an agent reference to a `DbEntry` via the scoped cap.
/// `None` = the running agent.
fn resolve_target_agent(
    ctx: &ToolContext,
    cap: &dyn AgentStateAdmin,
    agent_ref: Option<&str>,
) -> Result<DbEntry, String> {
    let name = agent_ref.unwrap_or(ctx.agent_name.as_str());
    cap.resolve_agent(name)
}

/// Open the target agent's DB for timer CRUD via the scoped cap.
async fn open_agent_db(
    cap: &dyn AgentStateAdmin,
    entry: &DbEntry,
) -> Result<AgentDb, String> {
    cap.open_agent_db(entry)
        .await
        .map_err(|e| format!("{e:#}"))
}

fn str_arg<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing '{name}' argument"))
}

fn opt_str<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(|v| v.as_str())
}

fn opt_bool(arguments: &Value, name: &str) -> Option<bool> {
    arguments.get(name).and_then(|v| v.as_bool())
}

fn validate_cron(expr: &str) -> Result<(), String> {
    Schedule::from_str(expr)
        .map(|_| ())
        .map_err(|e| format!("Invalid cron '{expr}': {e}"))
}

/// Parse the optional `target` argument: `"pinned"` (default) or
/// `"fresh"`. Returns the [`TimerTarget`] variant.
fn parse_target(target_str: Option<&str>, session_db_id: &str) -> TimerTarget {
    match target_str {
        Some("fresh") => TimerTarget::Fresh,
        _ => TimerTarget::Pinned {
            session_db_id: session_db_id.to_string(),
        },
    }
}

// -----------------------------------------------------------------------------
// heartbeat_add
// -----------------------------------------------------------------------------

/// Schedule a recurring timer on the target agent.
pub struct HeartbeatAdd {
    
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl HeartbeatAdd {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for HeartbeatAdd {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "heartbeat_add".to_string(),
            description:
                "Schedule a recurring timer on an agent. The timer fires into the current session by default (Pinned), or creates a fresh session each time (Fresh). The owning agent is woken with the task prompt. Timers live in the agent's DB and survive restarts. Fails if a timer with this id already exists — use heartbeat_modify to edit."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id":     { "type": "string", "description": "Unique id for this timer (e.g. 'hourly-check', 'daily-backup'). Referenced by heartbeat_modify and heartbeat_remove." },
                    "cron":   { "type": "string", "description": "6-field cron expression: sec min hour day-of-month month day-of-week. Examples: '0 */5 * * * *' = every 5 minutes; '0 0 9 * * *' = 9am daily." },
                    "task":   { "type": "string", "description": "Free-form instruction the agent receives when the timer fires." },
                    "agent":  { "type": "string", "description": "Optional: agent that owns the timer, by display name or DB id. Omit to target yourself." },
                    "target": { "type": "string", "description": "Optional: 'pinned' (fire into this session, default) or 'fresh' (create a new session each fire)." }
                },
                "required": ["id", "cron", "task"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let user_id = str_arg(&arguments, "id")?;
            let cron = str_arg(&arguments, "cron")?;
            let task = str_arg(&arguments, "task")?;
            validate_cron(cron)?;

            let entry =
                resolve_target_agent(ctx, &*self.agent_state, opt_str(&arguments, "agent"))?;
            let adb = open_agent_db(&*self.agent_state, &entry).await?;

            // Check for duplicate id.
            if adb.find_timer(user_id).await.map_err(|e| e.to_string())?.is_some() {
                return Err(format!(
                    "Timer '{user_id}' already exists on agent '{}'; use heartbeat_modify to edit or heartbeat_remove first",
                    entry.display_name
                )
                .into());
            }

            let session_db_id = {
                let s = ctx.session.lock().await;
                s.database().root_id().to_string()
            };
            let timer_target = parse_target(opt_str(&arguments, "target"), &session_db_id);

            let timer = Timer::new(
                user_id.to_string(),
                Trigger::Cron {
                    expr: cron.to_string(),
                },
                task.to_string(),
                timer_target,
            );
            adb.upsert_timer(timer)
                .await
                .map_err(|e| format!("Failed to save timer: {e}"))?;
            notify_agent_timers_changed(&entry.db_id.to_string(), &adb).await;

            let target_label = match opt_str(&arguments, "target") {
                Some("fresh") => "fresh session".to_string(),
                _ => "this session".to_string(),
            };
            Ok(format!(
                "Added timer '{user_id}' on agent '{}': cron='{cron}' → {target_label} — {task}",
                entry.display_name
            ))
        })
    }
}

// -----------------------------------------------------------------------------
// heartbeat_modify
// -----------------------------------------------------------------------------

/// Partial update of an existing timer.
pub struct HeartbeatModify {
    
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl HeartbeatModify {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for HeartbeatModify {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "heartbeat_modify".to_string(),
            description:
                "Edit an existing timer on an agent. Only the fields you pass are updated; others are left alone. Fails if no timer with this id exists."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id":      { "type": "string", "description": "Id of the timer to edit (as returned by heartbeat_list)." },
                    "agent":   { "type": "string", "description": "Optional: agent that owns the timer (defaults to yourself). Required if the timer is owned by a different agent." },
                    "cron":    { "type": "string", "description": "Optional: new 6-field cron expression." },
                    "task":    { "type": "string", "description": "Optional: new task text." },
                    "target":  { "type": "string", "description": "Optional: 'pinned' or 'fresh'." },
                    "enabled": { "type": "boolean", "description": "Optional: toggle the timer on/off without deleting it." }
                },
                "required": ["id"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let id = str_arg(&arguments, "id")?;
            let new_cron = opt_str(&arguments, "cron");
            let new_task = opt_str(&arguments, "task");
            let new_target = opt_str(&arguments, "target");
            let new_enabled = opt_bool(&arguments, "enabled");

            if new_cron.is_none()
                && new_task.is_none()
                && new_target.is_none()
                && new_enabled.is_none()
            {
                return Err(
                    "No fields to modify — pass at least one of: cron, task, target, enabled"
                        .into(),
                );
            }
            if let Some(c) = new_cron {
                validate_cron(c)?;
            }

            let entry =
                resolve_target_agent(ctx, &*self.agent_state, opt_str(&arguments, "agent"))?;
            let adb = open_agent_db(&*self.agent_state, &entry).await?;

            let mut timer = adb
                .find_timer(id)
                .await
                .map_err(|e| format!("Failed to read timers: {e}"))?
                .ok_or_else(|| {
                    format!("No timer with id '{id}' on agent '{}'", entry.display_name)
                })?;

            if let Some(c) = new_cron {
                timer.trigger = Trigger::Cron {
                    expr: c.to_string(),
                };
            }
            if let Some(t) = new_task {
                timer.prompt = t.to_string();
            }
            if let Some(t) = new_target {
                let session_db_id = {
                    let s = ctx.session.lock().await;
                    s.database().root_id().to_string()
                };
                timer.target = parse_target(Some(t), &session_db_id);
            }
            if let Some(e) = new_enabled {
                timer.enabled = e;
            }

            adb.upsert_timer(timer)
                .await
                .map_err(|e| format!("Failed to save timer: {e}"))?;
            notify_agent_timers_changed(&entry.db_id.to_string(), &adb).await;

            let mut parts = vec![format!("Modified timer '{id}' on '{}':", entry.display_name)];
            if let Some(c) = new_cron {
                parts.push(format!("cron='{c}'"));
            }
            if let Some(t) = new_task {
                parts.push(format!("task='{t}'"));
            }
            if let Some(t) = new_target {
                parts.push(format!("target={t}"));
            }
            if let Some(e) = new_enabled {
                parts.push(format!("enabled={e}"));
            }
            Ok(parts.join(" "))
        })
    }
}

// -----------------------------------------------------------------------------
// heartbeat_remove
// -----------------------------------------------------------------------------

/// Remove a timer by id from an agent.
pub struct HeartbeatRemove {
    
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl HeartbeatRemove {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for HeartbeatRemove {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "heartbeat_remove".to_string(),
            description:
                "Delete a timer from an agent by id. Pass the agent name if the timer belongs to a different agent."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id":    { "type": "string", "description": "Id of the timer to delete (as returned by heartbeat_list)." },
                    "agent": { "type": "string", "description": "Optional: agent that owns the timer (defaults to yourself)." }
                },
                "required": ["id"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let id = str_arg(&arguments, "id")?;
            let entry =
                resolve_target_agent(ctx, &*self.agent_state, opt_str(&arguments, "agent"))?;
            let adb = open_agent_db(&*self.agent_state, &entry).await?;

            match adb.remove_timer(id).await {
                Ok(true) => {
                    notify_agent_timers_changed(&entry.db_id.to_string(), &adb).await;
                    Ok(format!("Removed timer '{id}' from agent '{}'", entry.display_name))
                }
                Ok(false) => Err(format!(
                    "No timer with id '{id}' on agent '{}'",
                    entry.display_name
                )
                .into()),
                Err(e) => Err(format!("Failed to remove timer: {e}").into()),
            }
        })
    }
}

// -----------------------------------------------------------------------------
// heartbeat_list
// -----------------------------------------------------------------------------

/// List timers owned by an agent.
pub struct HeartbeatList {
    
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl HeartbeatList {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for HeartbeatList {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "heartbeat_list".to_string(),
            description:
                "List timers owned by an agent — id, schedule, target (pinned/fresh), task, and whether enabled. Pass agent name to list another agent's timers."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Optional: agent whose timers to list (defaults to yourself)." }
                }
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let entry =
                resolve_target_agent(ctx, &*self.agent_state, opt_str(&arguments, "agent"))?;
            let adb = open_agent_db(&*self.agent_state, &entry).await?;

            let timers = adb
                .list_timers()
                .await
                .map_err(|e| format!("Failed to list timers: {e}"))?;
            if timers.is_empty() {
                return Ok(format!(
                    "No timers on agent '{}'.",
                    entry.display_name
                ));
            }
            let mut lines = Vec::with_capacity(timers.len() + 1);
            lines.push(format!("Timers on '{}':", entry.display_name));
            for t in &timers {
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
                lines.push(format!(
                    "- **{}** [{schedule}]{state} → {target_label} — {}",
                    t.id, t.prompt
                ));
            }
            Ok(lines.join("\n"))
        })
    }
}

// -----------------------------------------------------------------------------
// wake_me_up
// -----------------------------------------------------------------------------

/// Minimum delay. Anything shorter is shorter than the runner's poll interval,
/// so the wakeup would fire at unpredictable times relative to the request.
const WAKE_MIN_SECONDS: u64 = 30;
/// Upper bound — 30 days. Far enough out that the agent should be using a
/// proper cron rule instead, but not so restrictive that "remind me next week"
/// is impossible.
const WAKE_MAX_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Schedule a one-shot timer that fires into the current session after a
/// delay, then deletes itself. The timer is owned by the calling agent.
pub struct WakeMeUp {
    
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl WakeMeUp {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for WakeMeUp {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "wake_me_up".to_string(),
            description: format!(
                "Schedule a one-shot timer that fires `task` into this session after `after_seconds`. \
                 The timer is owned by you; it deletes itself after firing. \
                 Use this when you need to come back to a session later — e.g. 'check the build in 10 minutes'. \
                 Range: {WAKE_MIN_SECONDS}–{WAKE_MAX_SECONDS} seconds. For recurring work, use heartbeat_add instead."
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "after_seconds": {
                        "type": "integer",
                        "minimum": WAKE_MIN_SECONDS,
                        "maximum": WAKE_MAX_SECONDS,
                        "description": "Seconds from now until the timer fires.",
                    },
                    "task": {
                        "type": "string",
                        "description": "The instruction you'll receive when the timer fires.",
                    }
                },
                "required": ["after_seconds", "task"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let after = arguments
                .get("after_seconds")
                .and_then(|v| v.as_u64())
                .ok_or("Missing or non-integer 'after_seconds' argument".to_string())?;
            if !(WAKE_MIN_SECONDS..=WAKE_MAX_SECONDS).contains(&after) {
                return Err(format!(
                    "after_seconds must be between {WAKE_MIN_SECONDS} and {WAKE_MAX_SECONDS}"
                )
                .into());
            }
            let task = str_arg(&arguments, "task")?;
            if task.trim().is_empty() {
                return Err("'task' must not be empty".into());
            }

            // Timer is always owned by the calling agent and targets the
            // current session (Pinned). Cross-agent scheduling stays in
            // `heartbeat_add`.
            let entry = resolve_target_agent(ctx, &*self.agent_state, None)?;
            let adb = open_agent_db(&*self.agent_state, &entry).await?;

            let now = chrono::Utc::now();
            let fire_at = now + chrono::Duration::seconds(after as i64);
            let id = format!("wakeup-{}", now.timestamp_millis());

            let session_db_id = {
                let s = ctx.session.lock().await;
                s.database().root_id().to_string()
            };

            let timer = Timer::new(
                id.clone(),
                Trigger::OneShot { fire_at },
                task.to_string(),
                TimerTarget::Pinned {
                    session_db_id,
                },
            );
            adb.upsert_timer(timer)
                .await
                .map_err(|e| format!("Failed to save wakeup: {e}"))?;
            notify_agent_timers_changed(&entry.db_id.to_string(), &adb).await;

            Ok(format!(
                "Wakeup '{id}' scheduled for {} ({}s from now)",
                fire_at.format("%Y-%m-%d %H:%M:%S UTC"),
                after
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
    use crate::hosted_index::{DbEntry, HostedIndex};
    use crate::session::{Session, SessionRegistry};
    use crate::tool::{ScopedTools, ToolContext, ToolProfile, ToolRegistry};
    use crate::types::ConversationId;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    /// Build an unrestricted AgentStateAdmin scoped handle for tests.
    fn scoped(registry: Arc<SessionRegistry>, index: HostedIndex) -> Arc<dyn AgentStateAdmin> {
        Arc::new(
            crate::extension::agent_state::ScopedAgentStateAdmin::new(registry, index, None),
        )
    }

    async fn fixture(
        agent_name: &str,
    ) -> (
        Instance,
        Arc<SessionRegistry>,
        HostedIndex,
        Arc<TokioMutex<Session>>,
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
                agent_name,
                &AgentDbConfig::default(),
                &AgentMeta {
                    display_name: Some(agent_name.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        };
        index.register(DbEntry {
            db_id: agent_db.id(),
            display_name: agent_name.to_string(),
            pubkey,
        });

        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session = Arc::new(TokioMutex::new(
            Session::new(ConversationId(session_db.root_id().to_string()), session_db).await,
        ));

        (instance, registry, index, session)
    }

    fn make_ctx(agent_name: &str, session: Arc<TokioMutex<Session>>) -> ToolContext {
        ToolContext {
            agent_name: agent_name.to_string(),
            call_depth: 0,
            max_call_depth: 10,
            tools: ScopedTools::new(Arc::new(ToolRegistry::new()), None),
            profile: ToolProfile::default(),
            session,
            grants: crate::grants::Grants::default(),
            agent_grants: std::collections::HashMap::new(),
            host: Arc::new(crate::tool_host::NativeToolHost::new()),
            active_extensions: std::collections::HashSet::new(),
        }
    }

    #[tokio::test]
    async fn add_then_list_round_trips() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let astate = scoped(registry, index);
        let add = HeartbeatAdd::new(astate.clone());
        let list = HeartbeatList::new(astate);
        let ctx = make_ctx("alpha", session);

        add.execute(
            serde_json::json!({
                "id": "five-min",
                "cron": "0 */5 * * * *",
                "task": "check in"
            }),
            &ctx,
        )
        .await
        .unwrap();

        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("five-min"), "id missing: {out}");
        assert!(out.contains("0 */5 * * * *"), "cron missing: {out}");
        assert!(out.contains("check in"), "task missing: {out}");
    }

    #[tokio::test]
    async fn add_rejects_duplicate_id() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        add.execute(
            serde_json::json!({ "id": "x", "cron": "0 * * * * *", "task": "t" }),
            &ctx,
        )
        .await
        .unwrap();

        let err = add
            .execute(
                serde_json::json!({ "id": "x", "cron": "0 * * * * *", "task": "other" }),
                &ctx,
            )
            .await
            .expect_err("second add should fail");
        assert!(format!("{err:?}").contains("already exists"));
    }

    #[tokio::test]
    async fn add_rejects_invalid_cron() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = add
            .execute(
                serde_json::json!({ "id": "x", "cron": "not-a-cron", "task": "t" }),
                &ctx,
            )
            .await
            .expect_err("invalid cron should fail");
        assert!(format!("{err:?}").contains("Invalid cron"));
    }

    #[tokio::test]
    async fn add_rejects_unknown_agent() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = add
            .execute(
                serde_json::json!({
                    "id": "x",
                    "cron": "0 * * * * *",
                    "task": "t",
                    "agent": "nope"
                }),
                &ctx,
            )
            .await
            .expect_err("unknown agent should fail");
        assert!(format!("{err:?}").contains("No hosted agent"));
    }

    #[tokio::test]
    async fn modify_partial_updates_only_given_fields() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(scoped(registry.clone(), index.clone()));
        let modify = HeartbeatModify::new(scoped(registry.clone(), index.clone()));
        let list = HeartbeatList::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        add.execute(
            serde_json::json!({ "id": "r1", "cron": "0 * * * * *", "task": "orig" }),
            &ctx,
        )
        .await
        .unwrap();

        modify
            .execute(serde_json::json!({ "id": "r1", "task": "new-task" }), &ctx)
            .await
            .unwrap();

        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("new-task"), "task not updated: {out}");
        assert!(
            out.contains("0 * * * * *"),
            "cron should be preserved: {out}"
        );
    }

    #[tokio::test]
    async fn modify_requires_at_least_one_field() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(scoped(registry.clone(), index.clone()));
        let modify = HeartbeatModify::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        add.execute(
            serde_json::json!({ "id": "r1", "cron": "0 * * * * *", "task": "t" }),
            &ctx,
        )
        .await
        .unwrap();

        let err = modify
            .execute(serde_json::json!({ "id": "r1" }), &ctx)
            .await
            .expect_err("empty modify should fail");
        assert!(format!("{err:?}").contains("No fields to modify"));
    }

    #[tokio::test]
    async fn modify_rejects_unknown_id() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let modify = HeartbeatModify::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = modify
            .execute(serde_json::json!({ "id": "missing", "task": "t" }), &ctx)
            .await
            .expect_err("unknown id should fail");
        assert!(format!("{err:?}").contains("No timer"));
    }

    #[tokio::test]
    async fn modify_can_disable_and_reenable() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(scoped(registry.clone(), index.clone()));
        let modify = HeartbeatModify::new(scoped(registry.clone(), index.clone()));
        let list = HeartbeatList::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        add.execute(
            serde_json::json!({ "id": "r1", "cron": "0 * * * * *", "task": "t" }),
            &ctx,
        )
        .await
        .unwrap();

        modify
            .execute(serde_json::json!({ "id": "r1", "enabled": false }), &ctx)
            .await
            .unwrap();
        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("(disabled)"), "should show disabled: {out}");

        modify
            .execute(serde_json::json!({ "id": "r1", "enabled": true }), &ctx)
            .await
            .unwrap();
        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(
            !out.contains("(disabled)"),
            "should be enabled again: {out}"
        );
    }

    #[tokio::test]
    async fn remove_deletes_timer() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(scoped(registry.clone(), index.clone()));
        let remove = HeartbeatRemove::new(scoped(registry.clone(), index.clone()));
        let list = HeartbeatList::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        add.execute(
            serde_json::json!({ "id": "r1", "cron": "0 * * * * *", "task": "t" }),
            &ctx,
        )
        .await
        .unwrap();

        remove
            .execute(serde_json::json!({ "id": "r1" }), &ctx)
            .await
            .unwrap();

        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("No timers"), "expected empty: {out}");
    }

    #[tokio::test]
    async fn remove_rejects_unknown_id() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let remove = HeartbeatRemove::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = remove
            .execute(serde_json::json!({ "id": "missing" }), &ctx)
            .await
            .expect_err("unknown remove should fail");
        assert!(format!("{err:?}").contains("No timer"));
    }

    #[tokio::test]
    async fn list_empty_returns_friendly_message() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let list = HeartbeatList::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("No timers"), "got: {out}");
    }

    #[tokio::test]
    async fn wake_me_up_creates_one_shot_timer() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let wake = WakeMeUp::new(scoped(registry.clone(), index.clone()));
        let list = HeartbeatList::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let out = wake
            .execute(
                serde_json::json!({ "after_seconds": 60, "task": "check on build" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("Wakeup"), "unexpected msg: {out}");

        let listed = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(listed.contains("wakeup-"), "timer id missing: {listed}");
        assert!(listed.contains("check on build"), "task missing: {listed}");
        assert!(listed.contains("[@"), "fire_at marker missing: {listed}");
    }

    #[tokio::test]
    async fn wake_me_up_rejects_too_short() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let wake = WakeMeUp::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = wake
            .execute(serde_json::json!({ "after_seconds": 5, "task": "t" }), &ctx)
            .await
            .expect_err("under-min should fail");
        assert!(format!("{err:?}").contains("after_seconds must be between"));
    }

    #[tokio::test]
    async fn wake_me_up_rejects_empty_task() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let wake = WakeMeUp::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = wake
            .execute(
                serde_json::json!({ "after_seconds": 60, "task": "   " }),
                &ctx,
            )
            .await
            .expect_err("empty task should fail");
        assert!(format!("{err:?}").contains("must not be empty"));
    }

    #[tokio::test]
    async fn add_with_fresh_target() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(scoped(registry.clone(), index.clone()));
        let list = HeartbeatList::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        add.execute(
            serde_json::json!({
                "id": "fresh-one",
                "cron": "0 0 * * * *",
                "task": "daily report",
                "target": "fresh"
            }),
            &ctx,
        )
        .await
        .unwrap();

        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("fresh-one"), "id missing: {out}");
        assert!(out.contains("fresh"), "target label missing: {out}");
    }
}
