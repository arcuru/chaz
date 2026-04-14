use crate::tool::{Tool, ToolContext};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Returns the current date and time in UTC
pub struct GetTime;

impl Tool for GetTime {
    fn name(&self) -> &str {
        "get_time"
    }

    fn description(&self) -> &str {
        "Get the current date and time in UTC"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn execute(
        &self,
        _arguments: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async { Ok(chrono::Utc::now().to_rfc3339()) })
    }
}
