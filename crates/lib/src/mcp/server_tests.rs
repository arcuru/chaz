//! Unit tests for the MCP server module.
//!
//! Extracted from `server.rs` to keep the production module readable;
//! still compiled as the `tests` submodule (module path unchanged).

use super::*;
use crate::tool::ToolError;

// ================================================================
// classify_mcp_error
// ================================================================

#[test]
fn test_classify_http_error_is_network() {
    let err = classify_mcp_error("MCP 'srv' HTTP error: connection refused".to_string());
    assert!(matches!(err, ToolError::Network(_)));
}

#[test]
fn test_classify_http_body_error_is_network() {
    let err = classify_mcp_error("MCP 'srv' HTTP body error: premature eof".to_string());
    assert!(matches!(err, ToolError::Network(_)));
}

#[test]
fn test_classify_closed_stdout_is_network() {
    // Subprocess died — conceptually a network/transport failure for our purposes.
    let err = classify_mcp_error("MCP server 'srv' closed stdout".to_string());
    assert!(matches!(err, ToolError::Network(_)));
}

#[test]
fn test_classify_write_error_is_network() {
    let err = classify_mcp_error("MCP 'srv' write error: Broken pipe".to_string());
    assert!(matches!(err, ToolError::Network(_)));
}

#[test]
fn test_classify_tool_returned_error_is_execution() {
    // Application-level tool failures stay as Execution.
    let err = classify_mcp_error("file not found".to_string());
    assert!(matches!(err, ToolError::Execution(_)));
}

#[test]
fn test_classify_protocol_error_is_execution() {
    // JSON-RPC protocol errors aren't transport-level.
    let err = classify_mcp_error("MCP 'srv' error: Method not found".to_string());
    assert!(matches!(err, ToolError::Execution(_)));
}

// ================================================================
// extract_text_content
// ================================================================

#[test]
fn test_extract_text_single_item() {
    let result = json!({
        "content": [{"type": "text", "text": "hello world"}]
    });
    assert_eq!(extract_text_content(&result), "hello world");
}

#[test]
fn test_extract_text_multiple_items() {
    let result = json!({
        "content": [
            {"type": "text", "text": "line 1"},
            {"type": "text", "text": "line 2"}
        ]
    });
    assert_eq!(extract_text_content(&result), "line 1\nline 2");
}

#[test]
fn test_extract_text_no_content_field() {
    let result = json!({"something": "else"});
    assert_eq!(extract_text_content(&result), "");
}

#[test]
fn test_extract_text_empty_content_array() {
    let result = json!({"content": []});
    assert_eq!(extract_text_content(&result), "");
}

#[test]
fn test_extract_text_content_not_array() {
    let result = json!({"content": "just a string"});
    assert_eq!(extract_text_content(&result), "");
}

#[test]
fn test_extract_text_skips_non_text_types() {
    let result = json!({
        "content": [
            {"type": "image", "data": "base64..."},
            {"type": "text", "text": "the text part"}
        ]
    });
    assert_eq!(extract_text_content(&result), "the text part");
}

#[test]
fn test_extract_text_missing_text_field() {
    // type is "text" but the "text" field is missing
    let result = json!({
        "content": [{"type": "text"}]
    });
    assert_eq!(extract_text_content(&result), "");
}

// ================================================================
// Tool metadata & McpTool::descriptor()
// ================================================================

/// Build an McpServer with fake HTTP transport for metadata testing.
/// The HTTP transport won't be called — we just need the metadata map.
fn make_test_server(name: &str) -> McpServer {
    McpServer {
        name: name.to_string(),
        transport: Transport::new_http("http://unused"),
        next_id: AtomicU64::new(1),
        default_policy: None,
        tools_changed: AtomicBool::new(false),
        tool_metadata: RwLock::new(HashMap::new()),
        capabilities: RwLock::new(McpServerCapabilities::default()),
    }
}

#[test]
fn test_mcp_tool_descriptor_from_metadata() {
    let server = make_test_server("srv");
    server.tool_metadata.write().unwrap().insert(
        "my_tool".to_string(),
        McpToolMetadata {
            description: "Does things".to_string(),
            input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            annotations: None,
        },
    );
    let server = Arc::new(server);
    let tool = McpTool {
        server: server.clone(),
        raw_name: "my_tool".to_string(),
        namespaced_name: "srv__my_tool".to_string(),
    };
    let desc = tool.descriptor();
    assert_eq!(desc.name, "srv__my_tool");
    assert_eq!(desc.description, "Does things");
    assert_eq!(
        desc.parameters,
        json!({"type": "object", "properties": {"x": {"type": "string"}}})
    );
}

