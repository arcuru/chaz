use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use crate::tool_host::Capability;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info};

/// Execute a shell command and return its output.
///
/// Security: High risk, always requires approval. The host enforces shell
/// grants (allowlist/denylist) at the capability boundary — the tool itself
/// does not inspect grants directly.
pub struct ShellExec;

impl Tool for ShellExec {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "shell".to_string(),
            description: "Execute a shell command and return its stdout, stderr, and exit code. \
                          Runs until it exits, the optional timeout expires, the tool policy \
                          timeout is reached, or the user stops the turn; a stopped command's \
                          whole process group is killed."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Optional working directory for the command"
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Optional timeout in seconds. On expiry the command is \
                                        killed and its output so far is returned. The tool \
                                        policy timeout still applies."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::High,
            approval: ApprovalRequirement::Always,
            timeout: 30,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    crate::tool::ToolError::InvalidArgument("Missing 'command' argument".into())
                })?;

            let working_dir = arguments
                .get("working_dir")
                .and_then(|v| v.as_str())
                .map(String::from);
            let requested = requested_timeout(&arguments)?;
            let policy = ctx.call_deadline.map(|deadline| {
                report_window(deadline.saturating_duration_since(tokio::time::Instant::now()))
            });
            let limit = stop_limit(requested, policy);

            info!(command = %command, ?limit, "Executing shell command via host");

            let result = ctx
                .host()
                .request(
                    &Capability::Shell {
                        command: command.to_string(),
                        working_dir,
                        timeout: limit.map(|limit| limit.after()),
                    },
                    ctx.grants(),
                )
                .await?;

            match result {
                crate::tool_host::CapabilityResult::Shell(output) => {
                    let formatted = match limit.filter(|_| output.timed_out) {
                        Some(limit) => format_timed_out(&output, limit),
                        None => {
                            format_shell_output(&output.stdout, &output.stderr, output.exit_code)
                        }
                    };

                    debug!(
                        exit_code = output.exit_code,
                        timed_out = output.timed_out,
                        elapsed_ms = output.elapsed.as_millis() as u64,
                        output_len = formatted.len(),
                        "Shell command completed"
                    );

                    Ok(formatted)
                }
                _ => Err(crate::tool::ToolError::Execution(
                    "Unexpected host result for shell capability".into(),
                )),
            }
        })
    }
}

/// Largest explicit `timeout`, matching Pi's bash tool (the Node timer cap).
const MAX_TIMEOUT_SECS: f64 = 2_147_483.647;

/// Read the optional `timeout` argument in seconds. Like Pi's bash tool it
/// must be a finite positive number; absent means no per-call deadline.
fn requested_timeout(
    arguments: &Value,
) -> Result<Option<std::time::Duration>, crate::tool::ToolError> {
    let Some(value) = arguments.get("timeout").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    match value.as_f64() {
        Some(secs) if secs > 0.0 && secs <= MAX_TIMEOUT_SECS => {
            Ok(Some(std::time::Duration::from_secs_f64(secs)))
        }
        _ => Err(crate::tool::ToolError::InvalidArgument(format!(
            "'timeout' must be a positive number of seconds no greater than {MAX_TIMEOUT_SECS}"
        ))),
    }
}

/// Time held back from the runtime's policy deadline so the tool can kill
/// the command, collect its partial output and report before the runtime
/// drops the call without any output.
const REPORT_RESERVE: std::time::Duration = std::time::Duration::from_secs(1);

/// The part of the remaining policy time the command itself may use.
fn report_window(remaining: std::time::Duration) -> std::time::Duration {
    remaining.saturating_sub(REPORT_RESERVE.min(remaining / 4))
}

/// Which deadline stops the command.
#[derive(Clone, Copy, Debug, PartialEq)]
enum StopLimit {
    /// The model's `timeout` argument.
    Requested(std::time::Duration),
    /// The tool policy timeout enforced by the runtime.
    Policy(std::time::Duration),
}

