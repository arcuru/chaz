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
            description: "Execute a shell command and return its stdout, stderr, and exit code"
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

            info!(command = %command, "Executing shell command via host");

            let result = ctx
                .host()
                .request(
                    &Capability::Shell {
                        command: command.to_string(),
                        working_dir,
                    },
                    ctx.grants(),
                )
                .await?;

            match result {
                crate::tool_host::CapabilityResult::Shell(output) => {
                    let formatted =
                        format_shell_output(&output.stdout, &output.stderr, output.exit_code);

                    debug!(
                        exit_code = output.exit_code,
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
    project_long_output(stdout, stderr, exit_code)
}

/// Project an over-limit result into a bounded preview.
///
/// The fixed framing (labels, separators, exit line) is charged against
/// [`OUTPUT_LIMIT`] before any output bytes are spent, so the verdict can
/// never be clipped away. Each present stream gets at least half the
/// remaining budget — a long stdout cannot starve stderr — and each long
/// stream keeps a head and a tail around an explicit omission notice.
fn project_long_output(stdout: &str, stderr: &str, exit_code: i32) -> String {
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

    let budget = OUTPUT_LIMIT.saturating_sub(overhead);
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
    if budget <= reserved {
        // Pathologically small budget (unreachable from this tool's call
        // sites): keep whatever head fits and disclose the rest.
        let head_len = floor_char_boundary(s, budget / 2);
        return format!(
            "{}[… {} bytes omitted …]",
            &s[..head_len],
            s.len() - head_len
        );
    }
    let tail_budget = (budget - reserved) / 2;
    let tail_start = floor_char_boundary(s, s.len() - tail_budget);
    let tail = &s[tail_start..];
    let head_len = floor_char_boundary(s, budget - reserved - tail.len());
    let head = &s[..head_len];
    let omitted = s.len() - head.len() - tail.len();
    format!("{head}\n[… {omitted} bytes omitted …]\n{tail}")
}

/// Largest index ≤ `index` that is a UTF-8 character boundary of `s`.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut index = index.min(s.len());
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
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
    fn descriptor_lists_command_required_and_working_dir_optional() {
        let d = ShellExec.descriptor();
        assert_eq!(d.name, "shell");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "command"));
        assert!(!required.iter().any(|v| v == "working_dir"));
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
            } => {
                assert_eq!(command, "ls -la");
                assert_eq!(working_dir.as_deref(), Some("/tmp"));
            }
            other => panic!("unexpected capability: {other:?}"),
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
        host.push_shell(&"o".repeat(30_000), &"e".repeat(30_000), 1);
        let (_i, c) = ctx_with(host).await;
        let out = ShellExec
            .execute(serde_json::json!({ "command": "x" }), &c)
            .await
            .unwrap();
        assert!(out.contains("[stdout]"), "stdout unlabeled: {out}");
        assert!(out.contains("[stderr]"), "stderr unlabeled: {out}");
        assert!(out.contains("[exit code 1]"), "verdict lost: {out}");
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

    #[test]
    fn exact_cap_is_returned_untouched() {
        let stdout = "a".repeat(OUTPUT_LIMIT);
        assert_eq!(format_shell_output(&stdout, "", 0), stdout);
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