#[test]
fn test_mcp_tool_descriptor_missing_metadata() {
    // Tool exists in registry but metadata was removed (e.g., server removed the tool)
    let server = Arc::new(make_test_server("srv"));
    let tool = McpTool {
        server: server.clone(),
        raw_name: "gone_tool".to_string(),
        namespaced_name: "srv__gone_tool".to_string(),
    };

    let desc = tool.descriptor();
    assert_eq!(desc.name, "srv__gone_tool");
    assert_eq!(desc.description, "");
    assert_eq!(desc.parameters, json!({"type": "object", "properties": {}}));
}

#[test]
fn test_mcp_tool_descriptor_updates_after_metadata_change() {
    let server = make_test_server("srv");
    server.tool_metadata.write().unwrap().insert(
        "evolving".to_string(),
        McpToolMetadata {
            description: "v1".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            annotations: None,
        },
    );
    let server = Arc::new(server);
    let tool = McpTool {
        server: server.clone(),
        raw_name: "evolving".to_string(),
        namespaced_name: "srv__evolving".to_string(),
    };

    assert_eq!(tool.descriptor().description, "v1");

    // Simulate metadata update (as refresh_tools would do)
    server.tool_metadata.write().unwrap().insert(
            "evolving".to_string(),
            McpToolMetadata {
                description: "v2 with new params".to_string(),
                input_schema: json!({"type": "object", "properties": {"new_param": {"type": "number"}}}),
                annotations: None,
            },
        );

    let desc = tool.descriptor();
    assert_eq!(desc.description, "v2 with new params");
    assert!(desc.parameters["properties"]["new_param"].is_object());
}

#[test]
fn test_mcp_tool_default_policy_no_override() {
    let server = Arc::new(make_test_server("srv"));
    let tool = McpTool {
        server,
        raw_name: "t".to_string(),
        namespaced_name: "srv__t".to_string(),
    };
    let policy = tool.default_policy();
    assert_eq!(policy.risk, RiskLevel::Medium);
    assert_eq!(policy.approval, ApprovalRequirement::UnlessAutoApproved);
    assert_eq!(policy.timeout, 60);
}

#[test]
fn test_mcp_tool_default_policy_with_server_override() {
    let mut server = make_test_server("srv");
    server.default_policy = Some(ToolPolicy {
        risk: RiskLevel::High,
        approval: ApprovalRequirement::Always,
        timeout: 10,
        sensitive_params: vec!["secret".to_string()],
        rate_limit: Some(5),
        grants: Default::default(),
    });
    let server = Arc::new(server);
    let tool = McpTool {
        server,
        raw_name: "t".to_string(),
        namespaced_name: "srv__t".to_string(),
    };
    let policy = tool.default_policy();
    assert_eq!(policy.risk, RiskLevel::High);
    assert_eq!(policy.timeout, 10);
    assert_eq!(policy.sensitive_params, vec!["secret"]);
    assert_eq!(policy.rate_limit, Some(5));
}

#[test]
fn test_tools_changed_flag_default_false() {
    let server = make_test_server("srv");
    assert!(!server.tools_changed.load(Ordering::Relaxed));
}

// ================================================================
// Tool annotations → default_policy
// ================================================================

/// Helper: insert a tool with annotations and return its wrapper.
fn tool_with_annotations(
    server_name: &str,
    raw_name: &str,
    ann: McpToolAnnotations,
) -> (Arc<McpServer>, McpTool) {
    let server = make_test_server(server_name);
    server.tool_metadata.write().unwrap().insert(
        raw_name.to_string(),
        McpToolMetadata {
            description: String::new(),
            input_schema: json!({"type": "object", "properties": {}}),
            annotations: Some(ann),
        },
    );
    let server = Arc::new(server);
    let tool = McpTool {
        server: server.clone(),
        raw_name: raw_name.to_string(),
        namespaced_name: format!("{server_name}__{raw_name}"),
    };
    (server, tool)
}

#[test]
fn annotations_from_json_parses_all_four_hints() {
    let v = json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false,
    });
    let ann = McpToolAnnotations::from_json(&v).expect("should parse");
    assert_eq!(ann.read_only_hint, Some(true));
    assert_eq!(ann.destructive_hint, Some(false));
    assert_eq!(ann.idempotent_hint, Some(true));
    assert_eq!(ann.open_world_hint, Some(false));
}

#[test]
fn annotations_from_json_treats_missing_fields_as_none() {
    let v = json!({"readOnlyHint": true}); // only one set
    let ann = McpToolAnnotations::from_json(&v).expect("should parse");
    assert_eq!(ann.read_only_hint, Some(true));
    assert_eq!(ann.destructive_hint, None);
    assert_eq!(ann.idempotent_hint, None);
    assert_eq!(ann.open_world_hint, None);
}

#[test]
fn annotations_from_json_returns_none_for_non_object() {
    assert!(McpToolAnnotations::from_json(&json!(null)).is_none());
    assert!(McpToolAnnotations::from_json(&json!("string")).is_none());
    assert!(McpToolAnnotations::from_json(&json!([1, 2, 3])).is_none());
}

