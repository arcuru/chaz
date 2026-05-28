//! Schedule tools — let agents schedule recurring or one-shot work via agent-owned
//! schedules.
//!
//! Schedules live in the owning agent's DB (`schedules` store), not in the
//! session DB. When a schedule fires, the agent is woken in the target
//! session (Pinned) or a fresh session created for it (Fresh). This
//! supersedes the legacy session-scoped routine model.
//!
//! Five tools, matching the prior `/schedule` CRUD pattern:
//!   - `schedule_add`    — create a schedule owned by the target agent
//!   - `schedule_modify` — partial update of an existing schedule
//!   - `schedule_remove` — delete a schedule by id
//!   - `schedule_list`   — list schedules owned by an agent
//!   - `schedule_once`       — one-shot schedule targeting current session
//!
//! Tools receive a scoped [`crate::extension::caps::AgentStateAdmin`]
//! handle — they can resolve and open agent DBs within the operator's
//! configured allowlist, but cannot enumerate hosts or access agents
//! outside that set.

use crate::agent_db::{AgentDb, Schedule, ScheduleTarget};
use crate::extension::caps::AgentStateAdmin;
use crate::hosted_index::DbEntry;
use crate::routine::{Trigger, notify_agent_schedules_changed};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy};
use cron::Schedule as CronSchedule;
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

/// Open the target agent's DB for schedule CRUD via the scoped cap.
async fn open_agent_db(cap: &dyn AgentStateAdmin, entry: &DbEntry) -> Result<AgentDb, String> {
    cap.open_agent_db(entry).await.map_err(|e| format!("{e:#}"))
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

/// Parse the optional `expires_at` arg (RFC 3339 / ISO 8601, e.g.
/// `2026-06-01T09:00:00Z`). `Ok(None)` if absent; `Err` if present but
/// unparseable.
fn opt_expires_at(arguments: &Value) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    match opt_str(arguments, "expires_at") {
        None => Ok(None),
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| Some(dt.with_timezone(&chrono::Utc)))
            .map_err(|e| {
                format!("Invalid expires_at '{s}' (use RFC 3339, e.g. 2026-06-01T09:00:00Z): {e}")
            }),
    }
}

/// Parse the optional `max_fires` arg. `Ok(None)` if absent; `Err` if
/// present but not a positive integer (0 would retire immediately).
fn opt_max_fires(arguments: &Value) -> Result<Option<u32>, String> {
    match arguments.get("max_fires") {
        None => Ok(None),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| "max_fires must be a positive integer".to_string())?;
            if n == 0 {
                return Err("max_fires must be >= 1 (0 would never fire)".to_string());
            }
            u32::try_from(n)
                .map(Some)
                .map_err(|_| "max_fires too large".to_string())
        }
    }
}

