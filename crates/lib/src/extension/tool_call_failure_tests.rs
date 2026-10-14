use super::*;
use crate::bridge::ApprovalDecision;
use crate::runtime::{ModelCallScope, RuntimeMessage, RuntimeRecord, ToolResultOutcome};
use crate::test_support::{
    MockBackend, RecordingRuntime, empty_secrets, fresh_session, permissive_security,
    security_with_decision, tool_context,
};
use crate::tool::{
    ApprovalRequirement, Tool, ToolDescriptor, ToolError, ToolPolicy, ToolPolicyRegistry,
    ToolRegistry,
};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Mode {
    Continue,
    Block,
    ConstructionPanic,
    PollingPanic,
}

struct Guard {
    mode: Mode,
    calls: Arc<AtomicUsize>,
}
impl handler::HookHandlerToolCall for Guard {
    fn on_tool_call<'a>(
        &'a self,
        _: &'a str,
        args: &'a mut Value,
    ) -> handler::HandlerFuture<'a, ToolCallDecision> {
        if matches!(self.mode, Mode::ConstructionPanic) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            args["content"] = json!("partially-mutated");
            panic!("construction failed after mutation");
        }
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.calls.fetch_add(1, Ordering::SeqCst);
            args["content"] = json!("partially-mutated");
            match self.mode {
                Mode::PollingPanic => panic!("polling failed after mutation"),
                Mode::Block => ToolCallDecision::Block {
                    reason: "guard denies".into(),
                },
                Mode::Continue => ToolCallDecision::Continue,
                Mode::ConstructionPanic => unreachable!(),
            }
        })
    }
}

struct FileEffect(Arc<AtomicUsize>);
impl Tool for FileEffect {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "file_effect".into(),
            description: "local counted write".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            approval: ApprovalRequirement::Always,
            ..Default::default()
        }
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a crate::tool::ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            ctx.host()
                .request(
                    &crate::tool_host::Capability::FileWrite {
                        path: args["path"].as_str().unwrap().into(),
                        content: args["content"].as_str().unwrap().into(),
                    },
                    ctx.grants(),
                )
                .await?;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("written".into())
        })
    }
}