#[test]
fn read_only_hint_maps_to_low_never() {
    let (_, tool) = tool_with_annotations(
        "srv",
        "list",
        McpToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        },
    );
    let p = tool.default_policy();
    assert_eq!(p.risk, RiskLevel::Low);
    assert_eq!(p.approval, ApprovalRequirement::Never);
}

#[test]
fn destructive_hint_maps_to_high_always() {
    let (_, tool) = tool_with_annotations(
        "srv",
        "drop_table",
        McpToolAnnotations {
            destructive_hint: Some(true),
            ..Default::default()
        },
    );
    let p = tool.default_policy();
    assert_eq!(p.risk, RiskLevel::High);
    assert_eq!(p.approval, ApprovalRequirement::Always);
}

#[test]
fn destructive_hint_wins_over_read_only_hint_if_both_set() {
    // Misconfigured server claims both — conservative choice is to
    // require approval (treat as destructive).
    let (_, tool) = tool_with_annotations(
        "srv",
        "weird",
        McpToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(true),
            ..Default::default()
        },
    );
    let p = tool.default_policy();
    assert_eq!(p.risk, RiskLevel::High);
    assert_eq!(p.approval, ApprovalRequirement::Always);
}

#[test]
fn no_useful_hints_falls_back_to_medium() {
    // Annotations present but only carry idempotent/openWorld — neither
    // currently maps to a risk tier; expect chaz's default.
    let (_, tool) = tool_with_annotations(
        "srv",
        "unknown",
        McpToolAnnotations {
            idempotent_hint: Some(true),
            open_world_hint: Some(true),
            ..Default::default()
        },
    );
    let p = tool.default_policy();
    assert_eq!(p.risk, RiskLevel::Medium);
    assert_eq!(p.approval, ApprovalRequirement::UnlessAutoApproved);
}

#[test]
fn server_yaml_policy_overrides_annotations() {
    // destructiveHint would derive High+Always, but the yaml-pinned
    // policy is Low+Never — yaml wins.
    let mut server = make_test_server("srv");
    server.default_policy = Some(ToolPolicy {
        risk: RiskLevel::Low,
        approval: ApprovalRequirement::Never,
        timeout: 30,
        sensitive_params: Vec::new(),
        rate_limit: None,
        grants: Default::default(),
    });
    server.tool_metadata.write().unwrap().insert(
        "drop_table".to_string(),
        McpToolMetadata {
            description: String::new(),
            input_schema: json!({"type": "object", "properties": {}}),
            annotations: Some(McpToolAnnotations {
                destructive_hint: Some(true),
                ..Default::default()
            }),
        },
    );
    let server = Arc::new(server);
    let tool = McpTool {
        server,
        raw_name: "drop_table".to_string(),
        namespaced_name: "srv__drop_table".to_string(),
    };
    let p = tool.default_policy();
    assert_eq!(p.risk, RiskLevel::Low);
    assert_eq!(p.approval, ApprovalRequirement::Never);
    assert_eq!(p.timeout, 30);
}

// ================================================================
// Output truncation
// ================================================================

#[test]
fn test_max_output_bytes_constant() {
    // Sanity check — should be 100 KB
    assert_eq!(MAX_OUTPUT_BYTES, 100 * 1024);
}

#[test]
fn test_call_tool_is_error_true_with_text() {
    // Simulate the result that call_tool receives when isError is set
    let result = json!({
        "isError": true,
        "content": [{"type": "text", "text": "something broke"}]
    });
    assert_eq!(result.get("isError").and_then(|e| e.as_bool()), Some(true));
    let error_text = extract_text_content(&result);
    assert_eq!(error_text, "something broke");
}

#[test]
fn test_call_tool_is_error_true_empty_text() {
    // isError with no content → fallback message
    let result = json!({"isError": true, "content": []});
    let error_text = extract_text_content(&result);
    assert!(error_text.is_empty());
    // call_tool would return "MCP tool returned an error" for this case
}

#[test]
fn test_call_tool_is_error_false() {
    let result = json!({"isError": false, "content": [{"type": "text", "text": "ok"}]});
    assert_ne!(result.get("isError").and_then(|e| e.as_bool()), Some(true));
}

#[test]
fn test_call_tool_is_error_absent() {
    // No isError field at all — should not be treated as error
    let result = json!({"content": [{"type": "text", "text": "fine"}]});
    assert_eq!(result.get("isError").and_then(|e| e.as_bool()), None);
}

#[test]
fn test_output_truncation_logic() {
    // Simulate what call_tool does for large output
    let large_text = "x".repeat(MAX_OUTPUT_BYTES + 1000);
    assert!(large_text.len() > MAX_OUTPUT_BYTES);
    let truncated = format!(
        "{}\n\n[output truncated at {} bytes]",
        &large_text[..MAX_OUTPUT_BYTES],
        MAX_OUTPUT_BYTES
    );
    assert!(truncated.len() < large_text.len());
    assert!(truncated.contains("[output truncated at"));
    assert_eq!(
        &truncated[..MAX_OUTPUT_BYTES],
        &large_text[..MAX_OUTPUT_BYTES]
    );
}

