use crate::tool::{Tool, ToolContext, ToolDescriptor};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Returns the current date and time in UTC
pub struct GetTime;

impl Tool for GetTime {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "get_time".to_string(),
            description: "Get the current date and time in UTC".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn execute(
        &self,
        _arguments: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + '_>> {
        Box::pin(async { Ok(chrono::Utc::now().to_rfc3339()) })
    }
}
