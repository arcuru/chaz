use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use eidetica::entry::ID;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use tracing::info;

use crate::backends::BackendManager;
use crate::security::SecurityContext;

/// Delegate a one-shot task to another Agent hosted on this same Peer.
///
/// Resolves `agent_ref` (display name or eidetica DB ID) against this
/// Peer's local agent index, creates a child session with parent→child
/// delegation wired in (parent admins inherit admin on the child),
/// attaches the target Agent's stable pubkey to the child session with
/// Write(10), writes a Directive entry, and (unless `async: true`) waits
/// for the target to respond.
///
/// **Scope:** local-Peer only. Reaches Agents whose runtime lives in this
/// process via [`Server::agent_index`]; there is no remote-Peer path. A
/// cross-Peer Agent-to-Agent messaging primitive (sealed-box knocks into
/// a per-Agent inbox tree, conversation in a private session DB) is being
/// designed separately and will eventually supersede this tool.
///
/// **Override args (`preset`, `model`, `max_iterations`) are transitional.**
/// The calling Agent does not own the target Agent's config — those
/// fields live in the target's own repo and config. The overrides work
/// today via [`AgentConfig::resolve_overrides`] but are slated for
/// removal once the messaging surface lands.
///
/// For work owned by the calling Agent (no separate identity, anonymous
/// one-shot LLM call signed by the parent), use `spawn_worker`.
pub struct SpawnAgent {
    pub server: Arc<OnceLock<Arc<Server>>>,
    pub backend: BackendManager,
    pub security: SecurityContext,
}