/// Render the lifecycle bounds as a short ` (…)` suffix, or empty when
/// unbounded. Shared by the add/modify replies and the list output.
fn fmt_bounds(
    expires_at: &Option<chrono::DateTime<chrono::Utc>>,
    max_fires: &Option<u32>,
) -> String {
    let mut parts = Vec::new();
    if let Some(n) = max_fires {
        parts.push(format!("max {n} fires"));
    }
    if let Some(exp) = expires_at {
        parts.push(format!("until {}", exp.format("%Y-%m-%d %H:%M:%SZ")));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn validate_cron(expr: &str) -> Result<(), String> {
    CronSchedule::from_str(expr)
        .map(|_| ())
        .map_err(|e| format!("Invalid cron '{expr}': {e}"))
}

/// Parse the optional `target` argument: `"pinned"` (default) or
/// `"fresh"`. Returns the [`ScheduleTarget`] variant.
fn parse_target(target_str: Option<&str>, session_db_id: &str) -> ScheduleTarget {
    match target_str {
        Some("fresh") => ScheduleTarget::Fresh,
        _ => ScheduleTarget::Pinned {
            session_db_id: session_db_id.to_string(),
        },
    }
}

// -----------------------------------------------------------------------------
// schedule_add
// -----------------------------------------------------------------------------

/// Schedule a recurring schedule on the target agent.
pub struct ScheduleAdd {
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl ScheduleAdd {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for ScheduleAdd {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "schedule_add".to_string(),
            description:
                "Schedule a recurring schedule on an agent. The schedule fires into the current session by default (Pinned), or creates a fresh session each time (Fresh). The owning agent is woken with the task prompt. Schedules live in the agent's DB and survive restarts. Fails if a schedule with this id already exists — use schedule_modify to edit."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id":     { "type": "string", "description": "Unique id for this schedule (e.g. 'hourly-check', 'daily-backup'). Referenced by schedule_modify and schedule_remove." },
                    "cron":   { "type": "string", "description": "6-field cron expression: sec min hour day-of-month month day-of-week. Examples: '0 */5 * * * *' = every 5 minutes; '0 0 9 * * *' = 9am daily." },
                    "task":   { "type": "string", "description": "Free-form instruction the agent receives when the schedule fires." },
                    "agent":  { "type": "string", "description": "Optional: agent that owns the schedule, by display name or DB id. Omit to target yourself." },
                    "target": { "type": "string", "description": "Optional: 'pinned' (fire into this session, default) or 'fresh' (create a new session each fire)." },
                    "max_fires":  { "type": "integer", "description": "Optional: retire the schedule after this many fires. E.g. cron hourly + max_fires 8 = 'wake hourly for 8 hours'." },
                    "expires_at": { "type": "string", "description": "Optional: RFC 3339 timestamp after which the schedule stops firing (e.g. '2026-06-01T09:00:00Z'). Whichever of max_fires/expires_at is hit first retires it." }
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
            let expires_at = opt_expires_at(&arguments)?;
            let max_fires = opt_max_fires(&arguments)?;

            let entry =
                resolve_target_agent(ctx, &*self.agent_state, opt_str(&arguments, "agent"))?;
            let adb = open_agent_db(&*self.agent_state, &entry).await?;

            // Check for duplicate id.
            if adb
                .find_schedule(user_id)
                .await
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Err(format!(
                    "Schedule '{user_id}' already exists on agent '{}'; use schedule_modify to edit or schedule_remove first",
                    entry.display_name
                )
                .into());
            }

            let session_db_id = {
                let s = ctx.session.lock().await;
                s.database().root_id().to_string()
            };
            let schedule_target = parse_target(opt_str(&arguments, "target"), &session_db_id);

            let mut schedule = Schedule::new(
                user_id.to_string(),
                Trigger::Cron {
                    expr: cron.to_string(),
                },
                task.to_string(),
                schedule_target,
            );
            schedule.expires_at = expires_at;
            schedule.max_fires = max_fires;
            adb.upsert_schedule(schedule)
                .await
                .map_err(|e| format!("Failed to save schedule: {e}"))?;
            notify_agent_schedules_changed(&entry.db_id.to_string(), &adb).await;

            let target_label = match opt_str(&arguments, "target") {
                Some("fresh") => "fresh session".to_string(),
                _ => "this session".to_string(),
            };
            let bounds = fmt_bounds(&expires_at, &max_fires);
            Ok(format!(
                "Added schedule '{user_id}' on agent '{}': cron='{cron}' → {target_label}{bounds} — {task}",
                entry.display_name
            ))
        })
    }
}

// -----------------------------------------------------------------------------
// schedule_modify
// -----------------------------------------------------------------------------

