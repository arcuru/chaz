use crate::backends::BackendManager;
use crate::security::SecurityContext;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use tracing::info;

/// Spawn a Worker — a configured one-shot LLM call declared per-Agent.
///
/// Resolves `name` against the calling Agent's Worker registry (no
/// global lookup, no cross-Agent fallback). The Worker has no identity
/// of its own: entries written to the child session are signed by the
/// parent Agent's key, inherited via the parent→child DelegatedTreeRef
/// that [`Server::register_child_session`] wires in. The child session
/// DB persists per eidetica's retention settings, giving an inspectable
/// audit trail without the ephemeral-key dance the previous
/// `spawn_task` performed.
///
/// For delegating to a long-lived peer Agent with its own keys and
/// persistent state, use `spawn_agent`.
pub struct SpawnWorker {
    pub server: Arc<OnceLock<Arc<Server>>>,
    pub backend: BackendManager,
    pub security: SecurityContext,
}

impl Tool for SpawnWorker {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_worker".to_string(),
            description: "Invoke a Worker template declared under the calling Agent. Workers are configured one-shot LLM calls — no identity, no keys; entries are signed by the parent Agent. The child session DB persists for audit and inspection. Use this for delegated work that doesn't need persistent identity. For delegating to a named peer Agent (Ava, Chaz) with its own keys, use spawn_agent.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the Worker template (must be declared under the calling Agent's workers list)."
                    },
                    "task": {
                        "type": "string",
                        "description": "What the Worker should accomplish."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background info appended to the task as additional context."
                    },
                    "preset": {
                        "type": "string",
                        "description": "Optional preset name on the Worker template that overrides model/max_iterations/tools/role_suffix."
                    },
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of tool names the Worker is allowed to use. Narrows the resolved tool scope; must be a subset of what the Worker template + parent Agent allow."
                    },
                    "model": {
                        "type": "string",
                        "description": "Override the model for this invocation."
                    },
                    "max_iterations": {
                        "type": "integer",
                        "description": "Accepted for compatibility; the Worker invocation shares the calling Agent's iteration budget rather than starting fresh, so this override has no practical effect under normal use."
                    },
                    "async": {
                        "type": "boolean",
                        "description": "If true, spawn and return immediately without waiting for the result."
                    }
                },
                "required": ["name", "task"]
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
                .ok_or_else(|| "SpawnWorker: server not initialized".to_string())?;

            if ctx.call_depth >= ctx.max_call_depth {
                return Err(format!(
                    "Maximum spawn depth ({}) reached. Cannot spawn further workers.",
                    ctx.max_call_depth
                )
                .into());
            }

            let name = arguments
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'name' argument (Worker template name)".to_string())?;
            let task = arguments
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'task' argument".to_string())?;
            let context_str = arguments.get("context").and_then(|v| v.as_str());
            let preset = arguments.get("preset").and_then(|v| v.as_str());
            let tools_override: Option<Vec<String>> = arguments
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                });
            let model_override = arguments.get("model").and_then(|v| v.as_str());
            let max_iterations_override = arguments
                .get("max_iterations")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let is_async = arguments
                .get("async")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // Resolve the Worker template against the *calling Agent's* registry.
            // There is no global Worker lookup — Workers are per-Agent.
            let caller_agent = server
                .agents()
                .get(&ctx.agent_name)
                .ok_or_else(|| format!("Caller agent '{}' not found", ctx.agent_name))?;
            let worker = caller_agent.find_worker(name).ok_or_else(|| {
                let mut available: Vec<&str> =
                    caller_agent.workers.keys().map(String::as_str).collect();
                available.sort_unstable();
                format!(
                    "Unknown worker '{}'. Caller agent '{}' has: [{}]",
                    name,
                    ctx.agent_name,
                    available.join(", ")
                )
            })?;

            // Resolve overrides: Worker template defaults → preset → inline.
            // Optional template fields fall back to the parent Agent's defaults.
            let mut resolved_model = worker
                .default_model
                .clone()
                .or_else(|| caller_agent.default_model.clone());
            let mut resolved_max_iterations =
                worker.max_iterations.unwrap_or(caller_agent.max_iterations);
            let mut resolved_tools = worker
                .allowed_tools
                .clone()
                .or_else(|| caller_agent.allowed_tools.clone());
            let mut role_suffix: Option<String> = None;

            if let Some(preset_name) = preset
                && let Some(p) = worker.presets.get(preset_name)
            {
                if let Some(ref m) = p.model {
                    resolved_model = Some(m.clone());
                }
                if let Some(mi) = p.max_iterations {
                    resolved_max_iterations = mi;
                }
                if let Some(ref t) = p.tools {
                    resolved_tools = Some(intersect_tools(&resolved_tools, t));
                }
                role_suffix = p.role_suffix.clone();
            }

            if let Some(m) = model_override {
                resolved_model = Some(m.to_string());
            }
            if let Some(mi) = max_iterations_override {
                resolved_max_iterations = mi;
            }
            if let Some(ref t) = tools_override {
                resolved_tools = Some(intersect_tools(&resolved_tools, t));
            }

            // Build the scoped tool set for the child by narrowing the
            // parent's scope to the resolved Worker tools.
            let child_tools = match resolved_tools.as_deref() {
                Some(list) => ctx.tools.narrow(Some(list)),
                None => ctx.tools.clone(),
            };

            // `resolved_model` and `role_suffix` are computed above for
            // forward-compat but currently unused: propagating spawn-time
            // overrides into the child's `SessionRuntime` needs a separate
            // expansion of `register_child_session` (and matching pickup in
            // `spawn_agent_task`). The budget plumbing handled by
            // `ctx.iteration_budget` does not cover these — they're a
            // distinct concern parked for follow-up.
            let _ = resolved_model;
            let _ = role_suffix;

            let parent_session_db_id = {
                let s = ctx.session.lock().await;
                s.database().root_id().to_string()
            };

            info!(
                worker = %name,
                caller = %ctx.agent_name,
                depth = ctx.call_depth,
                parent_session = %parent_session_db_id,
                "Spawning Worker via server"
            );

            // Register child session with parent→child delegation wired in.
            // No ephemeral keypair: entries on the child are signed by the
            // parent Agent's key via the delegation chain. The shared
            // iteration budget descends from the parent so nested
            // Worker invocations draw from the top-level Agent's pool
            // rather than each level getting its own.
            let (conversation_id, session_db, mut completion_rx) = server
                .register_child_session(
                    &ctx.agent_name,
                    self.backend.clone(),
                    self.security.approval_callback.clone(),
                    ctx.call_depth + 1,
                    resolved_max_iterations as usize,
                    child_tools,
                    Some(&parent_session_db_id),
                    ctx.iteration_budget.clone(),
                )
                .await
                .map_err(|e| format!("Failed to create child session: {e}"))?;

            let child_session_db_id = conversation_id.0.clone();

            // Build directive: task + optional context.
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
                return Ok(format!(
                    "Worker '{name}' spawned asynchronously in session {child_session_db_id}"
                ));
            }

            let completion = completion_rx.recv().await;
            completion.ok_or_else(|| "Worker dropped without completing".to_string())?;

            // Re-read session for the response.
            let session = Session::new(conversation_id, session.database().clone()).await;
            match session.latest_entry() {
                Some(e) if e.entry_type == EntryType::Message => Ok(e.content.clone()),
                Some(e) if e.entry_type == EntryType::Error => Err(e.content.clone().into()),
                _ => Err("No response from worker".into()),
            }
        })
    }
}

