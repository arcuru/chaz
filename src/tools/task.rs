use crate::backends::BackendManager;
use crate::security::SecurityContext;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use tracing::{info, warn};

/// Spawn a one-shot ephemeral Task in a fresh child session.
///
/// Living Agents Stage 5: generates a fresh keypair on this peer, creates a
/// child session with parent→child delegation wired in, grants the fresh
/// pubkey Write(100) on the child session, runs the ReAct loop, then
/// **revokes** the key on completion. The session DB persists as a permanent
/// audit record; no new writes can be made under the revoked key.
///
/// Use for focused work with no persistent identity. For delegating to a
/// long-lived agent whose memory and config survive across runs, use
/// `spawn_agent`.
pub struct SpawnTask {
    pub server: Arc<OnceLock<Arc<Server>>>,
    pub backend: BackendManager,
    pub security: SecurityContext,
}

impl Tool for SpawnTask {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_task".to_string(),
            description: "Run a one-shot ephemeral task in a fresh child session. Generates a new identity, runs the task, then revokes the identity — the session persists as an audit record but the task can't be resurrected. Inherits the caller's model/role/tools unless overridden. For delegating to a named persistent agent, use spawn_agent.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "What the task should accomplish"
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background info appended to the task as additional context"
                    },
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of tool names the task is allowed to use (narrows the caller's tool scope). Omit to inherit the caller's full scope."
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
                        "description": "If true, spawn and return immediately without waiting for the result."
                    }
                },
                "required": ["task"]
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
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let server = self
                .server
                .get()
                .ok_or_else(|| "SpawnTask: server not initialized".to_string())?;

            if ctx.call_depth >= ctx.max_call_depth {
                return Err(format!(
                    "Maximum spawn depth ({}) reached. Cannot spawn further tasks.",
                    ctx.max_call_depth
                )
                .into());
            }

            let task = arguments
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'task' argument".to_string())?;
            let context_str = arguments.get("context").and_then(|v| v.as_str());
            let tools_override: Option<Vec<String>> = arguments
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                });
            let _model_override = arguments.get("model").and_then(|v| v.as_str());
            let max_iterations_override = arguments
                .get("max_iterations")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let is_async = arguments
                .get("async")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // Tasks inherit the caller's agent config (model, role, tools).
            // Agent-level overrides for model/max_iterations come through the
            // running agent; `spawn_task` doesn't create a new agent definition.
            let caller_agent = server
                .agents()
                .get(&ctx.agent_name)
                .ok_or_else(|| format!("Caller agent '{}' not found", ctx.agent_name))?;
            let child_max_depth = max_iterations_override
                .map(|n| n as usize)
                .unwrap_or(caller_agent.max_iterations as usize);

            let parent_session_db_id = {
                let s = ctx.session.lock().await;
                s.database().root_id().to_string()
            };

            info!(
                caller = %ctx.agent_name,
                depth = ctx.call_depth,
                parent_session = %parent_session_db_id,
                "Spawning ephemeral Task via server"
            );

            // Register child session with parent→child delegation wired in.
            let (conversation_id, session_db, mut completion_rx) = server
                .register_child_session(
                    &ctx.agent_name,
                    self.backend.clone(),
                    self.security.approval_callback.clone(),
                    ctx.call_depth + 1,
                    child_max_depth,
                    ctx.tools.narrow(tools_override.as_deref()),
                    Some(&parent_session_db_id),
                )
                .await
                .map_err(|e| format!("Failed to create child session: {e}"))?;

            let child_session_db_id = conversation_id.0.clone();

            // Generate ephemeral keypair, grant Write(100) on child session.
            // The ephemeral key is the logical signer for this task — at
            // completion we'll revoke it so no further writes are possible.
            let key_label = format!(
                "task:{}",
                &child_session_db_id[..12.min(child_session_db_id.len())]
            );
            let ephemeral_pubkey = server
                .registry()
                .new_ephemeral_key(&key_label)
                .await
                .map_err(|e| format!("Failed to generate ephemeral key: {e}"))?;

            server
                .registry()
                .grant_write_on_session(&child_session_db_id, &ephemeral_pubkey, &key_label, 100)
                .await
                .map_err(|e| format!("Failed to authorize ephemeral key: {e}"))?;

            // Build directive: task + optional context
            let mut directive = task.to_string();
            if let Some(ctx_str) = context_str {
                directive = format!("{directive}\n\nContext: {ctx_str}");
            }

            let mut session = Session::new(conversation_id.clone(), session_db).await;
            session
                .add_entry(SessionEntry {
                    sender: ctx.agent_name.clone(),
                    content: directive,
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Directive,
                    metadata: None,
                })
                .await;

            if is_async {
                // Fire-and-forget: we cannot revoke here because the task is
                // still running. Async task keys stay live until manually
                // cleaned up; sync is the primary path.
                return Ok(format!(
                    "Task spawned asynchronously in session {child_session_db_id} (ephemeral key kept live — use sync spawn for revoke-on-completion)"
                ));
            }

            // Wait for agent completion.
            let completion = completion_rx.recv().await;

            // Revoke the ephemeral key regardless of whether the task
            // succeeded — the task is over, the key should not live on.
            if let Err(e) = server
                .registry()
                .revoke_key_on_session(&child_session_db_id, &ephemeral_pubkey)
                .await
            {
                warn!(
                    session = %child_session_db_id,
                    "Failed to revoke ephemeral task key: {e}"
                );
            }

            completion.ok_or_else(|| "Task dropped without completing".to_string())?;

            // Re-read session for the response.
            let session = Session::new(conversation_id, session.database().clone()).await;
            match session.latest_entry() {
                Some(e) if e.entry_type == EntryType::Message => Ok(e.content.clone()),
                Some(e) if e.entry_type == EntryType::Error => Err(e.content.clone().into()),
                _ => Err("No response from task".into()),
            }
        })
    }
}
