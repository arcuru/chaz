//! Heartbeat tools — let agents schedule recurring directives on the current
//! session.
//!
//! A heartbeat rule fires a Directive entry into this session on a cron
//! schedule. The target agent receives it via the usual mention-aware
//! routing path. By default the target is the running agent ("self").
//!
//! Four tools, matching the `/agent` CRUD pattern:
//!   - `heartbeat_add`    — create; rejects a duplicate id
//!   - `heartbeat_modify` — partial update of an existing rule
//!   - `heartbeat_remove` — delete by id
//!   - `heartbeat_list`   — list rules on this session
//!
//! Rules live in the session DB as [`Routine`] rows whose `target.payload`
//! deserializes to [`HeartbeatPayload`]. The routine engine picks them up
//! and dispatches them through the heartbeat extension's `RoutineHandler`.

use crate::extensions::heartbeat::HeartbeatPayload;
use crate::hosted_index::{DbEntry, HostedIndex};
use crate::routine::{
    Routine, RoutineId, RoutineTarget, Trigger, list_session_routines, remove_session_routine,
    upsert_session_routine,
};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy};
use cron::Schedule;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

const HEARTBEAT_EXTENSION: &str = "heartbeat";

/// Pull the heartbeat-shaped payload off a routine, falling back to a
/// debug-only placeholder when the payload didn't deserialize (e.g.
/// a row written by some other extension into the same store).
fn payload_for(routine: &Routine) -> HeartbeatPayload {
    serde_json::from_value(routine.target.payload.clone()).unwrap_or(HeartbeatPayload {
        rule_name: routine.name.clone(),
        target_agent_db_id: String::new(),
        task: String::new(),
        is_one_shot: matches!(routine.trigger, Trigger::OneShot { .. }),
    })
}

fn build_routine(
    id: &str,
    trigger: Trigger,
    payload: HeartbeatPayload,
    enabled: bool,
) -> anyhow::Result<Routine> {
    let target = RoutineTarget {
        extension: HEARTBEAT_EXTENSION.into(),
        payload: serde_json::to_value(&payload)?,
    };
    let mut r = match trigger {
        Trigger::Cron { expr } => Routine::cron(RoutineId::new(id), id, expr, target),
        Trigger::OneShot { fire_at } => Routine::one_shot(RoutineId::new(id), id, fire_at, target),
    };
    r.enabled = enabled;
    Ok(r)
}

/// Resolve an agent reference to a `DbEntry`. `None` = the running agent.
/// Matches the resolution order used by `/agent` commands: display name first,
/// then DB id.
fn resolve_target_agent(
    ctx: &ToolContext,
    index: &HostedIndex,
    agent_ref: Option<&str>,
) -> Result<DbEntry, String> {
    let name = agent_ref.unwrap_or(ctx.agent_name.as_str());
    if let Some(entry) = index.find_by_name(name) {
        return Ok(entry);
    }
    if let Ok(id) = eidetica::entry::ID::parse(name)
        && let Some(entry) = index.find_by_id(&id)
    {
        return Ok(entry);
    }
    Err(format!(
        "No hosted agent matches '{name}' — pass a display name or DB id, or omit to target yourself"
    ))
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

// -----------------------------------------------------------------------------
// heartbeat_add
// -----------------------------------------------------------------------------

/// Schedule a recurring directive on the current session.
pub struct HeartbeatAdd {
    agent_index: HostedIndex,
}

impl HeartbeatAdd {
    pub fn new(agent_index: HostedIndex) -> Self {
        Self { agent_index }
    }
}

impl Tool for HeartbeatAdd {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "heartbeat_add".to_string(),
            description:
                "Schedule a recurring directive on this session. When the cron fires, a Directive entry is written to the session telling the target agent to do `task`. The target defaults to you (the running agent). Rules persist in the session DB and survive restarts. Fails if a rule with this id already exists — use heartbeat_modify to edit."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id":    { "type": "string", "description": "Unique id for this rule on this session (e.g. 'hourly-check', 'daily-backup'). Referenced by heartbeat_modify and heartbeat_remove." },
                    "cron":  { "type": "string", "description": "6-field cron expression: sec min hour day-of-month month day-of-week. Examples: '0 */5 * * * *' = every 5 minutes; '0 0 9 * * *' = 9am daily." },
                    "task":  { "type": "string", "description": "Free-form instruction the target agent receives when the rule fires." },
                    "agent": { "type": "string", "description": "Optional: agent that receives the directive, by display name or DB id. Omit to target yourself." }
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
            let id = str_arg(&arguments, "id")?;
            let cron = str_arg(&arguments, "cron")?;
            let task = str_arg(&arguments, "task")?;
            validate_cron(cron)?;

            let target =
                resolve_target_agent(ctx, &self.agent_index, opt_str(&arguments, "agent"))?;

            let session = ctx.session.lock().await;
            let db = session.database();

            let existing = list_session_routines(db)
                .await
                .map_err(|e| format!("Failed to read routines: {e}"))?;
            if existing.iter().any(|r| r.id.as_str() == id) {
                return Err(format!(
                    "Heartbeat rule '{id}' already exists; use heartbeat_modify to edit or heartbeat_remove first"
                )
                .into());
            }

            let routine = build_routine(
                id,
                Trigger::Cron { expr: cron.into() },
                HeartbeatPayload {
                    rule_name: id.to_string(),
                    target_agent_db_id: target.db_id.to_string(),
                    task: task.to_string(),
                    is_one_shot: false,
                },
                true,
            )
            .map_err(|e| format!("Failed to encode routine payload: {e}"))?;
            upsert_session_routine(db, &routine)
                .await
                .map_err(|e| format!("Failed to save rule: {e}"))?;

            Ok(format!(
                "Added heartbeat '{id}': cron='{cron}' → {} — {task}",
                target.display_name
            ))
        })
    }
}

