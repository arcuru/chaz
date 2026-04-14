use crate::role::RoleDetails;
use crate::runtime;
use crate::session::{EntryType, Session, SessionEntry};
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tracing::info;

/// Spawn a new agent to handle a task in a fresh session.
///
/// Creates a new session (thread), runs the named agent's ReAct loop,
/// and returns the result. The session persists in eidetica as a record.
pub struct SpawnAgent;

impl Tool for SpawnAgent {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Spawn a thread: creates a new session, runs the named agent with the given task, and returns the result. Use for delegating research, coding, or other focused work. Supports presets and per-field overrides."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Agent definition name (e.g. 'researcher', 'coder')"
                },
                "task": {
                    "type": "string",
                    "description": "What the agent should accomplish"
                },
                "context": {
                    "type": "string",
                    "description": "Optional background info appended to the agent's system prompt"
                },
                "preset": {
                    "type": "string",
                    "description": "Named preset from the agent definition (e.g. 'deep', 'quick', 'max')"
                },
                "model": {
                    "type": "string",
                    "description": "Override the model"
                },
                "max_iterations": {
                    "type": "integer",
                    "description": "Override max ReAct iterations"
                }
            },
            "required": ["agent", "task"]
        })
    }

    fn risk_level(&self, _params: &Value) -> RiskLevel {
        RiskLevel::Medium
    }

    fn requires_approval(&self, _params: &Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(300)
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            if ctx.call_depth >= ctx.max_call_depth {
                return Err(format!(
                    "Maximum spawn depth ({}) reached. Cannot spawn further agents.",
                    ctx.max_call_depth
                ));
            }

            let agent_name = arguments
                .get("agent")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'agent' argument".to_string())?;
            let task = arguments
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'task' argument".to_string())?;
            let context_str = arguments.get("context").and_then(|v| v.as_str());
            let preset = arguments.get("preset").and_then(|v| v.as_str());
            let model_override = arguments.get("model").and_then(|v| v.as_str());
            let max_iterations_override = arguments
                .get("max_iterations")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            if !ctx.agent_registry.can_spawn(&ctx.agent_name, agent_name) {
                return Err(format!(
                    "Agent '{}' is not allowed to spawn '{}'",
                    ctx.agent_name, agent_name
                ));
            }

            let agent_def = ctx
                .agent_registry
                .get(agent_name)
                .ok_or_else(|| format!("Unknown agent: '{agent_name}'"))?;

            let resolved = agent_def.resolve_overrides(
                preset,
                model_override,
                max_iterations_override,
                None,
            );

            info!(
                caller = %ctx.agent_name,
                target = %agent_name,
                depth = ctx.call_depth,
                "Spawning agent"
            );

            // Build system prompt: agent's role + optional context suffix
            let mut role = agent_def.default_role.clone();
            if let Some(suffix) = &resolved.role_suffix {
                if let Some(ref mut r) = role {
                    let existing = r.get_prompt();
                    *r = RoleDetails::new(
                        &agent_def.name,
                        None,
                        Some(format!("{existing}\n\n{suffix}")),
                        None,
                    );
                }
            }
            if let Some(ctx_str) = context_str {
                if let Some(ref mut r) = role {
                    let existing = r.get_prompt();
                    *r = RoleDetails::new(
                        &agent_def.name,
                        None,
                        Some(format!("{existing}\n\nContext: {ctx_str}")),
                        None,
                    );
                }
            }

            // Create a fresh session for this spawned agent
            let session_id = crate::types::ConversationId(uuid::Uuid::new_v4().to_string());
            let mut session = Session::new_ephemeral(session_id, ctx.database.clone()).await;

            // Add the task as the first entry (from the calling agent)
            session
                .add_entry(SessionEntry {
                    sender: ctx.agent_name.clone(),
                    content: task.to_string(),
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                })
                .await;

            // Build context from the session
            let chat_context = session.build_context(agent_name, role, resolved.model);

            let filtered =
                ctx.tool_registry
                    .filtered_view(resolved.allowed_tools.as_deref());

            let child_security = crate::security::SecurityContext {
                leak_detector: ctx.security.leak_detector.clone(),
                auto_approved_tools: ctx.security.auto_approved_tools.clone(),
                approval_callback: ctx.security.approval_callback.clone(),
            };

            let child_ctx = ToolContext {
                agent_name: agent_name.to_string(),
                call_depth: ctx.call_depth + 1,
                max_call_depth: ctx.max_call_depth,
                agent_registry: ctx.agent_registry.clone(),
                tool_registry: ctx.tool_registry.clone(),
                backend: ctx.backend.clone(),
                security: child_security.clone(),
                database: ctx.database.clone(),
            };

            let result = runtime::execute(
                &chat_context,
                &ctx.backend,
                &filtered,
                &child_security,
                &child_ctx,
            )
            .await;

            // Store the response/error in the session for audit trail
            match &result {
                Ok(response) => {
                    session
                        .add_entry(SessionEntry {
                            sender: agent_name.to_string(),
                            content: response.clone(),
                            timestamp: chrono::Utc::now(),
                            entry_type: EntryType::Message,
                        })
                        .await;
                }
                Err(error) => {
                    session
                        .add_entry(SessionEntry {
                            sender: agent_name.to_string(),
                            content: format!("Agent error: {error}"),
                            timestamp: chrono::Utc::now(),
                            entry_type: EntryType::Error,
                        })
                        .await;
                }
            }

            result
        })
    }
}
