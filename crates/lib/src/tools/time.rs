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
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute(
        &self,
        _arguments: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + '_>> {
        Box::pin(async { Ok(chrono::Utc::now().to_rfc3339()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fresh_session, tool_context};
    use crate::tool::ToolRegistry;
    use std::sync::Arc;

    #[test]
    fn descriptor_advertises_get_time_with_no_required_params() {
        let d = GetTime.descriptor();
        assert_eq!(d.name, "get_time");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.is_empty());
    }

    #[tokio::test]
    async fn execute_returns_rfc3339_utc_timestamp() {
        let (_instance, session) = fresh_session().await;
        let ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        let out = GetTime
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect("get_time should succeed");
        // RFC3339 in UTC: yyyy-mm-ddTHH:MM:SS<...>+00:00
        assert!(
            out.contains('T') && (out.ends_with("+00:00") || out.ends_with('Z')),
            "expected RFC3339 UTC timestamp, got: {out}"
        );
        chrono::DateTime::parse_from_rfc3339(&out)
            .expect("output should round-trip through RFC3339 parsing");
    }
}
