//! End-to-end tests that drive `runtime::execute` through specific branches
//! of the ReAct loop against a scripted `MockBackend`. Each test isolates a
//! single behavior (approval, error path, loop detection, leak redaction, …)
//! and asserts on the observable result + the LLM call trace.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use serde_json::json;

use super::{
    MockBackend, empty_secrets, fresh_session, permissive_security, security_with_decision,
    tool_context,
};
use crate::backends::BackendManager;
use crate::error::LlmError;
use crate::gateway::ApprovalDecision;
use crate::runtime::{self, RuntimeMessage};
use crate::tool::{
    ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy,
    ToolPolicyRegistry, ToolRegistry,
};

// ---- Helper tools ----------------------------------------------------------

/// Echoes the `text` argument back; counts invocations.
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
                "properties": { "text": { "type": "string" } },
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

/// Always fails with `ToolError::Execution`.
struct FailingTool;
impl Tool for FailingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "fail".to_string(),
            description: "Always fails.".to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }
    fn execute<'a>(
        &'a self,
        _arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async { Err(ToolError::Execution("kaboom".into())) })
    }
}

/// Returns output containing an OpenAI-shaped API key, to trip the leak detector.
struct LeakyTool;
impl Tool for LeakyTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "leak".to_string(),
            description: "Returns text containing a fake API key.".to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }
    fn execute<'a>(
        &'a self,
        _arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async { Ok("here is a key: sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ012345".to_string()) })
    }
}

/// Echoes back its `text` argument, but declares `ApprovalRequirement::Always`
/// so the runtime hits the approval gate before dispatching.
struct GatedTool;
impl Tool for GatedTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "gated".to_string(),
            description: "Approval-gated echo.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        }
    }
    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(arguments
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string())
        })
    }
    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::High,
            approval: ApprovalRequirement::Always,
            timeout: 60,
            sensitive_params: Vec::new(),
            rate_limit: None,
            grants: Default::default(),
        }
    }
}

// ---- Tests -----------------------------------------------------------------

#[tokio::test]
async fn react_loop_dispatches_tool_call_and_returns_final_text() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;

    let echo = EchoTool::new();
    let call_counter = echo.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(echo);
    let ctx = tool_context(session, Arc::new(registry));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![(
        "call_1".to_string(),
        "echo".to_string(),
        json!({ "text": "hello world" }).to_string(),
    )]);
    mock.push_text("done: hello world");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![
            RuntimeMessage::System("test agent".into()),
            RuntimeMessage::User("echo it".into()),
        ],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("runtime::execute should succeed");

    assert_eq!(outcome.body, "done: hello world");
    assert_eq!(call_counter.load(Ordering::SeqCst), 1);
    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].tools.iter().any(|t| t.name == "echo"));
    assert!(
        calls[1]
            .messages
            .iter()
            .any(|m| matches!(m, RuntimeMessage::AssistantToolCalls { .. }))
    );
    assert!(calls[1].messages.iter().any(|m| match m {
        RuntimeMessage::ToolResult { content, .. } => content.contains("hello world"),
        _ => false,
    }));
    assert_eq!(mock.pending(), 0);
}

#[tokio::test]
async fn empty_tool_registry_uses_no_tools_fast_path() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let ctx = tool_context(session, Arc::new(ToolRegistry::new()));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    mock.push_text("plain reply");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("hi".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("ok");

    assert_eq!(outcome.body, "plain reply");
    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 1, "no-tools path makes one LLM call");
    assert!(
        calls[0].tools.is_empty(),
        "no-tools fast path advertises no tools"
    );
}

#[tokio::test]
async fn backend_supports_tools_false_uses_no_tools_path() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool::new());
    let ctx = tool_context(session, Arc::new(registry));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new().with_supports_tools(false));
    mock.push_text("backend says no tools");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("hi".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("ok");

    assert_eq!(outcome.body, "backend says no tools");
    let calls = mock.recorded_calls();
    assert!(
        calls[0].tools.is_empty(),
        "backend reporting no tool support skips advertising tools"
    );
}