impl StopLimit {
    fn after(self) -> std::time::Duration {
        match self {
            StopLimit::Requested(after) | StopLimit::Policy(after) => after,
        }
    }
}

/// The earlier of the requested timeout and the policy window.
fn stop_limit(
    requested: Option<std::time::Duration>,
    policy: Option<std::time::Duration>,
) -> Option<StopLimit> {
    match (requested, policy) {
        (Some(requested), Some(policy)) if policy < requested => Some(StopLimit::Policy(policy)),
        (Some(requested), _) => Some(StopLimit::Requested(requested)),
        (None, Some(policy)) => Some(StopLimit::Policy(policy)),
        (None, None) => None,
    }
}

/// Report a command the host stopped at `limit`.
///
/// The leading `Tool timed out` keeps the runtime's timed-out outcome. The
/// exit status of a killed group is an artifact of the kill, so it is left
/// out; the elapsed time is measured, not the configured limit.
fn format_timed_out(output: &crate::tool_host::ShellOutput, limit: StopLimit) -> String {
    let elapsed = output.elapsed.as_secs_f64();
    let reason = match limit {
        StopLimit::Requested(after) => format!(
            "the command reached its requested timeout of {}s",
            after.as_secs_f64()
        ),
        StopLimit::Policy(_) => {
            "the command reached the shell tool's policy timeout, which a larger \
             `timeout` argument cannot extend"
                .to_string()
        }
    };
    let mut formatted = format!(
        "Tool timed out after {elapsed:.1} seconds: {reason}. Its process group was killed. \
         Output before the stop:\n"
    );
    let budget = OUTPUT_LIMIT - formatted.len();
    let partial = format_shell_output(&output.stdout, &output.stderr, 0);
    if partial.len() <= budget {
        formatted.push_str(&partial);
    } else {
        formatted.push_str(&project_long_output(
            &output.stdout,
            &output.stderr,
            0,
            budget,
        ));
    }
    formatted
}

/// Total byte ceiling for a formatted shell result — the historical cap.
///
/// Results at or under this limit keep their exact historical shape: stdout
/// verbatim, then a tagged stderr line, then an exit-code line. Longer
/// results are projected into a bounded preview (see [`project_long_output`])
/// that keeps a head and tail of each stream, labels both streams, discloses
/// exactly how many bytes were omitted, and always preserves the nonzero-exit
/// verdict — a noisy command can no longer hide that it failed.
const OUTPUT_LIMIT: usize = 10_000;

/// Format a shell result for the model.
fn format_shell_output(stdout: &str, stderr: &str, exit_code: i32) -> String {
    let mut formatted = String::new();
    if !stdout.is_empty() {
        formatted.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !formatted.is_empty() {
            formatted.push('\n');
        }
        formatted.push_str(&format!("[stderr] {stderr}"));
    }
    if exit_code != 0 {
        if !formatted.is_empty() {
            formatted.push('\n');
        }
        formatted.push_str(&format!("[exit code {exit_code}]"));
    }
    if formatted.is_empty() {
        formatted.push_str("[no output]");
    }
    if formatted.len() <= OUTPUT_LIMIT {
        return formatted;
    }
    project_long_output(stdout, stderr, exit_code, OUTPUT_LIMIT)
}

