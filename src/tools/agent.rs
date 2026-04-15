use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use tracing::info;

use crate::backends::BackendManager;
use crate::security::SecurityContext;

/// Spawn a new agent to handle a task in a child session.
///
/// Creates a new session via the server, writes a Directive entry,
/// and waits for the server's callback-driven processing to run the
/// agent and write the response. The response is then returned to the
/// calling agent.
///
/// This routes through the same server processing loop as gateway messages,
/// unifying all agent invocation paths.
pub struct SpawnAgent {
    pub server: Arc<OnceLock<Arc<Server>>>,
    pub backend: BackendManager,
    pub security: SecurityContext,
}

impl Tool for SpawnAgent {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_agent".to_string(),
            description: "Spawn a thread: creates a new session, runs the named agent with the given task, and returns the result. Use for delegating research, coding, or other focused work. Supports presets and per-field overrides.".to_string(),
            parameters: serde_json::json!({
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
                    },
                    "async": {
                        "type": "boolean",
                        "description": "If true, spawn the agent and return immediately without waiting for the result. The agent runs in the background."
                    }
                },
                "required": ["agent", "task"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Medium,
            approval: ApprovalRequirement::UnlessAutoApproved,
            timeout: 300,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let server = self
                .server
                .get()
                .ok_or_else(|| "SpawnAgent: server not initialized".to_string())?;

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
            let is_async = arguments
                .get("async")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if !server.agents().can_spawn(&ctx.agent_name, agent_name) {
                return Err(format!(
                    "Agent '{}' is not allowed to spawn '{}'",
                    ctx.agent_name, agent_name
                ));
            }

            let agent_def = server
                .agents()
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
                "Spawning agent via server"
            );

            let child_max_depth = resolved.max_iterations as usize;

            // Register a child session with the server
            let (_transport_id, conversation_id, session_db, mut completion_rx) = server
                .register_child_session(
                    agent_name,
                    self.backend.clone(),
                    self.security.approval_callback.clone(),
                    ctx.call_depth + 1,
                    child_max_depth,
                    ctx.tools.clone(),
                )
                .await
                .map_err(|e| format!("Failed to create child session: {e}"))?;

            // Build the directive content: task + optional context
            let mut directive = task.to_string();
            if let Some(ctx_str) = context_str {
                directive = format!("{directive}\n\nContext: {ctx_str}");
            }

            // Write the directive entry to trigger agent execution
            let mut session = Session::new(conversation_id.clone(), session_db).await;
            session
                .add_entry(SessionEntry {
                    sender: ctx.agent_name.clone(),
                    content: directive,
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Directive,
                })
                .await;

            if is_async {
                // Fire-and-forget: return immediately, agent runs in background
                return Ok(format!(
                    "Agent '{agent_name}' spawned asynchronously in session {}",
                    conversation_id.0
                ));
            }

            // Synchronous: wait for the server to process and the agent to complete
            completion_rx
                .recv()
                .await
                .ok_or_else(|| "Child agent task dropped without completing".to_string())?;

            // Re-read the session to get the response
            let session = Session::new(conversation_id, session.database().clone()).await;
            match session.latest_entry() {
                Some(e) if e.entry_type == EntryType::Message => Ok(e.content.clone()),
                Some(e) if e.entry_type == EntryType::Error => Err(e.content.clone()),
                _ => Err("No response from child agent".to_string()),
            }
        })
    }
}