/// Partial update of an existing schedule.
pub struct ScheduleModify {
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl ScheduleModify {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for ScheduleModify {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "schedule_modify".to_string(),
            description:
                "Edit an existing schedule on an agent. Only the fields you pass are updated; others are left alone. Fails if no schedule with this id exists."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id":      { "type": "string", "description": "Id of the schedule to edit (as returned by schedule_list)." },
                    "agent":   { "type": "string", "description": "Optional: agent that owns the schedule (defaults to yourself). Required if the schedule is owned by a different agent." },
                    "cron":    { "type": "string", "description": "Optional: new 6-field cron expression." },
                    "task":    { "type": "string", "description": "Optional: new task text." },
                    "target":  { "type": "string", "description": "Optional: 'pinned' or 'fresh'." },
                    "enabled": { "type": "boolean", "description": "Optional: toggle the schedule on/off without deleting it. Re-enabling a schedule that already hit its max_fires/expires_at bound will retire again on the next fire." },
                    "max_fires":  { "type": "integer", "description": "Optional: set/replace the max-fires bound (counts existing fires)." },
                    "expires_at": { "type": "string", "description": "Optional: set/replace the RFC 3339 expiry timestamp." }
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
            let new_expires_at = opt_expires_at(&arguments)?;
            let new_max_fires = opt_max_fires(&arguments)?;

            if new_cron.is_none()
                && new_task.is_none()
                && new_target.is_none()
                && new_enabled.is_none()
                && new_expires_at.is_none()
                && new_max_fires.is_none()
            {
                return Err(
                    "No fields to modify — pass at least one of: cron, task, target, enabled, max_fires, expires_at"
                        .into(),
                );
            }
            if let Some(c) = new_cron {
                validate_cron(c)?;
            }

            let entry =
                resolve_target_agent(ctx, &*self.agent_state, opt_str(&arguments, "agent"))?;
            let adb = open_agent_db(&*self.agent_state, &entry).await?;

            let mut schedule = adb
                .find_schedule(id)
                .await
                .map_err(|e| format!("Failed to read schedules: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "No schedule with id '{id}' on agent '{}'",
                        entry.display_name
                    )
                })?;

            if let Some(c) = new_cron {
                schedule.trigger = Trigger::Cron {
                    expr: c.to_string(),
                };
            }
            if let Some(t) = new_task {
                schedule.prompt = t.to_string();
            }
            if let Some(t) = new_target {
                let session_db_id = {
                    let s = ctx.session.lock().await;
                    s.database().root_id().to_string()
                };
                schedule.target = parse_target(Some(t), &session_db_id);
            }
            if let Some(e) = new_enabled {
                schedule.enabled = e;
            }
            if new_expires_at.is_some() {
                schedule.expires_at = new_expires_at;
            }
            if new_max_fires.is_some() {
                schedule.max_fires = new_max_fires;
            }

            adb.upsert_schedule(schedule)
                .await
                .map_err(|e| format!("Failed to save schedule: {e}"))?;
            notify_agent_schedules_changed(&entry.db_id.to_string(), &adb).await;

            let mut parts = vec![format!(
                "Modified schedule '{id}' on '{}':",
                entry.display_name
            )];
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
            if let Some(n) = new_max_fires {
                parts.push(format!("max_fires={n}"));
            }
            if let Some(exp) = new_expires_at {
                parts.push(format!("expires_at={}", exp.to_rfc3339()));
            }
            Ok(parts.join(" "))
        })
    }
}

// -----------------------------------------------------------------------------
// schedule_remove
// -----------------------------------------------------------------------------

/// Remove a schedule by id from an agent.
pub struct ScheduleRemove {
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl ScheduleRemove {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for ScheduleRemove {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "schedule_remove".to_string(),
            description:
                "Delete a schedule from an agent by id. Pass the agent name if the schedule belongs to a different agent."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id":    { "type": "string", "description": "Id of the schedule to delete (as returned by schedule_list)." },
                    "agent": { "type": "string", "description": "Optional: agent that owns the schedule (defaults to yourself)." }
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

            match adb.remove_schedule(id).await {
                Ok(true) => {
                    notify_agent_schedules_changed(&entry.db_id.to_string(), &adb).await;
                    Ok(format!(
                        "Removed schedule '{id}' from agent '{}'",
                        entry.display_name
                    ))
                }
                Ok(false) => Err(format!(
                    "No schedule with id '{id}' on agent '{}'",
                    entry.display_name
                )
                .into()),
                Err(e) => Err(format!("Failed to remove schedule: {e}").into()),
            }
        })
    }
}

