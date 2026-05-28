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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockHost, fresh_session, tool_context_with_host};
    use crate::tool::ToolRegistry;
    use crate::tool_host::Capability;
    use std::sync::Arc;

    async fn ctx_with(host: Arc<MockHost>) -> (eidetica::Instance, ToolContext) {
        let (instance, session) = fresh_session().await;
        let ctx = tool_context_with_host(session, Arc::new(ToolRegistry::new()), host);
        (instance, ctx)
    }

    #[test]
    fn descriptor_lists_url_required() {
        let d = WebFetch.descriptor();
        assert_eq!(d.name, "web_fetch");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "url"));
    }

    #[test]
    fn default_policy_is_medium_unless_auto_approved() {
        let p = WebFetch.default_policy();
        assert!(matches!(p.risk, RiskLevel::Medium));
        assert!(matches!(p.approval, ApprovalRequirement::UnlessAutoApproved));
    }

    #[tokio::test]
    async fn missing_url_argument_errors() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host.clone()).await;
        let err = WebFetch
            .execute(serde_json::json!({}), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("url"));
        assert!(host.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn default_method_is_get_uppercased() {
        let host = Arc::new(MockHost::new());
        host.push_http(200, b"".to_vec());
        let (_i, c) = ctx_with(host.clone()).await;
        WebFetch
            .execute(serde_json::json!({ "url": "https://example" }), &c)
            .await
            .unwrap();
        match host.last_call().unwrap() {
            Capability::HttpRequest { method, body, .. } => {
                assert_eq!(method, "GET");
                assert!(body.is_none());
            }
            other => panic!("unexpected capability: {other:?}"),
        }
    }

    #[tokio::test]
    async fn lowercase_method_is_uppercased_and_body_forwarded() {
        let host = Arc::new(MockHost::new());
        host.push_http(204, b"".to_vec());
        let (_i, c) = ctx_with(host.clone()).await;
        WebFetch
            .execute(
                serde_json::json!({ "url": "https://x", "method": "post", "body": "{}" }),
                &c,
            )
            .await
            .unwrap();
        match host.last_call().unwrap() {
            Capability::HttpRequest { method, body, url, .. } => {
                assert_eq!(method, "POST");
                assert_eq!(url, "https://x");
                assert_eq!(body.as_deref(), Some("{}"));
            }
            other => panic!("unexpected capability: {other:?}"),
        }
    }

    #[tokio::test]
    async fn response_includes_status_prefix_and_body() {
        let host = Arc::new(MockHost::new());
        host.push_http(418, b"i am a teapot".to_vec());
        let (_i, c) = ctx_with(host).await;
        let out = WebFetch
            .execute(serde_json::json!({ "url": "https://t" }), &c)
            .await
            .unwrap();
        assert!(out.starts_with("[418]"), "got: {out}");
        assert!(out.contains("i am a teapot"));
    }

    #[tokio::test]
    async fn very_long_body_is_truncated() {
        let host = Arc::new(MockHost::new());
        host.push_http(200, vec![b'a'; 60_000]);
        let (_i, c) = ctx_with(host).await;
        let out = WebFetch
            .execute(serde_json::json!({ "url": "https://big" }), &c)
            .await
            .unwrap();
        assert!(out.ends_with("[truncated]"), "got tail: {}", &out[out.len() - 20..]);
    }

    #[tokio::test]
    async fn host_error_propagates() {
        let host = Arc::new(MockHost::new());
        host.push_err(crate::tool::ToolError::Network("connection refused".into()));
        let (_i, c) = ctx_with(host).await;
        let err = WebFetch
            .execute(serde_json::json!({ "url": "https://nope" }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("connection refused"));
    }
}
