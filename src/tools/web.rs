use crate::security::NetworkPolicy;
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Fetch a URL via HTTP. Network policy enforced before requests.
pub struct WebFetch {
    network_policy: Arc<NetworkPolicy>,
}

impl WebFetch {
    pub fn new(network_policy: Arc<NetworkPolicy>) -> Self {
        Self { network_policy }
    }
}

impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL via HTTP and return the response body. Supports GET and POST methods."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
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
        })
    }

    fn risk_level(&self, _params: &Value) -> RiskLevel {
        RiskLevel::Medium
    }

    fn requires_approval(&self, _params: &Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn execute(
        &self,
        arguments: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let url = arguments
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'url' argument".to_string())?;

            let method = arguments
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_uppercase();

            // Enforce network policy
            self.network_policy.check(url, &method)?;

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

            let request = match method.as_str() {
                "POST" => {
                    let mut req = client.post(url);
                    if let Some(body) = arguments.get("body").and_then(|v| v.as_str()) {
                        req = req.body(body.to_string());
                    }
                    req
                }
                _ => client.get(url),
            };

            let response = request
                .send()
                .await
                .map_err(|e| format!("HTTP request failed: {e}"))?;

            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|e| format!("Failed to read response body: {e}"))?;

            let mut result = format!("[{status}]\n{body}");

            // Truncate very long responses
            if result.len() > 50000 {
                result.truncate(50000);
                result.push_str("\n[truncated]");
            }

            Ok(result)
        })
    }
}