#[tokio::test]
async fn unknown_tool_name_returns_synthetic_message_to_llm() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool::new());
    let ctx = tool_context(session, Arc::new(registry));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![(
        "c1".into(),
        "no_such_tool".into(),
        json!({}).to_string(),
    )]);
    mock.push_text("ok, gave up on that");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("call something missing".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("ok");

    assert_eq!(outcome.body, "ok, gave up on that");
    let calls = mock.recorded_calls();
    let saw_unknown_msg = calls[1].messages.iter().any(|m| match m {
        RuntimeMessage::ToolResult { content, .. } => content.contains("Unknown tool"),
        _ => false,
    });
    assert!(saw_unknown_msg, "unknown tool name surfaces as a ToolResult");
}

#[tokio::test]
async fn tool_execution_error_surfaces_to_llm_and_run_continues() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let mut registry = ToolRegistry::new();
    registry.register(FailingTool);
    let ctx = tool_context(session, Arc::new(registry));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![("c1".into(), "fail".into(), "{}".into())]);
    mock.push_text("acknowledged failure");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("try fail".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("runtime should not abort on tool error");

    assert_eq!(outcome.body, "acknowledged failure");
    let calls = mock.recorded_calls();
    let saw_err = calls[1].messages.iter().any(|m| match m {
        RuntimeMessage::ToolResult { content, .. } => {
            content.contains("Tool error") && content.contains("kaboom")
        }
        _ => false,
    });
    assert!(
        saw_err,
        "tool execution error appears as ToolResult content for the LLM"
    );
}

#[tokio::test]
async fn approval_required_tool_dispatches_when_approved() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let mut registry = ToolRegistry::new();
    registry.register(GatedTool);
    let ctx = tool_context(session, Arc::new(registry));
    let (security, _approver) = security_with_decision(ApprovalDecision::Approve);
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![(
        "c1".into(),
        "gated".into(),
        json!({ "text": "secret" }).to_string(),
    )]);
    mock.push_text("approved and run");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("do the gated thing".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("ok");

    assert_eq!(outcome.body, "approved and run");
    let calls = mock.recorded_calls();
    let saw_result = calls[1].messages.iter().any(|m| match m {
        RuntimeMessage::ToolResult { content, .. } => content.contains("secret"),
        _ => false,
    });
    assert!(saw_result, "gated tool result reaches the second LLM call");
}

#[tokio::test]
async fn approval_required_tool_blocked_when_denied() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let mut registry = ToolRegistry::new();
    registry.register(GatedTool);
    let ctx = tool_context(session, Arc::new(registry));
    let (security, _denier) = security_with_decision(ApprovalDecision::Deny);
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![(
        "c1".into(),
        "gated".into(),
        json!({ "text": "secret" }).to_string(),
    )]);
    mock.push_text("ok i won't");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("do the gated thing".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("ok");

    assert_eq!(outcome.body, "ok i won't");
    let calls = mock.recorded_calls();
    let saw_denial = calls[1].messages.iter().any(|m| match m {
        RuntimeMessage::ToolResult { content, .. } => content.contains("denied by user"),
        _ => false,
    });
    assert!(saw_denial, "denied tool produces 'denied by user' ToolResult");
    // The tool's actual output "secret" should NOT appear (it never executed).
    let leaked = calls[1].messages.iter().any(|m| match m {
        RuntimeMessage::ToolResult { content, .. } => {
            // Exclude the user-prompt that may quote it back.
            content.contains("secret") && !content.contains("denied")
        }
        _ => false,
    });
    assert!(!leaked, "denied tool's output must not reach the LLM");
}

