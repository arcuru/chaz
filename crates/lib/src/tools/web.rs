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
                "Fetch a URL via HTTP and return the response body. Supports GET and POST methods. \
                 Redirects are not followed: a 3xx response reports its Location URL, which you \
                 may fetch with a new call."
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
                        url: url.clone(),
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

                    let mut formatted = format!("[{}]\n", response.status);
                    if let Some(location) = redirect_location(&url, &response) {
                        formatted.push_str(&format!(
                            "Location: {location}\n(redirect not followed; fetch the Location URL to continue)\n"
                        ));
                    }
                    formatted.push_str(&body);

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

/// The `Location` target of a 3xx response, resolved against the request URL
/// when relative. The host never follows redirects, so the model sees where the
/// server pointed and may issue a new, separately authorized request.
fn redirect_location(url: &str, response: &crate::tool_host::HttpResponse) -> Option<String> {
    if !(300..400).contains(&response.status) {
        return None;
    }
    let raw = response
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())?;
    Some(
        url::Url::parse(url)
            .and_then(|base| base.join(raw))
            .map(|u| u.to_string())
            .unwrap_or_else(|_| raw.to_string()),
    )
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
        assert!(matches!(
            p.approval,
            ApprovalRequirement::UnlessAutoApproved
        ));
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
            Capability::HttpRequest {
                method, body, url, ..
            } => {
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
    async fn redirect_surfaces_resolved_location_without_following() {
        let host = Arc::new(MockHost::new());
        host.push_http_with_headers(
            302,
            [("location".to_string(), "/secret".to_string())].into(),
            b"moved".to_vec(),
        );
        let (_i, c) = ctx_with(host.clone()).await;
        let out = WebFetch
            .execute(serde_json::json!({ "url": "http://h.test/ok/x" }), &c)
            .await
            .unwrap();
        assert!(
            out.starts_with("[302]\nLocation: http://h.test/secret\n"),
            "got: {out}"
        );
        assert!(out.contains("redirect not followed"));
        assert!(out.ends_with("moved"));
        // Exactly one host request: the tool never issues the redirected one.
        assert_eq!(host.recorded_calls().len(), 1);
    }

    #[tokio::test]
    async fn location_on_non_redirect_is_not_announced() {
        let host = Arc::new(MockHost::new());
        host.push_http_with_headers(
            201,
            [("location".to_string(), "/new".to_string())].into(),
            b"created".to_vec(),
        );
        let (_i, c) = ctx_with(host).await;
        let out = WebFetch
            .execute(serde_json::json!({ "url": "http://h.test/" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "[201]\ncreated");
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
        assert!(
            out.ends_with("[truncated]"),
            "got tail: {}",
            &out[out.len() - 20..]
        );
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
        assert!(
            format!("{err}")
                .to_lowercase()
                .contains("connection refused")
        );
    }
}
