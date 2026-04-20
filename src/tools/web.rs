use crate::grants::NetworkGrant;
use crate::security::network::EndpointPattern as PolicyEndpoint;
use crate::security::NetworkPolicy;
use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info};

/// Fetch a URL via HTTP. Network policy is built per call from the resolved
/// `NetworkGrant` in `ToolContext::grants()`. Private-IP blocking is always on
/// unless the grant opts in via `allow_private: true`.
pub struct WebFetch;

impl WebFetch {
    fn build_policy(grant: Option<&NetworkGrant>) -> NetworkPolicy {
        let (endpoints, allow_private) = match grant {
            Some(g) => (
                g.endpoints
                    .iter()
                    .map(|e| PolicyEndpoint {
                        host: e.host.clone(),
                        path_prefix: e.path_prefix.clone(),
                        methods: e.methods.clone(),
                    })
                    .collect(),
                g.allow_private,
            ),
            None => (Vec::new(), false),
        };
        NetworkPolicy::new(endpoints, !allow_private)
    }
}

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
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
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

            // Enforce network policy built from the resolved NetworkGrant
            let policy = Self::build_policy(ctx.grants().network.as_ref());
            policy.check(url, &method)?;
            info!(%method, %url, "Fetching URL");

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
            debug!(%status, body_len = body.len(), %url, "HTTP response received");

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::EndpointPattern;

    #[test]
    fn test_no_grant_blocks_private_ips_allows_public() {
        let p = WebFetch::build_policy(None);
        assert!(p.check("https://example.com/", "GET").is_ok());
        assert!(p.check("http://127.0.0.1/", "GET").is_err());
    }

    #[test]
    fn test_grant_endpoint_allowlist() {
        let grant = NetworkGrant {
            endpoints: vec![EndpointPattern {
                host: "api.example.com".into(),
                path_prefix: None,
                methods: Some(vec!["GET".into()]),
            }],
            allow_private: false,
        };
        let p = WebFetch::build_policy(Some(&grant));
        assert!(p.check("https://api.example.com/foo", "GET").is_ok());
        assert!(p.check("https://api.example.com/foo", "POST").is_err());
        assert!(p.check("https://evil.com/", "GET").is_err());
    }

    #[test]
    fn test_allow_private_opens_internal_hosts() {
        let grant = NetworkGrant {
            endpoints: vec![],
            allow_private: true,
        };
        let p = WebFetch::build_policy(Some(&grant));
        assert!(p.check("http://127.0.0.1/", "GET").is_ok());
    }
}
