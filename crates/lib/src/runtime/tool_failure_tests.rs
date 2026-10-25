//! Model-free regressions through the runtime and real loopback/stdio transports.
use super::*;
use crate::config::McpServerConfig;
use crate::grants::{Allowlist, Grants, NetworkGrant};
use crate::mcp::{server::McpServer, test_support::start_initialized};
use crate::test_support::{
    MockBackend, RecordingRuntime, empty_secrets, fresh_session, tool_context,
};
use crate::tool::{ApprovalRequirement, Tool, ToolError, ToolPolicy, ToolRegistry};
use crate::tools::WebFetch;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn run(
    registry: Arc<ToolRegistry>,
    name: &str,
    args: Value,
    expected: ToolResultOutcome,
) -> String {
    let (_instance, session) = fresh_session().await;
    let ctx = tool_context(session, registry);
    let mock = Arc::new(MockBackend::new());
    let arguments = args.to_string();
    mock.push_tool_calls([("call".into(), name.into(), arguments.clone())]);
    mock.push_text("done");
    let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
    // Explicit test admission, including loopback access; production defaults are unchanged.
    let policies = ToolPolicyRegistry::new(std::collections::HashMap::from([(
        name.into(),
        ToolPolicy {
            approval: ApprovalRequirement::Never,
            timeout: 10,
            grants: Grants {
                network: Some(NetworkGrant {
                    endpoints: Allowlist::Permissive,
                    allow_private: true,
                }),
                ..Default::default()
            },
            ..Default::default()
        },
    )]));
    let recorder = Arc::new(RecordingRuntime::default());
    let out = tokio::time::timeout(
        Duration::from_secs(15),
        execute_with_recorder(
            None,
            vec![RuntimeMessage::User("test".into())],
            &backend,
            &crate::test_support::permissive_security(),
            &ctx,
            &policies,
            Some(recorder.clone()),
            None,
            None,
            ModelCallScope::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(out.body, "done");
    assert_eq!(mock.pending(), 0);
    let records = recorder.records.lock().unwrap();
    assert_eq!(records.len(), 3);
    assert!(
        matches!(&records[0], RuntimeRecord::ModelResponse { tool_calls, .. } if tool_calls.len() == 1 && tool_calls[0].arguments == arguments)
    );
    let RuntimeRecord::ToolResult {
        call_id,
        name: recorded_name,
        output,
        outcome,
        ..
    } = &records[1]
    else {
        panic!("missing tool result")
    };
    assert_eq!(call_id, "call");
    assert_eq!(recorded_name, name);
    assert_eq!(*outcome, expected, "{output}");
    let calls = mock.recorded_calls();
    assert!(calls[1].messages.iter().any(|message| matches!(message, RuntimeMessage::ToolResult { call_id, content } if call_id == "call" && content == &wrap_tool_output(name, output))));
    output.clone()
}

// Consume the complete POST before counting an effect, then lose its reply.
async fn peer(
    mcp: bool,
    fault: &'static str,
    idempotent: bool,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/effect", listener.local_addr().unwrap());
    let effects = Arc::new(AtomicUsize::new(0));
    let counter = effects.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (end, length) = loop {
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0, "incomplete request headers");
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    assert!(headers.starts_with("POST "));
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (end + 4, length);
                }
                assert!(bytes.len() < 8192);
            };
            while bytes.len() < end + length {
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0, "incomplete request body");
                bytes.extend_from_slice(&buf[..n]);
            }
            let request: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
            let effect = !mcp || request["method"] == "tools/call";
            let nth = if effect {
                counter.fetch_add(1, Ordering::SeqCst) + 1
            } else {
                0
            };
            let result = if !mcp {
                json!({"ok": true})
            } else {
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"protocolVersion": "2025-11-25", "serverInfo": {"name": "local-test", "version": "0"}, "capabilities": {"tools": {}}})
                    }
                    "tools/list" => {
                        json!({"tools": [{"name": "mutate", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": idempotent, "destructiveHint": !idempotent, "idempotentHint": idempotent}}]})
                    }
                    "tools/call" => {
                        json!({"content": [{"type": "text", "text": "ok"}], "isError": fault == "execution"})
                    }
                    "notifications/initialized" => json!({}),
                    other => panic!("unexpected MCP method {other}"),
                };
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            };
            let body = serde_json::to_vec(&result).unwrap();
            if effect && nth == 1 && fault == "headers" {
                stream.shutdown().await.unwrap();
                continue;
            }
            let truncate = effect && ((nth == 1 && fault == "body") || fault == "always-body");
            let declared = if truncate {
                body.len() + 100
            } else {
                body.len()
            };
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    (url, effects, handle)
}

#[tokio::test]
async fn web_lost_replies_are_errors_after_one_effect() {
    for fault in ["none", "headers", "body", "always-body"] {
        let (url, effects, handle) = peer(false, fault, false).await;
        let registry = Arc::new(ToolRegistry::new());
        registry.register(WebFetch);
        let output = run(
            registry,
            "web_fetch",
            json!({"url": url, "method": "POST", "body": "{}"}),
            if fault == "none" {
                ToolResultOutcome::Success
            } else {
                ToolResultOutcome::Error
            },
        )
        .await;
        assert_eq!(effects.load(Ordering::SeqCst), 1, "fault={fault}");
        if fault == "none" {
            assert!(output.contains("ok"));
        } else {
            assert!(output.contains("network error:"), "{output}");
        }
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn mcp_http_lost_replies_never_resubmit_even_with_idempotent_hints() {
    for idempotent in [false, true] {
        for fault in ["none", "headers", "body", "always-body", "execution"] {
            let (url, effects, handle) = peer(true, fault, idempotent).await;
            let server = Arc::new(
                start_initialized(&mcp_config(Some(url), vec![]))
                    .await
                    .unwrap(),
            );
            let tool = server
                .discover_and_wrap_tools("local")
                .await
                .unwrap()
                .remove(0);
            let registry = Arc::new(ToolRegistry::new());
            registry.register(tool);
            let output = run(
                registry,
                "local__mutate",
                json!({}),
                if fault == "none" {
                    ToolResultOutcome::Success
                } else {
                    ToolResultOutcome::Error
                },
            )
            .await;
            assert_eq!(
                effects.load(Ordering::SeqCst),
                1,
                "fault={fault} idempotent={idempotent}"
            );
            if matches!(fault, "headers" | "body" | "always-body") {
                assert!(output.contains("network error:"), "{output}");
            }
            handle.abort();
            assert!(handle.await.unwrap_err().is_cancelled());
        }
    }
}

fn mcp_config(url: Option<String>, args: Vec<String>) -> McpServerConfig {
    McpServerConfig {
        name: "local".into(),
        command: "python3".into(),
        args: Some(args),
        env: None,
        url,
        default_policy: None,
        startup_timeout_secs: 5,
    }
}

async fn stdio_peer(
    directory: &std::path::Path,
    before_effect: bool,
    recovery_fails: bool,
) -> Arc<McpServer> {
    // A fsynced file represents the effect, committed before losing the reply.
    let script = r#"
import json, os, sys
path, before, recovery_fails = sys.argv[1:]
for line in sys.stdin:
    req = json.loads(line)
    method = req['method']
    if method.startswith('notifications/'):
        continue
    if method == 'initialize':
        if recovery_fails == 'true' and os.path.exists(path + '/called'):
            print(json.dumps({'jsonrpc': '2.0', 'id': req['id'], 'error': {'code': -32603, 'message': 'recovery rejected'}}), flush=True)
            continue
        result = {'protocolVersion': '2025-11-25', 'serverInfo': {'name': 'local-test'}, 'capabilities': {'tools': {}}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'mutate', 'inputSchema': {'type': 'object'}, 'annotations': {'destructiveHint': True, 'idempotentHint': False}}]}
    elif method == 'tools/call':
        first = not os.path.exists(path + '/called')
        with open(path + '/called', 'a') as f:
            f.write('call\n')
        if first and before == 'true':
            sys.exit(0)
        with open(path + '/effects', 'a') as f:
            f.write('effect\n')
            f.flush()
            os.fsync(f.fileno())
        if first:
            sys.exit(0)
        result = {'content': [{'type': 'text', 'text': 'ok'}]}
    else:
        raise AssertionError(method)
    print(json.dumps({'jsonrpc': '2.0', 'id': req['id'], 'result': result}), flush=True)
"#;
    Arc::new(
        start_initialized(&mcp_config(
            None,
            vec![
                "-u".into(),
                "-c".into(),
                script.into(),
                directory.to_str().unwrap().into(),
                before_effect.to_string(),
                recovery_fails.to_string(),
            ],
        ))
        .await
        .unwrap(),
    )
}

fn line_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}