/// Project an over-limit result into a bounded preview.
///
/// The fixed framing (labels, separators, exit line) is charged against
/// `limit` before any output bytes are spent, so the verdict can
/// never be clipped away. Each present stream gets at least half the
/// remaining budget — a long stdout cannot starve stderr — and each long
/// stream keeps a head and a tail around an explicit omission notice.
fn project_long_output(stdout: &str, stderr: &str, exit_code: i32, limit: usize) -> String {
    let exit_line = (exit_code != 0).then(|| format!("[exit code {exit_code}]"));

    let mut overhead = 0;
    if !stdout.is_empty() {
        overhead += "[stdout]\n".len();
    }
    if !stderr.is_empty() {
        overhead += "[stderr]\n".len();
    }
    if let Some(line) = &exit_line {
        overhead += line.len();
    }
    // One separator newline between each pair of sections.
    let sections = (!stdout.is_empty()) as usize
        + (!stderr.is_empty()) as usize
        + exit_line.is_some() as usize;
    overhead += sections.saturating_sub(1);

    let budget = limit.saturating_sub(overhead);
    let (mut stdout_budget, mut stderr_budget) = match (!stdout.is_empty(), !stderr.is_empty()) {
        (true, true) => (budget / 2, budget - budget / 2),
        (true, false) => (budget, 0),
        (false, true) => (0, budget),
        (false, false) => (0, 0),
    };
    // A stream that fits inside its half releases the leftover to the other.
    if stdout.len() <= stdout_budget && stderr.len() > stderr_budget {
        stderr_budget += stdout_budget - stdout.len();
    } else if stderr.len() <= stderr_budget && stdout.len() > stdout_budget {
        stdout_budget += stderr_budget - stderr.len();
    }

    let mut projected = String::new();
    if !stdout.is_empty() {
        projected.push_str("[stdout]\n");
        projected.push_str(&preview_stream(stdout, stdout_budget));
    }
    if !stderr.is_empty() {
        if !projected.is_empty() {
            projected.push('\n');
        }
        projected.push_str("[stderr]\n");
        projected.push_str(&preview_stream(stderr, stderr_budget));
    }
    if let Some(line) = exit_line {
        if !projected.is_empty() {
            projected.push('\n');
        }
        projected.push_str(&line);
    }
    projected
}

