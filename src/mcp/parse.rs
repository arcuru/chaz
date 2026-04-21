//! Response parsers for MCP transports: SSE event streams and plain
//! JSON-RPC. Both are pure string→`Value` transforms with no I/O, which
//! keeps them independently testable.

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info};

/// Parse an SSE body for a JSON-RPC response.
///
/// Handles both `data: {...}` (with space) and `data:{...}` (without space)
/// formats. Processes notifications inline, setting `tools_changed` flag
/// when `notifications/tools/list_changed` is encountered. Returns the
/// first JSON-RPC result found, or an error.
pub(super) fn parse_sse_body(
    server_name: &str,
    body: &str,
    tools_changed: &AtomicBool,
) -> Result<Value, String> {
    for line in body.lines() {
        // SSE spec: "data:" followed by optional space, then the value
        let data = if let Some(d) = line.strip_prefix("data: ") {
            d.trim()
        } else if let Some(d) = line.strip_prefix("data:") {
            d.trim()
        } else {
            continue;
        };

        if data.is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        debug!("MCP '{server_name}' ← (SSE) {data}");

        // Check if this is a notification (no "id" field)
        if parsed.get("id").is_none() {
            let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method == "notifications/tools/list_changed" {
                info!("MCP '{server_name}' signaled tools/list_changed");
                tools_changed.store(true, Ordering::Relaxed);
            }
            continue;
        }

        // This is a response — check for error first
        if let Some(err) = parsed.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(format!("MCP '{server_name}' error: {msg}"));
        }

        if let Some(result) = parsed.get("result").cloned() {
            return Ok(result);
        }
    }

    Err(format!(
        "MCP '{server_name}': no JSON-RPC response in SSE stream"
    ))
}

/// Parse a JSON-RPC response body, extracting the result or error.
pub(super) fn parse_jsonrpc_response(server_name: &str, body: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(body)
        .map_err(|e| format!("MCP '{server_name}' invalid JSON response: {e}"))?;

    if let Some(err) = parsed.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("MCP '{server_name}' error: {msg}"));
    }

    parsed
        .get("result")
        .cloned()
        .ok_or_else(|| format!("MCP '{server_name}': response missing result"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // parse_jsonrpc_response

    #[test]
    fn test_jsonrpc_response_success() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let result = parse_jsonrpc_response("test", body).unwrap();
        assert_eq!(result, json!({"tools": []}));
    }

    #[test]
    fn test_jsonrpc_response_error() {
        let body =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#;
        let err = parse_jsonrpc_response("test", body).unwrap_err();
        assert!(err.contains("Invalid Request"));
    }

    #[test]
    fn test_jsonrpc_response_error_missing_message() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600}}"#;
        let err = parse_jsonrpc_response("test", body).unwrap_err();
        assert!(err.contains("unknown error"));
    }

    #[test]
    fn test_jsonrpc_response_missing_result() {
        // Has id but neither result nor error — malformed
        let body = r#"{"jsonrpc":"2.0","id":1}"#;
        let err = parse_jsonrpc_response("test", body).unwrap_err();
        assert!(err.contains("response missing result"));
    }

    #[test]
    fn test_jsonrpc_response_invalid_json() {
        let err = parse_jsonrpc_response("test", "not json at all").unwrap_err();
        assert!(err.contains("invalid JSON"));
    }

    #[test]
    fn test_jsonrpc_response_null_result() {
        // result is explicitly null — valid JSON-RPC
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let result = parse_jsonrpc_response("test", body).unwrap();
        assert_eq!(result, Value::Null);
    }

    // parse_sse_body

    #[test]
    fn test_sse_basic_response() {
        let flag = AtomicBool::new(false);
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"value\":42}}\n\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!({"value": 42}));
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_sse_no_space_after_data_colon() {
        // Some SSE implementations omit the space
        let flag = AtomicBool::new(false);
        let body = "data:{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn test_sse_error_response() {
        let flag = AtomicBool::new(false);
        let body =
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-1,\"message\":\"boom\"}}\n\n";
        let err = parse_sse_body("test", body, &flag).unwrap_err();
        assert!(err.contains("boom"));
    }

    #[test]
    fn test_sse_notification_before_response() {
        let flag = AtomicBool::new(false);
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!({"tools": []}));
        // The notification should have set the flag
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_sse_only_notifications_no_response() {
        let flag = AtomicBool::new(false);
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        let err = parse_sse_body("test", body, &flag).unwrap_err();
        assert!(err.contains("no JSON-RPC response"));
    }

    #[test]
    fn test_sse_empty_body() {
        let flag = AtomicBool::new(false);
        let err = parse_sse_body("test", "", &flag).unwrap_err();
        assert!(err.contains("no JSON-RPC response"));
    }

    #[test]
    fn test_sse_non_data_lines_ignored() {
        let flag = AtomicBool::new(false);
        let body = "\
event: message\n\
id: 1\n\
retry: 5000\n\
: this is a comment\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"ok\"}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!("ok"));
    }

    #[test]
    fn test_sse_empty_data_line_skipped() {
        let flag = AtomicBool::new(false);
        let body = "\
data: \n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":true}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!(true));
    }

    #[test]
    fn test_sse_invalid_json_data_skipped() {
        let flag = AtomicBool::new(false);
        let body = "\
data: not valid json\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"found it\"}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!("found it"));
    }

    #[test]
    fn test_sse_response_with_id_null() {
        // id: null is present (not absent), so it shouldn't be treated as notification
        let flag = AtomicBool::new(false);
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":null,\"result\":\"null-id\"}\n\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!("null-id"));
    }

    #[test]
    fn test_sse_multiple_notifications_set_flag_once() {
        let flag = AtomicBool::new(false);
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"done\"}\n\
\n";
        let result = parse_sse_body("test", body, &flag).unwrap();
        assert_eq!(result, json!("done"));
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_tools_changed_flag_set_by_sse_notification() {
        let flag = AtomicBool::new(false);
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"ok\"}\n";
        let _ = parse_sse_body("test", body, &flag);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_tools_changed_flag_not_set_by_other_notifications() {
        let flag = AtomicBool::new(false);
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":50}}\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"ok\"}\n";
        let _ = parse_sse_body("test", body, &flag);
        assert!(!flag.load(Ordering::Relaxed));
    }
}