#[tokio::test]
async fn mcp_stdio_process_death_is_not_replayed_by_either_layer() {
    for before_effect in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let server = stdio_peer(directory.path(), before_effect, false).await;
        let tool = server
            .discover_and_wrap_tools("local")
            .await
            .unwrap()
            .remove(0);
        let registry = Arc::new(ToolRegistry::new());
        registry.register(tool);
        let output = run(
            registry.clone(),
            "local__mutate",
            json!({}),
            ToolResultOutcome::Error,
        )
        .await;
        assert_eq!(
            output,
            "Tool error: network error: MCP server 'local' closed stdout"
        );
        assert_eq!(line_count(&directory.path().join("called")), 1);
        assert_eq!(
            line_count(&directory.path().join("effects")),
            usize::from(!before_effect)
        );
        // Connection recovery is retained; only a new, separately issued call executes again.
        assert_eq!(
            run(
                registry,
                "local__mutate",
                json!({}),
                ToolResultOutcome::Success
            )
            .await,
            "ok"
        );
        assert_eq!(line_count(&directory.path().join("called")), 2);
        assert_eq!(
            line_count(&directory.path().join("effects")),
            usize::from(!before_effect) + 1
        );
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn mcp_stdio_single_execute_does_not_resubmit_after_lost_reply() {
    let directory = tempfile::tempdir().unwrap();
    let server = stdio_peer(directory.path(), false, false).await;
    let tool = server
        .discover_and_wrap_tools("local")
        .await
        .unwrap()
        .remove(0);
    let (_instance, session) = fresh_session().await;
    let ctx = tool_context(session, Arc::new(ToolRegistry::new()));
    let result = tokio::time::timeout(Duration::from_secs(10), tool.execute(json!({}), &ctx))
        .await
        .unwrap();
    assert!(
        matches!(result, Err(ToolError::Network(ref message)) if message.contains("closed stdout")),
        "{result:?}"
    );
    assert_eq!(line_count(&directory.path().join("effects")), 1);
    assert_eq!(line_count(&directory.path().join("called")), 1);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn web_send_failure_before_effect_is_recorded_as_error() {
    // Closed local port: an actual NativeToolHost send failure, not a fake classification.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/effect", listener.local_addr().unwrap());
    drop(listener);
    let registry = Arc::new(ToolRegistry::new());
    registry.register(WebFetch);
    let output = run(
        registry,
        "web_fetch",
        json!({"url": url, "method": "POST", "body": "{}"}),
        ToolResultOutcome::Error,
    )
    .await;
    assert!(output.contains("network error:"), "{output}");
}

struct FailedCall {
    mode: &'static str,
    calls: Arc<AtomicUsize>,
}
impl Tool for FailedCall {
    fn descriptor(&self) -> crate::tool::ToolDescriptor {
        crate::tool::ToolDescriptor {
            name: "failed_call".into(),
            description: "counted failed call".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn execute<'a>(
        &'a self,
        _: Value,
        _: &'a crate::tool::ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                "network" => Err(ToolError::Network("send failed".into())),
                "execution" => Err(ToolError::Execution("operation failed".into())),
                "timeout" => Err(ToolError::Timeout { secs: 10 }),
                "hang" => std::future::pending().await,
                _ => unreachable!(),
            }
        })
    }
}