/// Preview one output stream within `budget` bytes.
///
/// Text that fits is returned unchanged. Longer text keeps a head and a
/// tail — the tail carries summaries and verdicts — around a notice naming
/// exactly how many bytes were dropped. Every cut lands on a UTF-8 character
/// boundary, so multibyte content can never panic the formatter.
fn preview_stream(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    // Worst-case notice width for this stream: the fixed wording plus one
    // digit for every byte of `s`. Reserving it up front guarantees the
    // assembled preview stays within `budget`.
    let notice_fixed = "\n[… ".len() + " bytes omitted …]\n".len();
    let reserved = notice_fixed + s.len().to_string().len();
    // The caller gives each clipped stream roughly 5 KB or more, so the
    // notice always fits, even with the widest possible byte count.
    let tail_budget = (budget - reserved) / 2;
    let tail_start = s.floor_char_boundary(s.len() - tail_budget);
    let tail = &s[tail_start..];
    let head_len = s.floor_char_boundary(budget - reserved - tail.len());
    let head = &s[..head_len];
    let omitted = s.len() - head.len() - tail.len();
    format!("{head}\n[… {omitted} bytes omitted …]\n{tail}")
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
    fn default_policy_is_high_risk_and_requires_approval() {
        let p = ShellExec.default_policy();
        assert!(matches!(p.risk, RiskLevel::High));
        assert!(matches!(p.approval, ApprovalRequirement::Always));
        assert_eq!(p.timeout, 30);
    }

    #[test]
    fn descriptor_lists_command_required_and_optional_directory_and_timeout() {
        let d = ShellExec.descriptor();
        assert_eq!(d.name, "shell");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "command"));
        assert!(!required.iter().any(|v| v == "working_dir"));
        assert!(!required.iter().any(|v| v == "timeout"));
        assert_eq!(d.parameters["properties"]["timeout"]["type"], "number");
    }

    #[test]
    fn approval_only_policy_override_explains_the_generic_timeout() {
        let policy: ToolPolicy = serde_yaml::from_str("approval: never").unwrap();
        let policies = crate::tool::ToolPolicyRegistry::new(std::collections::HashMap::from([(
            "shell".into(),
            policy,
        )]));
        let effective = policies.resolve(&ShellExec);
        assert_eq!(ShellExec.default_policy().timeout, 30);
        assert_eq!(effective.timeout, 60);
        assert_eq!(effective.risk, RiskLevel::Low);
        assert_eq!(effective.approval, ApprovalRequirement::Never);
    }

    #[tokio::test]
    async fn missing_command_argument_errors() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host.clone()).await;
        let err = ShellExec
            .execute(serde_json::json!({}), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("command"));
        // Host should not have been touched if argument validation fails first.
        assert!(host.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn forwards_command_and_working_dir_to_host() {
        let host = Arc::new(MockHost::new());
        host.push_shell("ok", "", 0);
        let (_i, c) = ctx_with(host.clone()).await;
        ShellExec
            .execute(
                serde_json::json!({ "command": "ls -la", "working_dir": "/tmp" }),
                &c,
            )
            .await
            .unwrap();
        match host.last_call().expect("one host call") {
            Capability::Shell {
                command,
                working_dir,
                timeout,
            } => {
                assert_eq!(timeout, None, "no deadline without an argument or policy");
                assert_eq!(command, "ls -la");
                assert_eq!(working_dir.as_deref(), Some("/tmp"));
            }
            other => panic!("unexpected capability: {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_timeouts_are_rejected_before_running() {
        for timeout in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("10"),
            serde_json::json!(3_000_000),
        ] {
            let host = Arc::new(MockHost::new());
            let (_i, c) = ctx_with(host.clone()).await;
            let err = ShellExec
                .execute(
                    serde_json::json!({ "command": "x", "timeout": timeout }),
                    &c,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, crate::tool::ToolError::InvalidArgument(_)),
                "{timeout}: {err}"
            );
            assert!(host.recorded_calls().is_empty());
        }
    }

    #[test]
    fn the_earlier_deadline_stops_the_command() {
        let s = std::time::Duration::from_secs;
        assert_eq!(stop_limit(None, None), None);
        assert_eq!(
            stop_limit(Some(s(5)), None),
            Some(StopLimit::Requested(s(5)))
        );
        assert_eq!(
            stop_limit(None, Some(s(29))),
            Some(StopLimit::Policy(s(29)))
        );
        assert_eq!(
            stop_limit(Some(s(5)), Some(s(29))),
            Some(StopLimit::Requested(s(5)))
        );
        assert_eq!(
            stop_limit(Some(s(600)), Some(s(29))),
            Some(StopLimit::Policy(s(29)))
        );
        assert_eq!(report_window(s(30)), s(29));
        assert_eq!(
            report_window(std::time::Duration::from_millis(400)),
            std::time::Duration::from_millis(300)
        );
    }

    #[tokio::test]
    async fn the_policy_deadline_bounds_a_longer_requested_timeout() {
        let host = Arc::new(MockHost::new());
        host.push_shell_timed_out("partial\n", std::time::Duration::from_millis(29_000));
        let (_i, mut c) = ctx_with(host.clone()).await;
        c.call_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(30));
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x", "timeout": 600 }), &c)
            .await
            .unwrap();
        let Capability::Shell { timeout, .. } = host.last_call().unwrap() else {
            panic!("expected a shell call");
        };
        let timeout = timeout.expect("policy deadline forwarded");
        assert!(timeout <= std::time::Duration::from_secs(29), "{timeout:?}");
        assert!(timeout > std::time::Duration::from_secs(28), "{timeout:?}");
        assert_eq!(
            out,
            "Tool timed out after 29.0 seconds: the command reached the shell tool's policy \
             timeout, which a larger `timeout` argument cannot extend. Its process group was \
             killed. Output before the stop:\npartial\n"
        );
    }

    #[tokio::test]
    async fn a_requested_timeout_is_reported_without_a_kill_exit_code() {
        let host = Arc::new(MockHost::new());
        host.push_shell_timed_out("", std::time::Duration::from_millis(1_500));
        let (_i, c) = ctx_with(host.clone()).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x", "timeout": 1.5 }), &c)
            .await
            .unwrap();
        let Capability::Shell { timeout, .. } = host.last_call().unwrap() else {
            panic!("expected a shell call");
        };
        assert_eq!(timeout, Some(std::time::Duration::from_millis(1_500)));
        assert!(
            out.starts_with("Tool timed out after 1.5 seconds: "),
            "{out}"
        );
        assert!(out.contains("requested timeout of 1.5s"), "{out}");
        assert!(
            out.ends_with("Output before the stop:\n[no output]"),
            "{out}"
        );
        assert!(!out.contains("exit code"), "{out}");
    }

    #[test]
    fn timed_out_output_keeps_both_tails_within_the_total_cap() {
        let output = crate::tool_host::ShellOutput {
            stdout: format!("stdout head\n{}\nstdout tail", "é".repeat(10_000)),
            stderr: format!("stderr head\n{}\nstderr tail", "日".repeat(10_000)),
            exit_code: 137,
            timed_out: true,
            elapsed: std::time::Duration::from_secs(2),
        };
        for limit in [
            StopLimit::Requested(std::time::Duration::from_secs(2)),
            StopLimit::Policy(std::time::Duration::from_secs(2)),
        ] {
            let out = format_timed_out(&output, limit);
            assert!(out.len() <= OUTPUT_LIMIT, "bound exceeded: {}", out.len());
            assert!(out.starts_with("Tool timed out after 2.0 seconds:"));
            assert!(!out.contains("exit code"), "kill is not a command verdict");
            for retained in ["stdout head", "stdout tail", "stderr head", "stderr tail"] {
                assert!(out.contains(retained), "missing {retained}");
            }
            assert_eq!(out.matches("bytes omitted").count(), 2);
        }
    }

    #[tokio::test]
    async fn stdout_only_passes_through_unannotated() {
        let host = Arc::new(MockHost::new());
        host.push_shell("hello\n", "", 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "echo hello" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "hello\n");
    }

    #[tokio::test]
    async fn stderr_is_tagged_and_appended() {
        let host = Arc::new(MockHost::new());
        host.push_shell("out", "err", 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(out.contains("out"));
        assert!(
            out.contains("[stderr] err"),
            "stderr should be tagged, got: {out}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_code_is_appended() {
        let host = Arc::new(MockHost::new());
        host.push_shell("", "", 7);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(out.contains("[exit code 7]"), "got: {out}");
    }

    #[tokio::test]
    async fn empty_output_returns_placeholder() {
        let host = Arc::new(MockHost::new());
        host.push_shell("", "", 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "true" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "[no output]");
    }

    #[tokio::test]
    async fn very_long_stdout_is_projected_within_bound() {
        let host = Arc::new(MockHost::new());
        host.push_shell(&"a".repeat(15_000), "", 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(
            out.starts_with("[stdout]"),
            "long stdout must be labeled, got: {}",
            &out[..out.len().min(40)]
        );
        assert!(
            out.contains("bytes omitted"),
            "omission must be disclosed, got tail: {}",
            &out[out.len().saturating_sub(60)..]
        );
        assert!(
            out.len() <= 10_000,
            "projected output must respect the 10 KB ceiling, got {}",
            out.len()
        );
    }

    #[tokio::test]
    async fn noisy_stdout_keeps_tail_stderr_and_exit_verdict() {
        // Regression: the old formatter appended stderr and the exit marker
        // after stdout and then cut at byte 10_000, so a noisy command hid
        // both its test summary and the fact that it failed.
        let host = Arc::new(MockHost::new());
        let mut stdout = String::new();
        for i in 0..600 {
            stdout.push_str(&format!("compiling module-{i:04} ...\n"));
        }
        stdout.push_str("test result: FAILED. 3 passed; 42 failed\n");
        host.push_shell(&stdout, "warning: 12 warnings emitted", 7);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(out.contains("[exit code 7]"), "verdict lost, got: {out}");
        assert!(
            out.contains("test result: FAILED"),
            "stdout tail lost, got tail: {}",
            &out[out.len().saturating_sub(200)..]
        );
        assert!(out.contains("[stderr]"), "stderr evidence lost, got: {out}");
        assert!(
            out.contains("warning: 12 warnings emitted"),
            "short stderr should be shown in full, got: {out}"
        );
        assert!(out.contains("bytes omitted"), "no disclosure, got: {out}");
        assert!(out.len() <= 10_000, "bound exceeded: {}", out.len());
    }

    #[tokio::test]
    async fn long_stdout_and_stderr_are_labeled_and_bounded() {
        let host = Arc::new(MockHost::new());
        let stdout = format!("stdout head\n{}\nstdout tail", "o".repeat(30_000));
        let stderr = format!("stderr head\n{}\nstderr tail", "e".repeat(30_000));
        host.push_shell(&stdout, &stderr, 1);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(out.contains("[stdout]"), "stdout unlabeled: {out}");
        assert!(out.contains("[stderr]"), "stderr unlabeled: {out}");
        assert!(out.ends_with("[exit code 1]"), "verdict lost: {out}");
        for retained in ["stdout head", "stdout tail", "stderr head", "stderr tail"] {
            assert!(out.contains(retained), "missing {retained}: {out}");
        }
        assert_eq!(
            out.matches("bytes omitted").count(),
            2,
            "both streams must disclose omissions: {out}"
        );
        assert!(
            out.matches('e').count() >= 1_000,
            "long stdout starved stderr: {out}"
        );
        assert!(
            out.matches('o').count() >= 1_000,
            "stderr starved stdout: {out}"
        );
        assert!(out.len() <= 10_000, "bound exceeded: {}", out.len());
    }

    #[tokio::test]
    async fn long_stderr_only_is_labeled_and_bounded() {
        let host = Arc::new(MockHost::new());
        host.push_shell("", &"e".repeat(15_000), 0);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(
            out.starts_with("[stderr]"),
            "long stderr must be labeled, got: {}",
            &out[..out.len().min(40)]
        );
        assert!(!out.contains("[stdout]"), "no stdout expected: {out}");
        assert!(out.contains("bytes omitted"), "no disclosure: {out}");
        assert!(out.len() <= 10_000, "bound exceeded: {}", out.len());
    }

    #[tokio::test]
    async fn multibyte_output_at_old_cutoff_does_not_panic() {
        // Regression: the old formatter called String::truncate(10_000),
        // which panics when byte 10_000 lands inside a multibyte character.
        let host = Arc::new(MockHost::new());
        let stdout = format!("{}{}", "a".repeat(9_999), "é".repeat(5_000));
        host.push_shell(&stdout, "", 3);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(out.contains('é'), "tail dropped all multibyte text: {out}");
        assert!(out.contains("[exit code 3]"), "verdict lost: {out}");
        assert!(out.contains("bytes omitted"), "no disclosure: {out}");
        assert!(out.len() <= 10_000, "bound exceeded: {}", out.len());
    }

    #[test]
    fn short_results_keep_historical_format() {
        assert_eq!(format_shell_output("", "", 0), "[no output]");
        assert_eq!(format_shell_output("", "", 7), "[exit code 7]");
        assert_eq!(format_shell_output("hello\n", "", 0), "hello\n");
        assert_eq!(format_shell_output("out", "err", 0), "out\n[stderr] err");
        assert_eq!(
            format_shell_output("out", "err", 7),
            "out\n[stderr] err\n[exit code 7]"
        );
        assert_eq!(
            format_shell_output("", "err", 7),
            "[stderr] err\n[exit code 7]"
        );
    }

    #[tokio::test]
    async fn exact_cap_is_returned_untouched() {
        let cases = [
            (format!("{}é", "a".repeat(OUTPUT_LIMIT - 2)), "", 0),
            (
                "a".repeat(OUTPUT_LIMIT - "\n[stderr] err\n[exit code 7]".len()),
                "err",
                7,
            ),
        ];
        for (stdout, stderr, code) in cases {
            let expected = if stderr.is_empty() {
                stdout.clone()
            } else {
                format!("{stdout}\n[stderr] {stderr}\n[exit code {code}]")
            };
            assert_eq!(expected.len(), OUTPUT_LIMIT);
            let host = Arc::new(MockHost::new());
            host.push_shell(&stdout, stderr, code);
            let (_i, c) = ctx_with(host).await;
            let out = ShellExec
                .execute(serde_json::json!({ "command": "x" }), &c)
                .await
                .unwrap();
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn stream_preview_counts_omitted_utf8_bytes() {
        let input = "aé日🦀\n".repeat(4_000);
        for budget in [4_970, 4_971, 4_972, 4_973] {
            let out = preview_stream(&input, budget);
            let (head, rest) = out.split_once("\n[… ").expect("omission notice");
            let (count, tail) = rest.split_once(" bytes omitted …]\n").unwrap();
            let omitted: usize = count.parse().unwrap();
            assert!(input.starts_with(head));
            assert!(input.ends_with(tail));
            assert!(!head.is_empty() && !tail.is_empty());
            assert_eq!(head.len() + omitted + tail.len(), input.len());
            assert!(out.len() <= budget, "bound exceeded: {}", out.len());
        }
    }

    #[test]
    fn one_byte_over_cap_engages_projection() {
        let total = OUTPUT_LIMIT + 1;
        let out = format_shell_output(&"a".repeat(total), "", 0);
        assert!(out.starts_with("[stdout]"));
        assert!(out.len() <= OUTPUT_LIMIT);
        // The omission notice must be accurate: shown bytes plus the
        // disclosed count equal the full stream.
        let shown = out.matches('a').count();
        let disclosed: usize = out
            .split("[… ")
            .nth(1)
            .and_then(|rest| rest.split(" bytes omitted").next())
            .and_then(|count| count.parse().ok())
            .expect("omission notice with byte count");
        assert_eq!(shown + disclosed, total);
    }

    #[test]
    fn short_stderr_is_shown_in_full_alongside_huge_stdout() {
        let out = format_shell_output(&"o".repeat(20_000), "fatal error: disk full", 1);
        assert!(
            out.contains("fatal error: disk full"),
            "short stderr must survive, got: {out}"
        );
        assert!(out.contains("[stdout]"), "stdout unlabeled: {out}");
        assert!(out.contains("[exit code 1]"), "verdict lost: {out}");
        assert!(out.contains("bytes omitted"), "no disclosure: {out}");
        assert!(out.len() <= OUTPUT_LIMIT, "bound exceeded: {}", out.len());
    }

    #[test]
    fn projection_respects_bound_across_shapes() {
        let shapes = [
            (9_999, 0, 0),
            (10_001, 0, 9),
            (12_345, 700, 0),
            (30_000, 30_000, 1),
            (0, 10_001, 2),
            (50_000, 3_000, 127),
        ];
        for (out_len, err_len, code) in shapes {
            let out = format_shell_output(&"x".repeat(out_len), &"y".repeat(err_len), code);
            assert!(
                out.len() <= OUTPUT_LIMIT,
                "shape ({out_len}, {err_len}, {code}) exceeded bound: {}",
                out.len()
            );
            if code != 0 {
                assert!(
                    out.contains(&format!("[exit code {code}]")),
                    "shape ({out_len}, {err_len}, {code}) lost verdict: {out}"
                );
            }
        }
        // Multibyte content exercises every cut boundary.
        let out = format_shell_output(&"é".repeat(6_000), &"日".repeat(6_000), 5);
        assert!(out.len() <= OUTPUT_LIMIT, "bound exceeded: {}", out.len());
        assert!(out.contains("[exit code 5]"), "verdict lost: {out}");
    }
}