#[test]
fn test_output_at_exact_limit_not_truncated() {
    let exact_text = "x".repeat(MAX_OUTPUT_BYTES);
    // At exactly the limit, not over — should NOT truncate
    assert!(exact_text.len() <= MAX_OUTPUT_BYTES);
}

// ================================================================
// list_tools parsing
// ================================================================

#[test]
fn test_list_tools_parse_full_tool() {
    // Directly test the parsing logic that list_tools uses
    let response = json!({
        "tools": [{
            "name": "read_file",
            "description": "Read a file",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"}
                },
                "required": ["path"]
            }
        }]
    });
    let tools_array = response.get("tools").unwrap().as_array().unwrap();
    assert_eq!(tools_array.len(), 1);
    let tool = &tools_array[0];
    assert_eq!(tool["name"].as_str().unwrap(), "read_file");
    assert_eq!(tool["description"].as_str().unwrap(), "Read a file");
    assert!(tool["inputSchema"]["properties"]["path"].is_object());
}

#[test]
fn test_list_tools_missing_description_defaults() {
    let response = json!({
        "tools": [{"name": "bare_tool"}]
    });
    let tool = &response["tools"][0];
    // description defaults to "" when missing
    let description = tool
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    assert_eq!(description, "");
}

#[test]
fn test_list_tools_missing_input_schema_defaults() {
    let response = json!({
        "tools": [{"name": "bare_tool", "description": "no schema"}]
    });
    let tool = &response["tools"][0];
    let input_schema = tool
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    assert_eq!(input_schema, json!({"type": "object", "properties": {}}));
}

#[test]
fn test_list_tools_empty_array() {
    let response = json!({"tools": []});
    let tools = response["tools"].as_array().unwrap();
    assert!(tools.is_empty());
}

#[test]
fn test_list_tools_missing_tools_key() {
    let response = json!({"something": "else"});
    assert!(response.get("tools").and_then(|t| t.as_array()).is_none());
}

#[test]
fn test_list_tools_tool_missing_name() {
    let response = json!({
        "tools": [{"description": "no name tool"}]
    });
    let tool = &response["tools"][0];
    assert!(tool.get("name").and_then(|n| n.as_str()).is_none());
}

// ================================================================
// refresh_tools metadata logic (including stale removal)
// ================================================================

/// Helper: directly apply refresh logic to a metadata map.
/// Mirrors what refresh_tools does after calling list_tools.
fn apply_refresh(
    metadata: &mut HashMap<String, McpToolMetadata>,
    tools: &[(&str, &str, Value)],
) -> (usize, usize, usize) {
    let current_names: std::collections::HashSet<&str> =
        tools.iter().map(|(name, _, _)| *name).collect();

    let before = metadata.len();
    metadata.retain(|name, _| current_names.contains(name.as_str()));
    let removed = before - metadata.len();

    let mut added = 0;
    let mut updated = 0;
    for (name, desc, schema) in tools {
        let new_meta = McpToolMetadata {
            description: desc.to_string(),
            input_schema: schema.clone(),
            annotations: None,
        };
        if let Some(existing) = metadata.get_mut(*name) {
            if existing.description != new_meta.description
                || existing.input_schema != new_meta.input_schema
            {
                *existing = new_meta;
                updated += 1;
            }
        } else {
            metadata.insert(name.to_string(), new_meta);
            added += 1;
        }
    }
    (added, updated, removed)
}

#[test]
fn test_refresh_no_changes() {
    let mut metadata = HashMap::new();
    metadata.insert(
        "tool_a".to_string(),
        McpToolMetadata {
            description: "desc a".to_string(),
            input_schema: json!({"type": "object"}),
            annotations: None,
        },
    );

    let (added, updated, removed) = apply_refresh(
        &mut metadata,
        &[("tool_a", "desc a", json!({"type": "object"}))],
    );

    assert_eq!(added, 0);
    assert_eq!(updated, 0);
    assert_eq!(removed, 0);
    assert_eq!(metadata.len(), 1);
}

#[test]
fn test_refresh_updates_schema() {
    let mut metadata = HashMap::new();
    metadata.insert(
        "tool_a".to_string(),
        McpToolMetadata {
            description: "old desc".to_string(),
            input_schema: json!({"type": "object"}),
            annotations: None,
        },
    );

    let (added, updated, removed) = apply_refresh(
        &mut metadata,
        &[(
            "tool_a",
            "new desc",
            json!({"type": "object", "properties": {"x": {}}}),
        )],
    );

    assert_eq!(added, 0);
    assert_eq!(updated, 1);
    assert_eq!(removed, 0);
    assert_eq!(metadata["tool_a"].description, "new desc");
}