// -----------------------------------------------------------------------------
// schedule_list
// -----------------------------------------------------------------------------

/// List schedules owned by an agent.
pub struct ScheduleList {
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl ScheduleList {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for ScheduleList {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "schedule_list".to_string(),
            description:
                "List schedules owned by an agent — id, schedule, target (pinned/fresh), task, whether enabled, any lifecycle bounds (max_fires/expires_at) and the fire count. Pass agent name to list another agent's schedules."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Optional: agent whose schedules to list (defaults to yourself)." }
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

            let schedules = adb
                .list_schedules()
                .await
                .map_err(|e| format!("Failed to list schedules: {e}"))?;
            if schedules.is_empty() {
                return Ok(format!("No schedules on agent '{}'.", entry.display_name));
            }
            let mut lines = Vec::with_capacity(schedules.len() + 1);
            lines.push(format!("Schedules on '{}':", entry.display_name));
            for t in &schedules {
                let state = if t.enabled { "" } else { " (disabled)" };
                let schedule = match &t.trigger {
                    Trigger::Cron { expr } => expr.clone(),
                    Trigger::OneShot { fire_at } => {
                        format!("@{}", fire_at.format("%Y-%m-%d %H:%M:%SZ"))
                    }
                };
                let target_label = match &t.target {
                    ScheduleTarget::Pinned { .. } => "pinned".to_string(),
                    ScheduleTarget::Fresh => "fresh".to_string(),
                };
                let bounds = fmt_bounds(&t.expires_at, &t.max_fires);
                let fired = if t.fire_count > 0 {
                    format!(" [fired {}×]", t.fire_count)
                } else {
                    String::new()
                };
                lines.push(format!(
                    "- **{}** [{schedule}]{state}{bounds}{fired} → {target_label} — {}",
                    t.id, t.prompt
                ));
            }
            Ok(lines.join("\n"))
        })
    }
}

// -----------------------------------------------------------------------------
// schedule_once
// -----------------------------------------------------------------------------