#[tokio::test]
async fn errors_and_total_timeout_invoke_the_tool_once() {
    for mode in ["network", "execution", "timeout", "hang"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(ToolRegistry::new());
        registry.register(FailedCall {
            mode,
            calls: calls.clone(),
        });
        let output = run(
            registry,
            "failed_call",
            json!({}),
            if mode == "hang" {
                ToolResultOutcome::TimedOut
            } else {
                ToolResultOutcome::Error
            },
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "mode={mode}");
        assert!(
            output.contains(match mode {
                "network" => "send failed",
                "execution" => "operation failed",
                "timeout" => "tool timed out after 10s",
                "hang" => "Tool timed out after 10 seconds",
                _ => unreachable!(),
            }),
            "{output}"
        );
    }
}

#[tokio::test]
async fn mcp_stdio_failed_recovery_preserves_the_original_call_error() {
    let directory = tempfile::tempdir().unwrap();
    let server = stdio_peer(directory.path(), false, true).await;
    let tool = server
        .discover_and_wrap_tools("local")
        .await
        .unwrap()
        .remove(0);
    let registry = Arc::new(ToolRegistry::new());
    registry.register(tool);
    let output = run(
        registry,
        "local__mutate",
        json!({}),
        ToolResultOutcome::Error,
    )
    .await;
    assert_eq!(
        output,
        "Tool error: network error: MCP server 'local' closed stdout"
    );
    assert_eq!(line_count(&directory.path().join("called")), 1);
    assert_eq!(line_count(&directory.path().join("effects")), 1);
    server.shutdown().await.unwrap();
}

