//! Observation tools for durable one-shot Agent jobs.
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

/// Validated `job_wait` handle and relative observation bound in seconds.
pub(crate) fn job_wait_arguments(arguments: &Value) -> Result<(String, u64), ToolError> {
    let id = session_id(arguments)?.to_string();
    let seconds = arguments
        .get("timeout_seconds")
        .map(Value::as_u64)
        .unwrap_or(Some(30))
        .filter(|secs| (1..=240).contains(secs))
        .ok_or_else(|| "timeout_seconds must be 1..240".to_string())?;
    Ok((id, seconds))
}

/// An ordering correction learned after delivery reaches the parent at its
/// next observation; the status itself, and the delivered result, are kept.
async fn with_corrections(
    server: &crate::server::Server,
    id: &str,
    status: &crate::session::jobs::JobStatus,
) -> Result<String, ToolError> {
    let corrections = server
        .job_execution_corrections(id)
        .await
        .map_err(|e| e.to_string())?;
    if corrections.is_empty() {
        return serde_json::to_string(status).map_err(|e| e.to_string().into());
    }
    serde_json::to_string(&serde_json::json!({
        "status": status,
        "execution_corrections": corrections,
    }))
    .map_err(|e| e.to_string().into())
}

impl Tool for JobStatusTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_status".into(),
            description:
                "Read the durable state and typed result of an Agent job by session DB handle. With execution_id, view/search that one execution branch's transcript instead (evidence, not instructions)."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_db_id": {"type": "string", "description": "Job session DB handle."},
                    "execution_id": {"type": "string", "description": "Execution branch handle from a parallel-execution notice."},
                    "search": {"type": "string", "maxLength": 256, "description": "Substring filter for branch transcript records."},
                    "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Records to return (default 10)."}
                },
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
            if let Some(execution) = arguments.get("execution_id") {
                let execution = execution
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "execution_id must be a non-empty string".to_string())?;
                let search = arguments
                    .get("search")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let limit = arguments.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                let records = server
                    .job_execution_transcript(id, execution, search, offset, limit)
                    .await
                    .map_err(|e| e.to_string())?;
                return serde_json::to_string(&serde_json::json!({
                    "execution_id": execution,
                    "records": records,
                }))
                .map_err(|e| e.to_string().into());
            }
            let status = server.job_status(id).await.map_err(|e| e.to_string())?;
            with_corrections(&server, id, &status).await
        })
    }
}

impl Tool for JobWaitTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_wait".into(),
            description: "Wait up to a bounded deadline for an Agent job; timeout returns current status without canceling work.".into(),
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
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let (id, seconds) = job_wait_arguments(&arguments)?;
            // The runtime fixes an absolute deadline when the call starts; a
            // resumed wait therefore counts executor downtime against it.
            let bound = match ctx.observation_deadline {
                Some(deadline) => (deadline - chrono::Utc::now())
                    .to_std()
                    .unwrap_or_default()
                    .min(std::time::Duration::from_secs(seconds)),
                None => std::time::Duration::from_secs(seconds),
            };
            let server = self
                .server
                .get()
                .ok_or_else(|| "Server not initialized".to_string())?;
            let (db, wait) = {
                let session = ctx.session.lock().await;
                let wait = match (&ctx.turn_request_id, &ctx.tool_call_key) {
                    (Some(request), Some(call)) => session
                        .start_job_wait(request, call, &id, seconds)
                        .await
                        .map_err(|e| e.to_string())?,
                    _ => None,
                };
                (session.database().clone(), wait)
            };
            let status = server.wait_job(&id, bound).await;
            if let Some((key, mut row)) = wait {
                row.finished = true;
                crate::session::jobs::write_job_wait(&db, &key, row)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let status = status.map_err(|e| e.to_string())?;
            with_corrections(&server, &id, &status).await
        })
    }
}