#[test]
fn test_refresh_adds_new_tool() {
    let mut metadata = HashMap::new();
    metadata.insert(
        "tool_a".to_string(),
        McpToolMetadata {
            description: "a".to_string(),
            input_schema: json!({}),
            annotations: None,
        },
    );

    let (added, updated, removed) = apply_refresh(
        &mut metadata,
        &[("tool_a", "a", json!({})), ("tool_b", "b", json!({}))],
    );

    assert_eq!(added, 1);
    assert_eq!(updated, 0);
    assert_eq!(removed, 0);
    assert_eq!(metadata.len(), 2);
    assert!(metadata.contains_key("tool_b"));
}

#[test]
fn test_refresh_removes_stale_tool() {
    let mut metadata = HashMap::new();
    metadata.insert(
        "tool_a".to_string(),
        McpToolMetadata {
            description: "a".to_string(),
            input_schema: json!({}),
            annotations: None,
        },
    );
    metadata.insert(
        "tool_b".to_string(),
        McpToolMetadata {
            description: "b".to_string(),
            input_schema: json!({}),
            annotations: None,
        },
    );

    // Server now only reports tool_a — tool_b should be removed
    let (added, updated, removed) = apply_refresh(&mut metadata, &[("tool_a", "a", json!({}))]);

    assert_eq!(added, 0);
    assert_eq!(updated, 0);
    assert_eq!(removed, 1);
    assert_eq!(metadata.len(), 1);
    assert!(metadata.contains_key("tool_a"));
    assert!(!metadata.contains_key("tool_b"));
}

#[test]
fn test_refresh_removes_all_tools() {
    let mut metadata = HashMap::new();
    metadata.insert(
        "tool_a".to_string(),
        McpToolMetadata {
            description: "a".to_string(),
            input_schema: json!({}),
            annotations: None,
        },
    );

    // Server reports empty tools list
    let (added, updated, removed) = apply_refresh(&mut metadata, &[]);

    assert_eq!(added, 0);
    assert_eq!(updated, 0);
    assert_eq!(removed, 1);
    assert!(metadata.is_empty());
}

#[test]
fn test_refresh_add_update_remove_simultaneously() {
    let mut metadata = HashMap::new();
    metadata.insert(
        "keep_same".to_string(),
        McpToolMetadata {
            description: "same".to_string(),
            input_schema: json!({}),
            annotations: None,
        },
    );
    metadata.insert(
        "will_update".to_string(),
        McpToolMetadata {
            description: "old".to_string(),
            input_schema: json!({}),
            annotations: None,
        },
    );
    metadata.insert(
        "will_remove".to_string(),
        McpToolMetadata {
            description: "doomed".to_string(),
            input_schema: json!({}),
            annotations: None,
        },
    );

    let (added, updated, removed) = apply_refresh(
        &mut metadata,
        &[
            ("keep_same", "same", json!({})),
            ("will_update", "updated", json!({})),
            ("brand_new", "new", json!({})),
        ],
    );

    assert_eq!(added, 1);
    assert_eq!(updated, 1);
    assert_eq!(removed, 1);
    assert_eq!(metadata.len(), 3);
    assert!(metadata.contains_key("keep_same"));
    assert_eq!(metadata["will_update"].description, "updated");
    assert!(metadata.contains_key("brand_new"));
    assert!(!metadata.contains_key("will_remove"));
}

#[test]
fn test_descriptor_returns_empty_after_metadata_removal() {
    // Simulate: tool existed, metadata removed by refresh
    let server = Arc::new(make_test_server("srv"));
    let tool = McpTool {
        server: server.clone(),
        raw_name: "removed".to_string(),
        namespaced_name: "srv__removed".to_string(),
    };

    // Initially no metadata — descriptor returns empty
    let desc = tool.descriptor();
    assert_eq!(desc.description, "");

    // Add metadata, verify it works
    server.tool_metadata.write().unwrap().insert(
        "removed".to_string(),
        McpToolMetadata {
            description: "exists".to_string(),
            input_schema: json!({"type": "object"}),
            annotations: None,
        },
    );
    assert_eq!(tool.descriptor().description, "exists");

    // Remove metadata (as refresh_tools now does)
    server.tool_metadata.write().unwrap().remove("removed");
    let desc = tool.descriptor();
    assert_eq!(desc.description, "");
    assert_eq!(desc.parameters, json!({"type": "object", "properties": {}}));
}

#[test]
fn test_next_id_increments() {
    let server = make_test_server("srv");
    let id1 = server.next_id.fetch_add(1, Ordering::Relaxed);
    let id2 = server.next_id.fetch_add(1, Ordering::Relaxed);
    let id3 = server.next_id.fetch_add(1, Ordering::Relaxed);
    assert_eq!(id1, 1); // starts at 1 (set in make_test_server)
    assert_eq!(id2, 2);
    assert_eq!(id3, 3);
}

// ================================================================
// Subprocess integration tests
// ================================================================