// -----------------------------------------------------------------------------
// heartbeat_modify
// -----------------------------------------------------------------------------

/// Partial update of an existing heartbeat rule.
pub struct HeartbeatModify {
    agent_index: HostedIndex,
}

impl HeartbeatModify {
    pub fn new(agent_index: HostedIndex) -> Self {
        Self { agent_index }
    }
}

impl Tool for HeartbeatModify {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "heartbeat_modify".to_string(),
            description:
                "Edit an existing heartbeat rule on this session. Only the fields you pass are updated; others are left alone. Fails if no rule with this id exists."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id":      { "type": "string", "description": "Id of the rule to edit (as returned by heartbeat_list)." },
                    "cron":    { "type": "string", "description": "Optional: new 6-field cron expression." },
                    "task":    { "type": "string", "description": "Optional: new task text." },
                    "agent":   { "type": "string", "description": "Optional: new target agent (display name or DB id)." },
                    "enabled": { "type": "boolean", "description": "Optional: toggle the rule on/off without deleting it." }
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
            let new_agent = opt_str(&arguments, "agent");
            let new_enabled = opt_bool(&arguments, "enabled");

            if new_cron.is_none()
                && new_task.is_none()
                && new_agent.is_none()
                && new_enabled.is_none()
            {
                return Err(
                    "No fields to modify — pass at least one of: cron, task, agent, enabled".into(),
                );
            }
            if let Some(c) = new_cron {
                validate_cron(c)?;
            }

            let session = ctx.session.lock().await;
            let db = session.database();

            let mut routine = list_session_routines(db)
                .await
                .map_err(|e| format!("Failed to read routines: {e}"))?
                .into_iter()
                .find(|r| r.id.as_str() == id)
                .ok_or_else(|| format!("No heartbeat rule with id '{id}' on this session"))?;
            let mut payload = payload_for(&routine);

            if let Some(c) = new_cron {
                routine.trigger = Trigger::Cron { expr: c.into() };
            }
            if let Some(t) = new_task {
                payload.task = t.to_string();
            }
            let target_display = if let Some(a) = new_agent {
                let target = resolve_target_agent(ctx, &self.agent_index, Some(a))?;
                payload.target_agent_db_id = target.db_id.to_string();
                Some(target.display_name)
            } else {
                None
            };
            if let Some(e) = new_enabled {
                routine.enabled = e;
            }
            routine.target.payload = serde_json::to_value(&payload)
                .map_err(|e| format!("Failed to encode payload: {e}"))?;

            upsert_session_routine(db, &routine)
                .await
                .map_err(|e| format!("Failed to save rule: {e}"))?;

            let mut parts = vec![format!("Modified heartbeat '{id}':")];
            if let Some(c) = new_cron {
                parts.push(format!("cron='{c}'"));
            }
            if let Some(t) = new_task {
                parts.push(format!("task='{t}'"));
            }
            if let Some(name) = target_display {
                parts.push(format!("agent={name}"));
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

/// Remove a heartbeat rule by id.
pub struct HeartbeatRemove;

impl Tool for HeartbeatRemove {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "heartbeat_remove".to_string(),
            description: "Delete a heartbeat rule from this session by id.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Id of the rule to delete (as returned by heartbeat_list)." }
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
            let session = ctx.session.lock().await;
            let db = session.database();
            match remove_session_routine(db, &RoutineId::new(id)).await {
                Ok(true) => Ok(format!("Removed heartbeat '{id}'")),
                Ok(false) => {
                    Err(format!("No heartbeat rule with id '{id}' on this session").into())
                }
                Err(e) => Err(format!("Failed to remove rule: {e}").into()),
            }
        })
    }
}