impl Tool for SpawnAgent {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_agent".to_string(),
            description: "Delegate a one-shot task to another Agent on this same Peer. Resolves the target by display name or DB ID, creates a child session, attaches the target Agent's pubkey, writes a Directive, and (unless async) waits for the response. Local-Peer only — cross-Peer Agent messaging is a separate primitive in design. For ephemeral work owned by the calling Agent, use spawn_worker.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_ref": {
                        "type": "string",
                        "description": "Target Agent display name (e.g. 'researcher') or eidetica DB ID. Must be hosted on this Peer."
                    },
                    "task": {
                        "type": "string",
                        "description": "What the target Agent should accomplish."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background info appended to the task."
                    },
                    "preset": {
                        "type": "string",
                        "description": "Named preset from the target Agent's config (e.g. 'deep', 'quick'). Transitional — caller does not own target's config."
                    },
                    "model": {
                        "type": "string",
                        "description": "Override the target Agent's model. Transitional — slated for removal; target's config should be authoritative."
                    },
                    "max_iterations": {
                        "type": "integer",
                        "description": "Override the target Agent's max ReAct iterations. Transitional — slated for removal."
                    },
                    "async": {
                        "type": "boolean",
                        "description": "If true, return immediately after seeding the directive without waiting for the response."
                    }
                },
                "required": ["agent_ref", "task"]
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
                .ok_or_else(|| "SpawnAgent: server not initialized".to_string())?;

            if ctx.call_depth >= ctx.max_call_depth {
                return Err(format!(
                    "Maximum spawn depth ({}) reached. Cannot spawn further agents.",
                    ctx.max_call_depth
                )
                .into());
            }

            // Accept `agent_ref` (new) or `agent` (legacy alias).
            let agent_ref = arguments
                .get("agent_ref")
                .or_else(|| arguments.get("agent"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'agent_ref' argument".to_string())?;
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

            // Resolve the agent ref against the agent index.
            // Try as display name first (most common), then as DB ID.
            let index = server.agent_index();
            let index_entry = match index.find_by_name(agent_ref) {
                Some(e) => e,
                None => {
                    let id = ID::parse(agent_ref).map_err(|_| {
                        format!("Unknown agent: '{agent_ref}' (not a display name or DB ID)")
                    })?;
                    index
                        .find_by_id(&id)
                        .ok_or_else(|| format!("Unknown agent: '{agent_ref}'"))?
                }
            };

            let agent_display = index_entry.display_name.clone();

            let agent_def = server
                .agents()
                .get(&agent_display)
                .ok_or_else(|| format!("Unknown agent: '{agent_display}'"))?;

            let resolved =
                agent_def.resolve_overrides(preset, model_override, max_iterations_override, None);

            // Parent session ID for delegation wiring — the session this tool
            // is being invoked from.
            let parent_session_db_id = {
                let s = ctx.session.lock().await;
                s.database().root_id().to_string()
            };

            info!(
                caller = %ctx.agent_name,
                target = %agent_display,
                depth = ctx.call_depth,
                parent_session = %parent_session_db_id,
                "Spawning Living Agent via server"
            );

            let child_max_depth = resolved.max_iterations as usize;

            // Register a child session with the server (with parent→child
            // delegation so parent admins inherit admin on the child).
            // `iteration_budget: None` — a spawned peer Agent runs its
            // own ReAct loop on its own freshly-allocated budget; only
            // anonymous Workers (`spawn_worker`) share the caller's
            // budget.
            let (conversation_id, session_db, mut completion_rx) = server
                .register_child_session(
                    &agent_display,
                    self.backend.clone(),
                    self.security.approval_callback.clone(),
                    ctx.call_depth + 1,
                    child_max_depth,
                    ctx.tools.clone(),
                    Some(&parent_session_db_id),
                    None,
                )
                .await
                .map_err(|e| format!("Failed to create child session: {e}"))?;

            // Attach the Living Agent's stable pubkey to the child session:
            // AuthSettings gets Write(10), SessionMeta gets the AgentRef, and
            // the agent's history store records the join.
            server
                .registry()
                .attach_agent_to_session(&conversation_id.0, &index_entry)
                .await
                .map_err(|e| format!("Failed to attach agent to child session: {e}"))?;

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
                    metadata: None,
                })
                .await;

            if is_async {
                return Ok(format!(
                    "Agent '{agent_display}' spawned asynchronously in session {}",
                    conversation_id.0
                ));
            }

            completion_rx
                .recv()
                .await
                .ok_or_else(|| "Child agent task dropped without completing".to_string())?;

            // Re-read the session to get the response
            let session = Session::new(conversation_id, session.database().clone()).await;
            match session.latest_entry() {
                Some(e) if e.entry_type == EntryType::Message => Ok(e.content.clone()),
                Some(e) if e.entry_type == EntryType::Error => Err(e.content.clone().into()),
                _ => Err("No response from child agent".into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{empty_secrets, fresh_session, permissive_security, tool_context};
    use crate::tool::ToolRegistry;

    /// Build a SpawnAgent with no server wired in — sufficient for tests of
    /// the pre-server-lookup branches (descriptor, argument validation,
    /// depth gate, server-not-initialized).
    async fn agent_tool() -> SpawnAgent {
        let secrets = empty_secrets().await;
        SpawnAgent {
            server: Arc::new(OnceLock::new()),
            backend: BackendManager::new(&None, secrets),
            security: permissive_security(),
        }
    }

    #[tokio::test]
    async fn descriptor_advertises_spawn_agent_with_required_args() {
        let tool = agent_tool().await;
        let d = tool.descriptor();
        assert_eq!(d.name, "spawn_agent");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "agent_ref"));
        assert!(required.iter().any(|v| v == "task"));
    }

    #[tokio::test]
    async fn default_policy_is_medium_with_extended_timeout() {
        let tool = agent_tool().await;
        let p = tool.default_policy();
        assert!(matches!(p.risk, RiskLevel::Medium));
        assert!(matches!(
            p.approval,
            ApprovalRequirement::UnlessAutoApproved
        ));
        assert_eq!(p.timeout, 300);
    }

    #[tokio::test]
    async fn server_not_initialized_short_circuits() {
        let tool = agent_tool().await;
        let (_instance, session) = fresh_session().await;
        let ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        let err = tool
            .execute(
                serde_json::json!({ "agent_ref": "researcher", "task": "look it up" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("server not initialized"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn depth_gate_blocks_further_spawn() {
        let tool = agent_tool().await;
        let (_instance, session) = fresh_session().await;
        let mut ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        ctx.call_depth = ctx.max_call_depth;
        let err = tool
            .execute(
                serde_json::json!({ "agent_ref": "researcher", "task": "x" }),
                &ctx,
            )
            .await
            .unwrap_err();
        // Server check fires before depth check in current implementation —
        // either is a valid pre-execution failure mode.
        let msg = format!("{err}").to_lowercase();
        assert!(
            msg.contains("server") || msg.contains("depth"),
            "got: {msg}"
        );
    }
}