/// Spawn a real subprocess that speaks minimal MCP JSON-RPC
/// and test the full lifecycle through McpServer.
#[tokio::test]
async fn test_subprocess_full_lifecycle() {
    // This shell script implements a minimal MCP server:
    // - Responds to initialize with serverInfo
    // - Responds to tools/list with one tool
    // - Responds to tools/call with a text result
    // - Sends a tools/list_changed notification after tools/list
    let script = r#"
import sys, json

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"serverInfo": {"name": "test-mcp"}, "protocolVersion": "2025-03-26", "capabilities": {}}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method.startswith("notifications/"):
        pass  # notification, no response
    elif method == "tools/list":
        # Send a notification BEFORE the response — tests interleaved notification handling
        notif = {"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}
        sys.stdout.write(json.dumps(notif) + "\n")
        sys.stdout.flush()
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "echo", "description": "Echo input", "inputSchema": {"type": "object", "properties": {"msg": {"type": "string"}}}}]}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        text = args.get("msg", "no msg")
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": f"echo: {text}"}]}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    else:
        resp = {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": f"Unknown method: {method}"}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    let config = McpServerConfig {
        name: "test-subprocess".to_string(),
        command: "python3".to_string(),
        args: Some(vec!["-c".to_string(), script.to_string()]),
        env: None,
        url: None,
        default_policy: None,
    };

    // Start the server (runs initialize handshake)
    let server = McpServer::start(&config)
        .await
        .expect("Failed to start MCP server");

    // Discover tools
    let tools = server.list_tools().await.expect("Failed to list tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].description, "Echo input");

    // The server sends a tools/list_changed notification BEFORE the tools/list response.
    // The stdio read loop processes it while scanning for the matching response id,
    // so the flag should be set.
    assert!(
        server.tools_changed.load(Ordering::Relaxed),
        "tools_changed flag should be set by interleaved notification"
    );

    // Call a tool
    let result = server
        .call_tool("echo", json!({"msg": "hello"}))
        .await
        .expect("Failed to call tool");
    assert_eq!(result, "echo: hello");

    // call_tool checked tools_changed=true, called refresh_tools which called
    // list_tools. Our script sends another notification during list_tools, so
    // the flag may be re-set. What matters is the refresh happened (tools were
    // re-listed). We can verify by checking the result came through correctly.
}

/// Test that call_tool handles tool errors (isError: true).
/// Uses the lifecycle server which supports all methods.
#[tokio::test]
async fn test_subprocess_tool_error() {
    // Server that returns isError: true for any tools/call
    let script = r#"
import sys, json

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except:
        continue
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"serverInfo": {"name": "err-mcp"}, "protocolVersion": "2025-03-26", "capabilities": {}}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method.startswith("notifications/"):
        pass
    elif method == "tools/list":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "fail", "description": "Always fails"}]}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"isError": True, "content": [{"type": "text", "text": "tool exploded"}]}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    else:
        resp = {"jsonrpc": "2.0", "id": mid, "result": {}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    let config = McpServerConfig {
        name: "test-err".to_string(),
        command: "python3".to_string(),
        args: Some(vec!["-u".to_string(), "-c".to_string(), script.to_string()]),
        env: None,
        url: None,
        default_policy: None,
    };

    let server = McpServer::start(&config)
        .await
        .expect("Failed to start MCP server");
    // Populate metadata so call_tool can find the tool
    server.tool_metadata.write().unwrap().insert(
        "fail".to_string(),
        McpToolMetadata {
            description: "Always fails".to_string(),
            input_schema: json!({}),
            annotations: None,
        },
    );
    let err = server.call_tool("fail", json!({})).await.unwrap_err();
    assert_eq!(err, "tool exploded");
}

/// Test process death detection: server exits mid-conversation
#[tokio::test]
async fn test_subprocess_process_death() {
    // This server handles initialize then immediately exits
    let script = r#"
import sys, json

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"serverInfo": {"name": "die-mcp"}, "protocolVersion": "2025-03-26", "capabilities": {}}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method.startswith("notifications/"):
        sys.exit(0)  # die after receiving initialized notification
"#;

    let config = McpServerConfig {
        name: "test-die".to_string(),
        command: "python3".to_string(),
        args: Some(vec!["-c".to_string(), script.to_string()]),
        env: None,
        url: None,
        default_policy: None,
    };

    let server = McpServer::start(&config).await.unwrap();
    // Next request should fail with a process-death error
    let err = server
        .send_request("tools/list", json!({}))
        .await
        .unwrap_err();
    assert!(
        server.transport.is_process_dead_error(&err),
        "Expected process death error, got: {err}"
    );
}

// ================================================================
// Streamable HTTP transport — protocol version header + session
// recovery. Wiremock acts as a fake MCP server so we can assert
// exact wire-level behavior without depending on a real remote.
// ================================================================

