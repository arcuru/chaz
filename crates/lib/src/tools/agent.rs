use crate::session::TurnRequestId;
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Submit a one-shot job for another Agent hosted by this executor peer.
/// The parent turn and model-call site form a stable submission key; the
/// child session DB ID is returned after durable acceptance, not completion.
pub struct SpawnAgent {
    pub server: crate::instance::ServerSlot,
}

impl Tool for SpawnAgent {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_agent".to_string(),
            description: "Submit a durable local Agent job and return its session DB handle immediately. Use job_status or job_wait to observe completion. No worktree/private scope or per-call model overrides in this version.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_ref": {
                        "type": "string",
                        "description": "Target Agent display name or DB ID hosted on this executor."
                    },
                    "task": {
                        "type": "string",
                        "description": "Work to submit to the target Agent."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background appended to the task."
                    }
                },
                "required": ["agent_ref", "task"],
                "additionalProperties": false
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
            let args = arguments
                .as_object()
                .ok_or_else(|| "spawn_agent needs an object".to_string())?;
            if let Some(unknown) = args
                .keys()
                .find(|key| !matches!(key.as_str(), "agent_ref" | "agent" | "task" | "context"))
            {
                return Err(format!(
                    "Unsupported spawn_agent argument '{unknown}': narrow/workspace scope, overrides and synchronous execution are unavailable"
                )
                .into());
            }
            let agent_ref = args
                .get("agent_ref")
                .or_else(|| args.get("agent"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "Missing 'agent_ref' argument".to_string())?;
            let task = args
                .get("task")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "Missing 'task' argument".to_string())?;
            let directive = match args.get("context").and_then(Value::as_str) {
                Some(context) if !context.is_empty() => format!("{task}\n\nContext: {context}"),
                _ => task.to_string(),
            };
            let turn_id = ctx
                .turn_request_id
                .as_ref()
                .ok_or_else(|| "spawn_agent needs a persisted parent turn".to_string())?;
            let call_key = ctx
                .tool_call_key
                .as_deref()
                .ok_or_else(|| "spawn_agent needs a stable model tool-call key".to_string())?;
            let (parent_id, created_at) = {
                let session = ctx.session.lock().await;
                let (_, entry) = session
                    .entries_with_ids()
                    .find(|(id, _)| *id == turn_id)
                    .ok_or_else(|| "parent turn is not persisted".to_string())?;
                (session.database().root_id().to_string(), entry.timestamp)
            };
            let command_id = TurnRequestId::parse(format!("job:{turn_id}:{call_key}"));
            let session_db_id = server
                .submit_agent_job_inline(
                    &parent_id,
                    command_id,
                    &ctx.agent_name,
                    created_at,
                    agent_ref,
                    &directive,
                )
                .await
                .map_err(|error| format!("Job submission failed: {error}"))?;
            Ok(serde_json::json!({
                "session_db_id": session_db_id,
                "state": "accepted"
            })
            .to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fresh_session, tool_context};
    use crate::tool::ToolRegistry;
    use std::sync::Arc;

    #[tokio::test]
    async fn descriptor_advertises_immediate_job_submission() {
        let tool = SpawnAgent {
            server: crate::instance::ServerSlot::default(),
        };
        let descriptor = tool.descriptor();
        assert_eq!(descriptor.name, "spawn_agent");
        assert_eq!(
            descriptor.parameters["required"],
            serde_json::json!(["agent_ref", "task"])
        );
        assert_eq!(descriptor.parameters["additionalProperties"], false);
    }

    #[tokio::test]
    async fn default_policy_requires_medium_risk_approval() {
        let tool = SpawnAgent {
            server: crate::instance::ServerSlot::default(),
        };
        let policy = tool.default_policy();
        assert!(matches!(policy.risk, RiskLevel::Medium));
        assert!(matches!(
            policy.approval,
            ApprovalRequirement::UnlessAutoApproved
        ));
    }

    #[tokio::test]
    async fn no_server_is_not_accepted() {
        let tool = SpawnAgent {
            server: crate::instance::ServerSlot::default(),
        };
        let (_instance, session) = fresh_session().await;
        let ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        let error = tool
            .execute(
                serde_json::json!({"agent_ref": "researcher", "task": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(format!("{error}").contains("server not initialized"));
    }
}