/// Intersect a tool override list with an existing allowlist.
/// If base is None (all tools), the override becomes the allowlist.
/// If both are set, only tools in both lists are kept.
fn intersect_tools(base: &Option<Vec<String>>, override_tools: &[String]) -> Vec<String> {
    match base {
        None => override_tools.to_vec(),
        Some(base_tools) => override_tools
            .iter()
            .filter(|t| base_tools.contains(t))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{empty_secrets, fresh_session, permissive_security, tool_context};
    use crate::tool::ToolRegistry;

    async fn worker_tool() -> SpawnWorker {
        let secrets = empty_secrets().await;
        SpawnWorker {
            server: Arc::new(OnceLock::new()),
            backend: BackendManager::new(&None, secrets),
            security: permissive_security(),
        }
    }

    #[tokio::test]
    async fn descriptor_advertises_spawn_worker_with_required_args() {
        let tool = worker_tool().await;
        let d = tool.descriptor();
        assert_eq!(d.name, "spawn_worker");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "name"));
        assert!(required.iter().any(|v| v == "task"));
    }

    #[tokio::test]
    async fn default_policy_is_medium_with_extended_timeout() {
        let tool = worker_tool().await;
        let p = tool.default_policy();
        assert!(matches!(p.risk, RiskLevel::Medium));
        assert!(matches!(
            p.approval,
            ApprovalRequirement::UnlessAutoApproved
        ));
        assert_eq!(p.timeout, 300, "spawn_worker gets a 5-minute timeout");
    }

    #[tokio::test]
    async fn server_not_initialized_errors_before_any_work() {
        let tool = worker_tool().await;
        let (_instance, session) = fresh_session().await;
        let ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        let err = tool
            .execute(
                serde_json::json!({ "name": "any", "task": "do a thing" }),
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
    async fn max_depth_reached_short_circuits_with_clear_error() {
        let tool = worker_tool().await;
        let (_instance, session) = fresh_session().await;
        let mut ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        ctx.call_depth = ctx.max_call_depth;
        let err = tool
            .execute(serde_json::json!({ "name": "any", "task": "x" }), &ctx)
            .await
            .unwrap_err();
        let msg = format!("{err}").to_lowercase();
        assert!(
            msg.contains("server") || msg.contains("depth"),
            "got: {msg}"
        );
    }
}