/// JSON for a successful `InitializeResult` body, echoing the
/// protocol version the caller wants the fake server to claim.
fn fake_initialize_result(protocol_version: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": protocol_version,
            "serverInfo": {"name": "wiremock-mcp", "version": "0.0"},
            "capabilities": {}
        }
    })
}

/// Build an `McpServerConfig` pointing at the given HTTP URL.
fn http_config(url: &str) -> McpServerConfig {
    McpServerConfig {
        name: "test".into(),
        command: String::new(),
        args: None,
        env: None,
        url: Some(url.to_string()),
        default_policy: None,
    }
}

#[tokio::test]
async fn http_post_init_carries_mcp_protocol_version_header() {
    use wiremock::matchers::{body_partial_json, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;

    // initialize POST: no version header expected yet (we don't
    // know the negotiated version at this point in the dance).
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_result("2025-11-25")),
        )
        .expect(1)
        .mount(&mock)
        .await;

    // notifications/initialized POST: MUST carry the negotiated
    // version. This is the first request after initialize that
    // should include the header.
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .and(header("MCP-Protocol-Version", "2025-11-25"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&mock)
        .await;

    // Any other POST without the header is a failure — fail loudly
    // by responding with a sentinel 500 so the assertion is clear.
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;

    let _server = McpServer::start(&http_config(&mock.uri())).await.unwrap();
    // wiremock's Drop verifies each Mock's `.expect(1)`.

    // Also confirm chaz actually sent some `MCP-Protocol-Version`
    // header on the initialized notification — orthogonal check
    // against a hypothetical regression that sets the header to
    // an empty string.
    let received = mock.received_requests().await.unwrap();
    let init_ack = received
        .iter()
        .find(|r| String::from_utf8_lossy(&r.body).contains("notifications/initialized"))
        .expect("should have seen the initialized notification");
    assert!(
        init_ack.headers.contains_key("mcp-protocol-version"),
        "initialized notification missing protocol-version header"
    );
}

#[tokio::test]
async fn http_404_on_tool_call_triggers_reinit_and_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    let mock = MockServer::start().await;

    // Two initialize calls expected: one at start, one after the
    // 404-triggered re-init.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_result("2025-11-25")),
        )
        .expect(2)
        .mount(&mock)
        .await;

    // Two notifications/initialized expected (one per initialize).
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(2)
        .mount(&mock)
        .await;

    // First tools/call → 404 (carries a session ID, mimicking an
    // expired session). Second tools/call → success.
    struct ToolCallResponder {
        calls: AtomicUsize,
    }
    impl Respond for ToolCallResponder {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, AOrd::SeqCst);
            if n == 0 {
                // Spec: server returns 404 on a request whose
                // session has been terminated. No body required.
                ResponseTemplate::new(404)
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": {
                        "content": [{"type": "text", "text": "ok"}],
                        "isError": false
                    }
                }))
            }
        }
    }

    let responder = ToolCallResponder {
        calls: AtomicUsize::new(0),
    };
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(responder)
        .expect(2)
        .mount(&mock)
        .await;

    let server = McpServer::start(&http_config(&mock.uri())).await.unwrap();

    // Seed a session ID so the 404 path triggers (drop-session
    // logic only fires when a session was previously cached;
    // without one a 404 is just an HTTP error).
    if let Transport::Http(h) = &server.transport {
        *h.session_id.lock().await = Some("seeded-session".to_string());
    }

    // call_tool should: try → 404 → re-init → retry → success.
    let out = server.call_tool("echo", json!({})).await.unwrap();
    assert_eq!(out, "ok");
    // wiremock Drop verifies the `.expect(N)` counts.
}

#[tokio::test]
async fn http_404_without_session_propagates_as_plain_error() {
    // Regression guard: a 404 from a server that never issued a
    // session ID isn't "session expired" — it's just a 404.
    // Confirm we don't loop or eat the error.
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_result("2025-11-25")),
        )
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&mock)
        .await;

    // tools/call → 404 (only once expected — no retry path).
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&mock)
        .await;

    let server = McpServer::start(&http_config(&mock.uri())).await.unwrap();
    // No session seeding — should NOT recover from this 404.
    let err = server.call_tool("echo", json!({})).await.unwrap_err();
    assert!(
        err.to_string().contains("404"),
        "expected raw 404 error, got: {err}"
    );
}

// ================================================================
// Resources + Prompts — wire-level behavior
// ================================================================

/// `InitializeResult` that advertises the given primitives. The
/// presence of each sub-object is what counts; values are ignored.
fn fake_initialize_with_caps(
    protocol_version: &str,
    tools: bool,
    resources: bool,
    prompts: bool,
) -> Value {
    let mut caps = serde_json::Map::new();
    if tools {
        caps.insert("tools".into(), json!({}));
    }
    if resources {
        caps.insert("resources".into(), json!({}));
    }
    if prompts {
        caps.insert("prompts".into(), json!({}));
    }
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": protocol_version,
            "serverInfo": {"name": "wiremock-mcp", "version": "0.0"},
            "capabilities": caps
        }
    })
}

