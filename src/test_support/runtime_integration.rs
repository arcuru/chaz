//! End-to-end test that drives `runtime::execute` through one full ReAct
//! turn (user message → tool call → tool dispatch → final text response)
//! against a scripted `MockBackend`. Validates the test_support harness.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use serde_json::json;

use super::{MockBackend, empty_secrets, fresh_session, permissive_security, tool_context};
use crate::backends::BackendManager;
use crate::runtime::{self, RuntimeMessage};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicyRegistry, ToolRegistry};

/// Minimal Tool impl for tests: echoes its `text` argument back, optionally
/// failing on the first N calls to exercise retry paths. Records the call
/// count so tests can assert dispatch happened.
struct EchoTool {
    calls: Arc<AtomicUsize>,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "echo".to_string(),
            description: "Return the `text` argument back to the caller.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let text = arguments
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("missing `text`".into()))?;
            Ok(text.to_string())
        })
    }
}

#[tokio::test]
async fn react_loop_dispatches_tool_call_and_returns_final_text() {
    // ---- Setup ----
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;

    let echo = EchoTool::new();
    let call_counter = echo.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(echo);
    let registry = Arc::new(registry);

    let ctx = tool_context(session.clone(), registry);
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    // Turn 1: LLM asks to call echo("hello world").
    mock.push_tool_calls(vec![(
        "call_1".to_string(),
        "echo".to_string(),
        json!({ "text": "hello world" }).to_string(),
    )]);
    // Turn 2: LLM acknowledges and produces final text.
    mock.push_text("done: hello world");

    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let initial = vec![
        RuntimeMessage::System("You are a test agent.".into()),
        RuntimeMessage::User("please echo hello world".into()),
    ];

    // ---- Act ----
    let outcome = runtime::execute(
        Some("mock-model"),
        initial,
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("runtime::execute should succeed");

    // ---- Assert ----
    assert_eq!(
        outcome.body, "done: hello world",
        "final body should be the second-turn text response"
    );
    assert_eq!(
        call_counter.load(Ordering::SeqCst),
        1,
        "echo tool should have been dispatched exactly once"
    );

    let calls = mock.recorded_calls();
    assert_eq!(
        calls.len(),
        2,
        "LLM should be called twice: once for the tool call, once for the follow-up"
    );
    // First call: tool definitions present (echo registered), no tool result yet in messages.
    assert!(
        calls[0].tools.iter().any(|t| t.name == "echo"),
        "first LLM call should advertise the echo tool"
    );
    assert!(
        calls[0]
            .messages
            .iter()
            .all(|m| !matches!(m, RuntimeMessage::ToolResult { .. })),
        "first LLM call should not contain any ToolResult messages"
    );
    // Second call: the conversation now includes the assistant tool-call and a ToolResult.
    assert!(
        calls[1]
            .messages
            .iter()
            .any(|m| matches!(m, RuntimeMessage::AssistantToolCalls { .. })),
        "second LLM call should include the AssistantToolCalls turn"
    );
    let saw_result = calls[1].messages.iter().any(|m| match m {
        RuntimeMessage::ToolResult { content, .. } => content.contains("hello world"),
        _ => false,
    });
    assert!(saw_result, "second LLM call should include the tool result");

    assert_eq!(mock.pending(), 0, "script should be fully consumed");
}