async fn hook_case(
    mode: Mode,
    approval: Option<ApprovalDecision>,
    grant: bool,
    active: bool,
    expected: ToolResultOutcome,
) {
    let hooks = Arc::new(AtomicUsize::new(0));
    let later_hooks = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let mut hub = test_hub().await;
    hub.install_all(vec![
        Arc::new(TestExt::new("guard").tool_call(Arc::new(Guard {
            mode,
            calls: hooks.clone(),
        }))),
        Arc::new(TestExt::new("later").tool_call(Arc::new(Guard {
            mode: Mode::Continue,
            calls: later_hooks.clone(),
        }))),
    ])
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("effect");
    let (_instance, session) = fresh_session().await;
    let registry = Arc::new(ToolRegistry::new());
    registry.register(FileEffect(effects.clone()));
    let mut ctx = tool_context(session, registry);
    if active {
        ctx.active_extensions = all_active(&["guard", "later"]);
    }
    if !grant {
        ctx.agent_capabilities.fs = Some(crate::grants::FsGrant {
            allow_read: Default::default(),
            allow_write: crate::grants::Allowlist::Only(vec![]),
        });
    }
    let approve = approval == Some(ApprovalDecision::Approve);
    let (security, approval_task) = if let Some(decision) = approval {
        let (s, t) = security_with_decision(decision);
        (s, Some(t))
    } else {
        (permissive_security(), None)
    };
    let arguments = json!({"path": file.to_str().unwrap(), "content": "original"}).to_string();
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls([("call".into(), "file_effect".into(), arguments.clone())]);
    mock.push_text("done");
    let backend = crate::backends::BackendManager::with_mock(mock.clone(), empty_secrets().await);
    let recorder = Arc::new(RecordingRuntime::default());
    let out = crate::runtime::execute_with_recorder(
        None,
        vec![RuntimeMessage::User("test".into())],
        &backend,
        &security,
        &ctx,
        &ToolPolicyRegistry::empty(),
        Some(recorder.clone()),
        Some(&hub),
        None,
        ModelCallScope::default(),
    )
    .await
    .unwrap();
    assert_eq!(out.body, "done");
    assert_eq!(mock.pending(), 0);
    let executes = expected == ToolResultOutcome::Success;
    assert_eq!(hooks.load(Ordering::SeqCst), usize::from(active && approve));
    assert_eq!(
        later_hooks.load(Ordering::SeqCst),
        usize::from(active && approve && matches!(mode, Mode::Continue))
    );
    assert_eq!(effects.load(Ordering::SeqCst), usize::from(executes));
    assert_eq!(file.exists(), executes);
    if executes {
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            if active {
                "partially-mutated"
            } else {
                "original"
            }
        );
    }
    let records = recorder.records.lock().unwrap();
    assert_eq!(records.len(), 3);
    // A failed hook does not forge a successful call or replace the model's original request.
    assert!(
        matches!(&records[0], RuntimeRecord::ModelResponse { tool_calls, .. } if tool_calls[0].arguments == arguments)
    );
    let RuntimeRecord::ToolResult {
        outcome,
        output,
        call_id,
        name,
        ..
    } = &records[1]
    else {
        panic!("missing result")
    };
    assert_eq!(*outcome, expected, "{output}");
    assert_eq!(call_id, "call");
    assert_eq!(name, "file_effect");
    if expected == ToolResultOutcome::Blocked {
        assert!(output.contains("Tool blocked by extension:"));
        if matches!(mode, Mode::ConstructionPanic | Mode::PollingPanic) {
            assert!(
                output.contains("guard")
                    && output.contains("tool_call")
                    && output.contains("panicked"),
                "{output}"
            );
        }
    } else if !approve {
        assert!(output.contains("denied by user"));
    } else if !grant {
        assert!(output.contains("outside the allowed paths"));
    }
    let calls = mock.recorded_calls();
    assert!(calls[1].messages.iter().any(|message| matches!(message, RuntimeMessage::ToolResult { call_id, content } if call_id == "call" && content.contains(output))));
    if let Some(task) = approval_task {
        task.abort();
    }
}

#[tokio::test]
async fn construction_panic_blocks_partial_arguments_and_later_hooks() {
    hook_case(
        Mode::ConstructionPanic,
        Some(ApprovalDecision::Approve),
        true,
        true,
        ToolResultOutcome::Blocked,
    )
    .await;
}
#[tokio::test]
async fn polling_panic_blocks_partial_arguments_and_later_hooks() {
    hook_case(
        Mode::PollingPanic,
        Some(ApprovalDecision::Approve),
        true,
        true,
        ToolResultOutcome::Blocked,
    )
    .await;
}
#[tokio::test]
async fn explicit_block_control() {
    hook_case(
        Mode::Block,
        Some(ApprovalDecision::Approve),
        true,
        true,
        ToolResultOutcome::Blocked,
    )
    .await;
}
#[tokio::test]
async fn continue_propagates_mutations_control() {
    hook_case(
        Mode::Continue,
        Some(ApprovalDecision::Approve),
        true,
        true,
        ToolResultOutcome::Success,
    )
    .await;
}
#[tokio::test]
async fn inactive_panicking_hook_is_not_called() {
    for mode in [Mode::ConstructionPanic, Mode::PollingPanic] {
        hook_case(
            mode,
            Some(ApprovalDecision::Approve),
            true,
            false,
            ToolResultOutcome::Success,
        )
        .await;
    }
}
#[tokio::test]
async fn approval_denial_and_missing_callback_precede_panicking_hook() {
    for approval in [None, Some(ApprovalDecision::Deny)] {
        for mode in [Mode::ConstructionPanic, Mode::PollingPanic] {
            hook_case(
                mode,
                approval.clone(),
                true,
                true,
                ToolResultOutcome::Denied,
            )
            .await;
        }
    }
}
#[tokio::test]
async fn host_grant_denial_still_applies_after_successful_hook() {
    hook_case(
        Mode::Continue,
        Some(ApprovalDecision::Approve),
        false,
        true,
        ToolResultOutcome::Error,
    )
    .await;
}