/// Minimum delay. Anything shorter is shorter than the runner's poll interval,
/// so the wakeup would fire at unpredictable times relative to the request.
const WAKE_MIN_SECONDS: u64 = 30;
/// Upper bound — 30 days. Far enough out that the agent should be using a
/// proper cron rule instead, but not so restrictive that "remind me next week"
/// is impossible.
const WAKE_MAX_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Schedule a one-shot schedule that fires into the current session after a
/// delay, then deletes itself. The schedule is owned by the calling agent.
pub struct ScheduleOnce {
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl ScheduleOnce {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}

impl Tool for ScheduleOnce {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "schedule_once".to_string(),
            description: format!(
                "Schedule a one-shot schedule that fires `task` into this session after `after_seconds`. \
                 The schedule is owned by you; it deletes itself after firing. \
                 Use this when you need to come back to a session later — e.g. 'check the build in 10 minutes'. \
                 Range: {WAKE_MIN_SECONDS}–{WAKE_MAX_SECONDS} seconds. For recurring work, use schedule_add instead."
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "after_seconds": {
                        "type": "integer",
                        "minimum": WAKE_MIN_SECONDS,
                        "maximum": WAKE_MAX_SECONDS,
                        "description": "Seconds from now until the schedule fires.",
                    },
                    "task": {
                        "type": "string",
                        "description": "The instruction you'll receive when the schedule fires.",
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

            // Schedule is always owned by the calling agent and targets the
            // current session (Pinned). Cross-agent scheduling stays in
            // `schedule_add`.
            let entry = resolve_target_agent(ctx, &*self.agent_state, None)?;
            let adb = open_agent_db(&*self.agent_state, &entry).await?;

            let now = chrono::Utc::now();
            let fire_at = now + chrono::Duration::seconds(after as i64);
            let id = format!("wakeup-{}", now.timestamp_millis());

            let session_db_id = {
                let s = ctx.session.lock().await;
                s.database().root_id().to_string()
            };

            let schedule = Schedule::new(
                id.clone(),
                Trigger::OneShot { fire_at },
                task.to_string(),
                ScheduleTarget::Pinned { session_db_id },
            );
            adb.upsert_schedule(schedule)
                .await
                .map_err(|e| format!("Failed to save wakeup: {e}"))?;
            notify_agent_schedules_changed(&entry.db_id.to_string(), &adb).await;

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
        Arc::new(crate::extension::agent_state::ScopedAgentStateAdmin::new(
            registry, index, None,
        ))
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
        let add = ScheduleAdd::new(astate.clone());
        let list = ScheduleList::new(astate);
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
        let add = ScheduleAdd::new(scoped(registry, index));
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
        let add = ScheduleAdd::new(scoped(registry, index));
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
        let add = ScheduleAdd::new(scoped(registry, index));
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
        let add = ScheduleAdd::new(scoped(registry.clone(), index.clone()));
        let modify = ScheduleModify::new(scoped(registry.clone(), index.clone()));
        let list = ScheduleList::new(scoped(registry, index));
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
        let add = ScheduleAdd::new(scoped(registry.clone(), index.clone()));
        let modify = ScheduleModify::new(scoped(registry, index));
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
        let modify = ScheduleModify::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = modify
            .execute(serde_json::json!({ "id": "missing", "task": "t" }), &ctx)
            .await
            .expect_err("unknown id should fail");
        assert!(format!("{err:?}").contains("No schedule"));
    }

    #[tokio::test]
    async fn modify_can_disable_and_reenable() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = ScheduleAdd::new(scoped(registry.clone(), index.clone()));
        let modify = ScheduleModify::new(scoped(registry.clone(), index.clone()));
        let list = ScheduleList::new(scoped(registry, index));
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
    async fn remove_deletes_schedule() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let add = ScheduleAdd::new(scoped(registry.clone(), index.clone()));
        let remove = ScheduleRemove::new(scoped(registry.clone(), index.clone()));
        let list = ScheduleList::new(scoped(registry, index));
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
        assert!(out.contains("No schedules"), "expected empty: {out}");
    }

    #[tokio::test]
    async fn remove_rejects_unknown_id() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let remove = ScheduleRemove::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = remove
            .execute(serde_json::json!({ "id": "missing" }), &ctx)
            .await
            .expect_err("unknown remove should fail");
        assert!(format!("{err:?}").contains("No schedule"));
    }

    #[tokio::test]
    async fn list_empty_returns_friendly_message() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let list = ScheduleList::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("No schedules"), "got: {out}");
    }

    #[tokio::test]
    async fn schedule_once_creates_one_shot_schedule() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let wake = ScheduleOnce::new(scoped(registry.clone(), index.clone()));
        let list = ScheduleList::new(scoped(registry, index));
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
        assert!(listed.contains("wakeup-"), "schedule id missing: {listed}");
        assert!(listed.contains("check on build"), "task missing: {listed}");
        assert!(listed.contains("[@"), "fire_at marker missing: {listed}");
    }

    #[tokio::test]
    async fn schedule_once_rejects_too_short() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let wake = ScheduleOnce::new(scoped(registry, index));
        let ctx = make_ctx("alpha", session);

        let err = wake
            .execute(serde_json::json!({ "after_seconds": 5, "task": "t" }), &ctx)
            .await
            .expect_err("under-min should fail");
        assert!(format!("{err:?}").contains("after_seconds must be between"));
    }

    #[tokio::test]
    async fn schedule_once_rejects_empty_task() {
        let (_i, registry, index, session) = fixture("alpha").await;
        let wake = ScheduleOnce::new(scoped(registry, index));
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
        let add = ScheduleAdd::new(scoped(registry.clone(), index.clone()));
        let list = ScheduleList::new(scoped(registry, index));
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