/// Run one real native `shell` call under a policy timeout and return its
/// recorded outcome, model-visible output and the call's wall time.
#[cfg(unix)]
async fn run_shell(policy_timeout: u64, args: Value) -> (ToolResultOutcome, String, Duration) {
    let (_instance, session) = fresh_session().await;
    let registry = Arc::new(ToolRegistry::new());
    registry.register(crate::tools::ShellExec);
    let ctx = tool_context(session, registry);
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls([("call".into(), "shell".into(), args.to_string())]);
    mock.push_text("done");
    let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
    let policies = ToolPolicyRegistry::new(std::collections::HashMap::from([(
        "shell".into(),
        ToolPolicy {
            approval: ApprovalRequirement::Never,
            timeout: policy_timeout,
            ..crate::tools::ShellExec.default_policy()
        },
    )]));
    let recorder = Arc::new(RecordingRuntime::default());
    let started = std::time::Instant::now();
    let out = execute_with_recorder(
        None,
        vec![RuntimeMessage::User("test".into())],
        &backend,
        &crate::test_support::permissive_security(),
        &ctx,
        &policies,
        Some(recorder.clone()),
        None,
        None,
        ModelCallScope::default(),
    )
    .await
    .unwrap();
    let wall = started.elapsed();
    assert_eq!(out.body, "done");
    let records = recorder.records.lock().unwrap();
    let RuntimeRecord::ToolResult {
        output, outcome, ..
    } = &records[1]
    else {
        panic!("missing tool result")
    };
    (outcome.clone(), output.clone(), wall)
}

/// The policy deadline stops the command's whole process group before the
/// runtime drops the call, so the result keeps the partial output and its
/// timed-out classification instead of a bare runtime timeout.
#[cfg(unix)]
#[tokio::test]
async fn a_shell_policy_timeout_reports_partial_output_and_kills_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let leaked = directory.path().join("leaked");
    let command = format!(
        "echo started; (sleep 3; echo leaked > '{}') & sleep 30; echo finished",
        leaked.display()
    );
    let (outcome, output, wall) = run_shell(2, json!({ "command": command })).await;
    assert_eq!(outcome, ToolResultOutcome::TimedOut, "{output}");
    assert!(output.starts_with("Tool timed out after 1."), "{output}");
    assert!(output.contains("policy timeout"), "{output}");
    assert!(output.contains("started"), "{output}");
    assert!(!output.contains("finished"), "{output}");
    assert!(!output.contains("[exit code"), "{output}");
    assert!(wall < Duration::from_secs(3), "stopped at {wall:?}");
    tokio::time::sleep(Duration::from_millis(3200)).await;
    assert!(!leaked.exists(), "a timed-out shell descendant survived");
}

/// A requested `timeout` shorter than the policy stops the command at the
/// requested time and says so.
#[cfg(unix)]
#[tokio::test]
async fn a_requested_shell_timeout_stops_before_the_policy() {
    let (outcome, output, wall) = run_shell(
        30,
        json!({ "command": "echo partial; sleep 30", "timeout": 0.5 }),
    )
    .await;
    assert_eq!(outcome, ToolResultOutcome::TimedOut, "{output}");
    assert!(output.starts_with("Tool timed out after 0."), "{output}");
    assert!(output.contains("requested timeout of 0.5s"), "{output}");
    assert!(output.ends_with("partial\n"), "{output}");
    assert!(wall < Duration::from_secs(5), "stopped at {wall:?}");
}

/// A command that finishes inside its deadline keeps the historical result:
/// complete output, its real exit status and a success classification.
#[cfg(unix)]
#[tokio::test]
async fn a_completed_shell_command_keeps_its_exit_status() {
    let (outcome, output, _) = run_shell(
        30,
        json!({ "command": "echo 'test result: ok. 2 passed'; exit 3", "timeout": 20 }),
    )
    .await;
    assert_eq!(outcome, ToolResultOutcome::Success, "{output}");
    assert_eq!(output, "test result: ok. 2 passed\n\n[exit code 3]");
}

/// Manual check that a command runs past the former 30s/60s shell limits
/// when the operator's policy allows it, and still reports its final output,
/// exit status and wall time. Minutes long, so ignored by default:
///
/// ```text
/// cargo test -p chaz-core -- --ignored --nocapture a_shell_command_outlives_the_former_limit
/// ```
#[cfg(unix)]
#[tokio::test]
#[ignore = "runs for over a minute"]
async fn a_shell_command_outlives_the_former_limit() {
    // Exercise this exact build's real tests, not a printed fake summary.
    // The final nonzero status also checks the shell verdict survives.
    let command = format!(
        "sleep 65; '{}' --nocapture tools::shell::tests && exit 3",
        std::env::current_exe().unwrap().display()
    );
    let (outcome, output, wall) = run_shell(600, json!({ "command": command })).await;
    println!(
        "[outcome] {outcome:?} after {:.1}s\n{output}",
        wall.as_secs_f64()
    );
    assert_eq!(outcome, ToolResultOutcome::Success, "{output}");
    assert!(wall > Duration::from_secs(60), "finished at {wall:?}");
    assert!(output.contains("test result: ok."), "{output}");
    assert!(output.contains("0 failed"), "{output}");
    assert!(output.ends_with("[exit code 3]"), "{output}");
}