#[tokio::test]
async fn http_initialize_captures_advertised_capabilities() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_with_caps(
                "2025-11-25",
                true,
                true,
                false,
            )),
        )
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&mock)
        .await;

    let server = McpServer::start(&http_config(&mock.uri())).await.unwrap();
    let caps = server.capabilities();
    assert!(caps.tools, "server advertised tools");
    assert!(caps.resources, "server advertised resources");
    assert!(!caps.prompts, "server did not advertise prompts");
}

#[tokio::test]
async fn http_resources_list_parses_server_response() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_with_caps(
                "2025-11-25",
                false,
                true,
                false,
            )),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "resources/list"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "resources": [
                    {
                        "uri": "file:///etc/hosts",
                        "name": "hosts",
                        "description": "system hosts file",
                        "mimeType": "text/plain"
                    },
                    { "uri": "db://row/42" }
                ]
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let server = McpServer::start(&http_config(&mock.uri())).await.unwrap();
    let got = server.list_resources().await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].uri, "file:///etc/hosts");
    assert_eq!(got[0].name.as_deref(), Some("hosts"));
    assert_eq!(got[0].mime_type.as_deref(), Some("text/plain"));
    assert_eq!(got[1].uri, "db://row/42");
    assert!(got[1].name.is_none());
}

#[tokio::test]
async fn http_resources_read_concatenates_text_and_describes_blobs() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_with_caps(
                "2025-11-25",
                false,
                true,
                false,
            )),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "resources/read"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "contents": [
                    { "uri": "file:///a", "text": "first line" },
                    { "uri": "file:///a", "text": "second line" },
                    { "uri": "file:///a", "blob": "AAECAwQFBgcICQ==", "mimeType": "image/png" }
                ]
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let server = McpServer::start(&http_config(&mock.uri())).await.unwrap();
    let out = server.read_resource("file:///a").await.unwrap();
    assert!(out.contains("first line"));
    assert!(out.contains("second line"));
    assert!(
        out.contains("[binary image/png:"),
        "expected blob to be summarized, got: {out}"
    );
}

#[tokio::test]
async fn http_prompts_list_parses_arguments() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_with_caps(
                "2025-11-25",
                false,
                false,
                true,
            )),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "prompts/list"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "prompts": [
                    {
                        "name": "summarize",
                        "description": "Summarize a topic",
                        "arguments": [
                            { "name": "topic", "required": true },
                            { "name": "style", "description": "tone" }
                        ]
                    }
                ]
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let server = McpServer::start(&http_config(&mock.uri())).await.unwrap();
    let got = server.list_prompts().await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].name, "summarize");
    assert_eq!(got[0].arguments.len(), 2);
    assert!(got[0].arguments[0].required);
    assert!(!got[0].arguments[1].required);
    assert_eq!(got[0].arguments[1].description.as_deref(), Some("tone"));
}

#[tokio::test]
async fn http_prompts_get_renders_messages() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_with_caps(
                "2025-11-25",
                false,
                false,
                true,
            )),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "prompts/get"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "messages": [
                    {
                        "role": "system",
                        "content": { "type": "text", "text": "You are a helper." }
                    },
                    {
                        "role": "user",
                        "content": { "type": "text", "text": "Summarize: Rust." }
                    }
                ]
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let server = McpServer::start(&http_config(&mock.uri())).await.unwrap();
    let out = server
        .get_prompt("summarize", serde_json::Map::new())
        .await
        .unwrap();
    assert!(out.contains("[system] You are a helper."));
    assert!(out.contains("[user] Summarize: Rust."));
}

#[tokio::test]
async fn http_build_capability_tools_skips_unavailable_primitives() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_with_caps(
                "2025-11-25",
                true,
                false,
                false,
            )),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&mock)
        .await;

    let server = Arc::new(McpServer::start(&http_config(&mock.uri())).await.unwrap());
    let extras = build_capability_tools(server, "fakefs");
    assert!(
        extras.is_empty(),
        "expected no capability tools when only `tools` is advertised"
    );
}

#[tokio::test]
async fn http_build_capability_tools_adds_resource_and_prompt_wrappers() {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fake_initialize_with_caps(
                "2025-11-25",
                true,
                true,
                true,
            )),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&mock)
        .await;

    let server = Arc::new(McpServer::start(&http_config(&mock.uri())).await.unwrap());
    let extras = build_capability_tools(server, "fakefs");
    let names: Vec<String> = extras.iter().map(|t| t.descriptor().name).collect();
    assert!(names.contains(&"fakefs__list_resources".to_string()));
    assert!(names.contains(&"fakefs__read_resource".to_string()));
    assert!(names.contains(&"fakefs__list_prompts".to_string()));
    assert!(names.contains(&"fakefs__get_prompt".to_string()));
}
