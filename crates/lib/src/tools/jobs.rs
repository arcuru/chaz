//! Observation tools for durable Agent jobs and workflow parents.
use crate::instance::ServerSlot;
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub struct JobStatusTool {
    pub server: ServerSlot,
}

pub struct JobWaitTool {
    pub server: ServerSlot,
}

fn session_id(arguments: &Value) -> Result<&str, ToolError> {
    arguments
        .get("session_db_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Missing 'session_db_id' job handle".into())
}

impl Tool for JobStatusTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_status".into(),
            description:
                "Read the durable state and typed result of an Agent job or workflow parent by session DB handle."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"session_db_id": {"type": "string", "description": "Job session DB handle."}},
                "required": ["session_db_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let id = session_id(&arguments)?;
            let server = self
                .server
                .get()
                .ok_or_else(|| "Server not initialized".to_string())?;
            let status = server
                .observe_job_status(id)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_string(&status).map_err(|e| e.to_string().into())
        })
    }
}

impl Tool for JobWaitTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_wait".into(),
            description: "Wait up to a bounded deadline for an Agent job or workflow parent; timeout returns current status without canceling work.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_db_id": {"type": "string", "description": "Job session DB handle."},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 240, "description": "Observation deadline (default 30 seconds)."}
                },
                "required": ["session_db_id"],
                "additionalProperties": false
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            timeout: 300,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let id = session_id(&arguments)?;
            let seconds = arguments
                .get("timeout_seconds")
                .map(Value::as_u64)
                .unwrap_or(Some(30))
                .filter(|secs| (1..=240).contains(secs))
                .ok_or_else(|| "timeout_seconds must be 1..240".to_string())?;
            let server = self
                .server
                .get()
                .ok_or_else(|| "Server not initialized".to_string())?;
            let status = server
                .observe_job_wait(id, std::time::Duration::from_secs(seconds))
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_string(&status).map_err(|e| e.to_string().into())
        })
    }
}