#[tokio::test]
async fn leak_detector_redacts_secret_in_tool_output() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let mut registry = ToolRegistry::new();
    registry.register(LeakyTool);
    let ctx = tool_context(session, Arc::new(registry));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![("c1".into(), "leak".into(), "{}".into())]);
    mock.push_text("acknowledged");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("leak it".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("ok");

    assert_eq!(outcome.body, "acknowledged");
    let calls = mock.recorded_calls();
    let raw_key = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
    let tool_result_content = calls[1]
        .messages
        .iter()
        .find_map(|m| match m {
            RuntimeMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("expected one ToolResult in the second LLM call");
    assert!(
        !tool_result_content.contains(raw_key),
        "raw API key must be redacted before reaching the LLM, got: {tool_result_content}"
    );
    assert!(
        tool_result_content.contains("REDACTED"),
        "redacted output should mark the redaction site, got: {tool_result_content}"
    );
}

#[tokio::test]
async fn loop_detection_breaks_when_same_tool_call_repeats() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let echo = EchoTool::new();
    let call_counter = echo.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(echo);
    let ctx = tool_context(session, Arc::new(registry));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    // Three identical tool calls trip the LOOP_DETECTION_THRESHOLD.
    for i in 0..3 {
        mock.push_tool_calls(vec![(
            format!("c{i}"),
            "echo".into(),
            json!({ "text": "loop" }).to_string(),
        )]);
    }
    // Runtime then forces a no-tools final call after detecting the loop.
    mock.push_text("got unstuck");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("loop please".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("runtime should exit cleanly when looping");

    assert_eq!(outcome.body, "got unstuck");
    // Loop detector trips ON the 3rd repeated call before the tool runs again,
    // so the echo tool only dispatches twice.
    assert_eq!(
        call_counter.load(Ordering::SeqCst),
        2,
        "echo runs for the first two iterations; the third trips the detector"
    );
    let final_call = mock.recorded_calls().pop().expect("at least one call");
    assert!(
        final_call.tools.is_empty(),
        "the loop-break call advertises no tools"
    );
    let saw_break_prompt = final_call.messages.iter().any(|m| match m {
        RuntimeMessage::User(s) => s.contains("stuck in a loop"),
        _ => false,
    });
    assert!(
        saw_break_prompt,
        "loop-break user-prompt is appended before the final no-tools call"
    );
}

#[tokio::test]
async fn multiple_tool_calls_in_one_turn_all_dispatched() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let echo = EchoTool::new();
    let call_counter = echo.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(echo);
    let ctx = tool_context(session, Arc::new(registry));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls(vec![
        ("c1".into(), "echo".into(), json!({ "text": "one" }).to_string()),
        ("c2".into(), "echo".into(), json!({ "text": "two" }).to_string()),
        ("c3".into(), "echo".into(), json!({ "text": "three" }).to_string()),
    ]);
    mock.push_text("all done");
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let outcome = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("three things please".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await
    .expect("ok");

    assert_eq!(outcome.body, "all done");
    assert_eq!(
        call_counter.load(Ordering::SeqCst),
        3,
        "all three tool calls in a single assistant turn dispatch"
    );
    let follow_up = &mock.recorded_calls()[1];
    let result_count = follow_up
        .messages
        .iter()
        .filter(|m| matches!(m, RuntimeMessage::ToolResult { .. }))
        .count();
    assert_eq!(
        result_count, 3,
        "follow-up LLM call sees three ToolResult messages"
    );
}

#[tokio::test]
async fn non_retryable_llm_error_propagates_as_runtime_error() {
    let (_instance, session) = fresh_session().await;
    let secrets = empty_secrets().await;
    let ctx = tool_context(session, Arc::new(ToolRegistry::new()));
    let security = permissive_security();
    let policies = ToolPolicyRegistry::empty();

    let mock = Arc::new(MockBackend::new());
    // No-tools fast path; an auth-failed error is non-retryable and bubbles up.
    mock.push_err(LlmError::AuthFailed {
        status: 401,
        message: "bad key".into(),
    });
    let backend = BackendManager::with_mock(mock.clone(), secrets);

    let result = runtime::execute(
        Some("mock-model"),
        vec![RuntimeMessage::User("hi".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        None,
        None,
    )
    .await;

    let err = match result {
        Ok(_) => panic!("auth error should surface as runtime Err"),
        Err(e) => e,
    };
    assert!(
        err.to_lowercase().contains("bad key") || err.to_lowercase().contains("auth"),
        "auth error message should propagate: {err}"
    );
}
