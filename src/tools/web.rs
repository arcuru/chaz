use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use crate::tool_host::Capability;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info};

/// Fetch a URL via HTTP. Network policy (endpoint allowlisting, private-IP
/// blocking) is enforced by the host at the capability boundary — the tool
/// itself does not inspect grants directly.
pub struct WebFetch;

impl Tool for WebFetch {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "web_fetch".to_string(),
            description:
                "Fetch a URL via HTTP and return the response body. Supports GET and POST methods."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "method": {
                        "type": "string",
                        "description": "HTTP method: GET (default) or POST"
                    },
                    "body": {
                        "type": "string",
                        "description": "Request body (for POST requests)"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Medium,
            approval: ApprovalRequirement::UnlessAutoApproved,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        use crate::tool::ToolError;
        Box::pin(async move {
            let url = arguments
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'url' argument".into()))?
                .to_string();

            let method = arguments
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_uppercase();

            let body = arguments
                .get("body")
                .and_then(|v| v.as_str())
                .map(String::from);

            info!(%method, %url, "Fetching URL via host");

            let result = ctx
                .host()
                .request(
                    &Capability::HttpRequest {
                        url,
                        method,
                        headers: HashMap::new(),
                        body,
                    },
                    ctx.grants(),
                )
                .await?;

            match result {
                crate::tool_host::CapabilityResult::HttpResponse(response) => {
                    let body = String::from_utf8_lossy(&response.body).into_owned();
                    debug!(
                        status = response.status,
                        body_len = body.len(),
                        "HTTP response received"
                    );

                    let mut formatted = format!("[{}]\n{body}", response.status);

                    // Truncate very long responses
                    if formatted.len() > 50000 {
                        formatted.truncate(50000);
                        formatted.push_str("\n[truncated]");
                    }

                    Ok(formatted)
                }
                _ => Err(ToolError::Execution(
                    "Unexpected host result for HTTP capability".into(),
                )),
            }
        })
    }
}
