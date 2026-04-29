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
//! Rules live in the session DB (via `crate::heartbeat::{upsert_rule,
//! remove_rule, list_rules}`), so they sync and survive restarts. `HeartbeatRunner`
//! picks them up on its next tick.

use crate::heartbeat::{list_rules, remove_rule, upsert_rule, HeartbeatRule};
use crate::hosted_index::{DbEntry, HostedIndex};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy};
use cron::Schedule;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

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
    if let Ok(id) = eidetica::entry::ID::parse(name) {
        if let Some(entry) = index.find_by_id(&id) {
            return Ok(entry);
        }
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

            let existing = list_rules(db)
                .await
                .map_err(|e| format!("Failed to read rules: {e}"))?;
            if existing.iter().any(|r| r.id == id) {
                return Err(format!(
                    "Heartbeat rule '{id}' already exists; use heartbeat_modify to edit or heartbeat_remove first"
                )
                .into());
            }

            let rule = HeartbeatRule {
                id: id.to_string(),
                name: id.to_string(),
                cron: cron.to_string(),
                task: task.to_string(),
                target_agent_db_id: target.db_id.to_string(),
                enabled: true,
            };
            upsert_rule(db, rule)
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

            let mut rule = list_rules(db)
                .await
                .map_err(|e| format!("Failed to read rules: {e}"))?
                .into_iter()
                .find(|r| r.id == id)
                .ok_or_else(|| format!("No heartbeat rule with id '{id}' on this session"))?;

            if let Some(c) = new_cron {
                rule.cron = c.to_string();
            }
            if let Some(t) = new_task {
                rule.task = t.to_string();
            }
            let target_display = if let Some(a) = new_agent {
                let target = resolve_target_agent(ctx, &self.agent_index, Some(a))?;
                rule.target_agent_db_id = target.db_id.to_string();
                Some(target.display_name)
            } else {
                None
            };
            if let Some(e) = new_enabled {
                rule.enabled = e;
            }

            upsert_rule(db, rule.clone())
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
            match remove_rule(db, id).await {
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
            let rules = list_rules(db)
                .await
                .map_err(|e| format!("Failed to list rules: {e}"))?;
            if rules.is_empty() {
                return Ok("No heartbeat rules on this session.".to_string());
            }
            let mut lines = Vec::with_capacity(rules.len() + 1);
            lines.push("Heartbeat rules:".to_string());
            for r in &rules {
                let target_display = match eidetica::entry::ID::parse(&r.target_agent_db_id) {
                    Ok(id) => self
                        .agent_index
                        .find_by_id(&id)
                        .map(|e| e.display_name)
                        .unwrap_or_else(|| r.target_agent_db_id.clone()),
                    Err(_) => r.target_agent_db_id.clone(),
                };
                let state = if r.enabled { "" } else { " (disabled)" };
                lines.push(format!(
                    "- **{}** [{}]{state} → {} — {}",
                    r.id, r.cron, target_display, r.task
                ));
            }
            Ok(lines.join("\n"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{create_agent_db, AgentDbConfig, AgentMeta};
    use crate::hosted_index::{DbEntry, HostedIndex};
    use crate::session::{Session, SessionRegistry};
    use crate::tool::{ScopedTools, ToolContext, ToolProfile, ToolRegistry};
    use crate::types::ConversationId;
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;
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
        let agents_reg = Arc::new(AgentRegistry::from_config(&blank_config()));
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

    fn blank_config() -> crate::config::Config {
        crate::config::Config::default()
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
}