// -----------------------------------------------------------------------------
// heartbeat_list
// -----------------------------------------------------------------------------

/// List heartbeat rules on the current session. Resolves target agent DB ids to
/// display names where possible.
pub struct HeartbeatList {
    agent_index: HostedIndex,
}

impl HeartbeatList {
    pub fn new(agent_index: HostedIndex) -> Self {
        Self { agent_index }
    }
}

impl Tool for HeartbeatList {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "heartbeat_list".to_string(),
            description:
                "List heartbeat rules on this session — id, cron, target agent, task, and whether the rule is enabled."
                    .to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn execute<'a>(
        &'a self,
        _arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let session = ctx.session.lock().await;
            let db = session.database();
            let routines = list_session_routines(db)
                .await
                .map_err(|e| format!("Failed to list rules: {e}"))?;
            if routines.is_empty() {
                return Ok("No heartbeat rules on this session.".to_string());
            }
            let mut lines = Vec::with_capacity(routines.len() + 1);
            lines.push("Heartbeat rules:".to_string());
            for r in &routines {
                let p = payload_for(r);
                let target_display = match eidetica::entry::ID::parse(&p.target_agent_db_id) {
                    Ok(id) => self
                        .agent_index
                        .find_by_id(&id)
                        .map(|e| e.display_name)
                        .unwrap_or_else(|| p.target_agent_db_id.clone()),
                    Err(_) => p.target_agent_db_id.clone(),
                };
                let state = if r.enabled { "" } else { " (disabled)" };
                let schedule = match &r.trigger {
                    Trigger::Cron { expr } => expr.clone(),
                    Trigger::OneShot { fire_at } => {
                        format!("@{}", fire_at.format("%Y-%m-%d %H:%M:%SZ"))
                    }
                };
                lines.push(format!(
                    "- **{}** [{schedule}]{state} → {} — {}",
                    r.id, target_display, p.task
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

/// Schedule a one-shot wakeup that fires a Directive into this session after a
/// delay, then deletes itself. The wakeup targets the calling agent so the
/// resulting directive is routed back to *you*.
pub struct WakeMeUp {
    agent_index: HostedIndex,
}

impl WakeMeUp {
    pub fn new(agent_index: HostedIndex) -> Self {
        Self { agent_index }
    }
}

impl Tool for WakeMeUp {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "wake_me_up".to_string(),
            description: format!(
                "Schedule a one-shot wakeup that fires `task` into this session after `after_seconds`. \
                 The directive is routed to you (the calling agent); the rule deletes itself once it fires. \
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
                        "description": "Seconds from now until the wakeup fires.",
                    },
                    "task": {
                        "type": "string",
                        "description": "The instruction you'll receive when the wakeup fires.",
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

            // Target is always self — agents wake themselves, not each other.
            // Cross-agent scheduling stays in `heartbeat_add`.
            let target = resolve_target_agent(ctx, &self.agent_index, None)?;

            let now = chrono::Utc::now();
            let fire_at = now + chrono::Duration::seconds(after as i64);
            // Epoch-ms id keeps it unique and sortable for /heartbeat list and
            // heartbeat_list output; no need for randomness in single-process use.
            let id = format!("wakeup-{}", now.timestamp_millis());

            let session = ctx.session.lock().await;
            let db = session.database();
            let routine = build_routine(
                &id,
                Trigger::OneShot { fire_at },
                HeartbeatPayload {
                    rule_name: id.clone(),
                    target_agent_db_id: target.db_id.to_string(),
                    task: task.to_string(),
                    is_one_shot: true,
                },
                true,
            )
            .map_err(|e| format!("Failed to encode wakeup payload: {e}"))?;
            upsert_session_routine(db, &routine)
                .await
                .map_err(|e| format!("Failed to save wakeup: {e}"))?;

            Ok(format!(
                "Wakeup '{id}' scheduled for {} ({}s from now) → {}",
                fire_at.format("%Y-%m-%d %H:%M:%S UTC"),
                after,
                target.display_name
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

    /// Same shape as memory.rs's fixture — peer with registry, a single
    /// registered agent, and a blank session for ToolContext.
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
        let (_i, _r, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(index.clone());
        let list = HeartbeatList::new(index.clone());
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
        assert!(out.contains("alpha"), "target display missing: {out}");
        assert!(out.contains("check in"), "task missing: {out}");
    }

    #[tokio::test]
    async fn add_rejects_duplicate_id() {
        let (_i, _r, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(index);
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
        let (_i, _r, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(index);
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
        let (_i, _r, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(index);
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
        let (_i, _r, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(index.clone());
        let modify = HeartbeatModify::new(index.clone());
        let list = HeartbeatList::new(index);
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
        let (_i, _r, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(index.clone());
        let modify = HeartbeatModify::new(index);
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
        let (_i, _r, index, session) = fixture("alpha").await;
        let modify = HeartbeatModify::new(index);
        let ctx = make_ctx("alpha", session);

        let err = modify
            .execute(serde_json::json!({ "id": "missing", "task": "t" }), &ctx)
            .await
            .expect_err("unknown id should fail");
        assert!(format!("{err:?}").contains("No heartbeat rule"));
    }

    #[tokio::test]
    async fn modify_can_disable_and_reenable() {
        let (_i, _r, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(index.clone());
        let modify = HeartbeatModify::new(index.clone());
        let list = HeartbeatList::new(index);
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
    async fn remove_deletes_rule() {
        let (_i, _r, index, session) = fixture("alpha").await;
        let add = HeartbeatAdd::new(index.clone());
        let remove = HeartbeatRemove;
        let list = HeartbeatList::new(index);
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
        assert!(out.contains("No heartbeat rules"), "expected empty: {out}");
    }

    #[tokio::test]
    async fn remove_rejects_unknown_id() {
        let (_i, _r, _index, session) = fixture("alpha").await;
        let remove = HeartbeatRemove;
        let ctx = make_ctx("alpha", session);

        let err = remove
            .execute(serde_json::json!({ "id": "missing" }), &ctx)
            .await
            .expect_err("unknown remove should fail");
        assert!(format!("{err:?}").contains("No heartbeat rule"));
    }

    #[tokio::test]
    async fn list_empty_returns_friendly_message() {
        let (_i, _r, index, session) = fixture("alpha").await;
        let list = HeartbeatList::new(index);
        let ctx = make_ctx("alpha", session);

        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("No heartbeat rules"), "got: {out}");
    }

    #[tokio::test]
    async fn wake_me_up_writes_one_shot_rule_targeting_self() {
        let (_i, _r, index, session) = fixture("alpha").await;
        let wake = WakeMeUp::new(index);
        let ctx = make_ctx("alpha", session.clone());

        let before = chrono::Utc::now();
        let out = wake
            .execute(
                serde_json::json!({ "after_seconds": 60, "task": "check on build" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("Wakeup"), "unexpected msg: {out}");
        assert!(out.contains("alpha"), "should target self: {out}");

        let db = session.lock().await.database().clone();
        let routines = list_session_routines(&db).await.unwrap();
        assert_eq!(routines.len(), 1);
        let r = &routines[0];
        let fire_at = match r.trigger {
            Trigger::OneShot { fire_at } => fire_at,
            ref other => panic!("expected OneShot, got {other:?}"),
        };
        let delta = fire_at - before;
        // We asked for 60s; allow some slack for clock motion.
        assert!(
            delta.num_seconds() >= 59 && delta.num_seconds() <= 65,
            "fire_at off: {delta}"
        );
        let payload = payload_for(r);
        assert!(payload.is_one_shot);
        assert_eq!(payload.task, "check on build");
    }

    #[tokio::test]
    async fn wake_me_up_rejects_too_short() {
        let (_i, _r, index, session) = fixture("alpha").await;
        let wake = WakeMeUp::new(index);
        let ctx = make_ctx("alpha", session);

        let err = wake
            .execute(serde_json::json!({ "after_seconds": 5, "task": "t" }), &ctx)
            .await
            .expect_err("under-min should fail");
        assert!(format!("{err:?}").contains("after_seconds must be between"));
    }

    #[tokio::test]
    async fn wake_me_up_rejects_empty_task() {
        let (_i, _r, index, session) = fixture("alpha").await;
        let wake = WakeMeUp::new(index);
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
    async fn list_renders_one_shot_rule_with_fire_at() {
        let (_i, _r, index, session) = fixture("alpha").await;
        let wake = WakeMeUp::new(index.clone());
        let list = HeartbeatList::new(index);
        let ctx = make_ctx("alpha", session);

        wake.execute(
            serde_json::json!({ "after_seconds": 120, "task": "later thing" }),
            &ctx,
        )
        .await
        .unwrap();

        let out = list.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("wakeup-"), "id missing: {out}");
        // Format prefix "@YYYY-MM-DD ..." distinguishes one-shot from cron.
        assert!(out.contains("[@"), "fire_at marker missing: {out}");
        assert!(out.contains("later thing"), "task missing: {out}");
    }
}
