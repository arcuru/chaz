use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use crate::tool_host::Capability;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tracing::{debug, info};

/// Most lines one `read_file` result may contain, notices included.
pub const READ_MAX_LINES: usize = 2000;
/// Most bytes one `read_file` result may contain, notices included.
pub const READ_MAX_BYTES: usize = 51_200;
/// Bytes held back from the content budget for the separator and the
/// trailing status notice. Notices never embed caller-controlled text
/// (no path), so their length is bounded by a few integers.
const NOTICE_RESERVE: usize = 512;
/// Output lines a paged result spends on its notice: one blank separator
/// line plus the notice line itself.
const NOTICE_LINES: usize = 2;

/// Read the contents of a file, optionally one line-bounded page at a time.
///
/// Filesystem grants (read paths) are enforced by the host at the
/// capability boundary. The tool itself does not inspect grants directly;
/// paging only bounds what the model sees, not what the host reads.
pub struct ReadFile;

impl Tool for ReadFile {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read_file".to_string(),
            description: format!(
                "Read the contents of a text file at the given path. Output is capped at \
                 {READ_MAX_LINES} lines or {READ_MAX_BYTES} bytes, whichever comes first. \
                 A short file read from the start is returned verbatim; otherwise the text \
                 is followed by a blank line and one [bracketed] notice giving the lines \
                 shown, the file's total line count, and `offset=N` to continue when more \
                 remains. Page through a large file by passing that offset back."
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file path to read"
                    },
                    "offset": {
                        "type": ["integer", "null"],
                        "description": "1-based line number to start reading from. Null or omitted means 1."
                    },
                    "limit": {
                        "type": ["integer", "null"],
                        "description": "Maximum number of lines to return (positive). Null or omitted means as many as fit the output cap."
                    }
                },
                "required": ["path", "offset", "limit"],
                "additionalProperties": false
            }),
        }
    }

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        use crate::tool::ToolError;
        Box::pin(async move {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'path' argument".into()))?;
            let offset = line_arg(&arguments, "offset")?;
            let limit = line_arg(&arguments, "limit")?;

            debug!(path, ?offset, ?limit, "Reading file via host");

            let result = ctx
                .host()
                .request(
                    &Capability::FileRead {
                        path: path.to_string(),
                    },
                    ctx.grants(),
                )
                .await?;

            match result {
                crate::tool_host::CapabilityResult::FileRead(content) => {
                    let text = String::from_utf8_lossy(&content);
                    debug!(path, bytes = text.len(), "File read complete");
                    page(&text, offset.unwrap_or(1), limit)
                }
                _ => Err(ToolError::Execution(
                    "Unexpected host result for file read capability".into(),
                )),
            }
        })
    }
}

/// Parse an optional positive-integer line argument. Absent and `null` both
/// mean "use the default"; anything else must be a JSON integer >= 1.
fn line_arg(arguments: &Value, name: &str) -> Result<Option<usize>, crate::tool::ToolError> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
            _ => Err(crate::tool::ToolError::InvalidArgument(format!(
                "'{name}' must be a positive integer (1 or more), got {v}"
            ))),
        },
    }
}

/// Select the model-visible page of `text` starting at 1-based line
/// `offset`. Lines keep their own terminators (`\n` or `\r\n`); a page only
/// ever ends on a line boundary, so multibyte characters are never split.
fn page(text: &str, offset: usize, limit: Option<usize>) -> Result<String, crate::tool::ToolError> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total = lines.len();

    if total == 0 {
        return if offset == 1 {
            Ok("[Empty file: 0 lines.]".to_string())
        } else {
            Err(crate::tool::ToolError::InvalidArgument(format!(
                "offset {offset} is beyond the end of the file (empty file, 0 lines)"
            )))
        };
    }
    if offset > total {
        return Err(crate::tool::ToolError::InvalidArgument(format!(
            "offset {offset} is beyond the end of the file ({total} lines)"
        )));
    }

    // A short file read from the start comes back verbatim, exactly as
    // before paging existed.
    let covers_all = limit.is_none_or(|l| l >= total);
    if offset == 1 && covers_all && total <= READ_MAX_LINES && text.len() <= READ_MAX_BYTES {
        return Ok(text.to_string());
    }

    let start = offset - 1;
    let want = limit.unwrap_or(usize::MAX);
    let line_budget = READ_MAX_LINES - NOTICE_LINES;
    let byte_budget = READ_MAX_BYTES - NOTICE_RESERVE;

    let mut end = start;
    let mut bytes = 0usize;
    let mut byte_stop = false;
    while end < total && end - start < want && end - start < line_budget {
        let len = lines[end].len();
        if bytes + len > byte_budget {
            byte_stop = true;
            break;
        }
        bytes += len;
        end += 1;
    }

    if end == start {
        // The first requested line alone exceeds the content budget. Say so rather
        // than returning a cut line or silently skipping it.
        let size = lines[start].len();
        let skip = if offset < total {
            format!(", or use offset={} to skip it", offset + 1)
        } else {
            "; it is the last line".to_string()
        };
        return Ok(format!(
            "[Line {offset} of {total} is {size} bytes, over the {byte_budget}-byte \
             page content budget (the {READ_MAX_BYTES}-byte output ceiling includes the \
             notice), so it was not returned. Read a bounded slice of it with the \
             shell tool (for example: sed -n '{offset}p' FILE | head -c 50000){skip}.]"
        ));
    }

    let last = end; // 1-based number of the last line shown
    let notice = if end == total {
        if lines[total - 1].ends_with('\n') {
            format!("[Lines {offset}-{last} of {total}; end of file.]")
        } else {
            format!(
                "[Lines {offset}-{last} of {total}; end of file (the last line has no final newline).]"
            )
        }
    } else {
        let reason = if byte_stop {
            format!("the {READ_MAX_BYTES}-byte output ceiling")
        } else if end - start == want {
            "the requested limit".to_string()
        } else {
            format!("the {READ_MAX_LINES}-line output ceiling")
        };
        format!(
            "[Lines {offset}-{last} of {total}; stopped at {reason}. Use offset={} to continue.]",
            last + 1
        )
    };
    debug_assert!(notice.len() + 2 <= NOTICE_RESERVE);

    let mut out = String::with_capacity(bytes + 2 + notice.len());
    for line in &lines[start..end] {
        out.push_str(line);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&notice);
    Ok(out)
}

/// Write content to a file.
///
/// Filesystem grants (write paths) are enforced by the host at the
/// capability boundary. The tool itself does not inspect grants directly.
pub struct WriteFile;

impl Tool for WriteFile {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "write_file".to_string(),
            description: "Write content to a file at the given path. Creates the file if it doesn't exist, overwrites if it does.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file path to write to"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
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

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        use crate::tool::ToolError;
        Box::pin(async move {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'path' argument".into()))?;

            let content = arguments
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'content' argument".into()))?;

            info!(path, bytes = content.len(), "Writing file via host");

            let _result = ctx
                .host()
                .request(
                    &Capability::FileWrite {
                        path: path.to_string(),
                        content: content.to_string(),
                    },
                    ctx.grants(),
                )
                .await?;

            Ok(format!("Wrote {} bytes to {path}", content.len()))
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
    fn read_file_descriptor_requires_path() {
        let d = ReadFile.descriptor();
        assert_eq!(d.name, "read_file");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "path"));
    }

    #[test]
    fn write_file_descriptor_requires_path_and_content() {
        let d = WriteFile.descriptor();
        assert_eq!(d.name, "write_file");
        let required = d.parameters["required"].as_array().expect("required[]");
        assert!(required.iter().any(|v| v == "path"));
        assert!(required.iter().any(|v| v == "content"));
    }

    #[test]
    fn write_file_default_policy_is_medium_and_requires_approval_unless_auto() {
        let p = WriteFile.default_policy();
        assert!(matches!(p.risk, RiskLevel::Medium));
        assert!(matches!(
            p.approval,
            ApprovalRequirement::UnlessAutoApproved
        ));
    }

    #[tokio::test]
    async fn read_file_missing_path_errors_without_host_call() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host.clone()).await;
        let err = ReadFile
            .execute(serde_json::json!({}), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("path"));
        assert!(host.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn read_file_returns_host_content_as_utf8() {
        let host = Arc::new(MockHost::new());
        host.push_file_read(b"hello\nworld".to_vec());
        let (_i, c) = ctx_with(host.clone()).await;
        let out = ReadFile
            .execute(serde_json::json!({ "path": "/etc/hosts" }), &c)
            .await
            .unwrap();
        assert_eq!(out, "hello\nworld");
        match host.last_call().unwrap() {
            Capability::FileRead { path } => assert_eq!(path, "/etc/hosts"),
            other => panic!("unexpected capability: {other:?}"),
        }
    }

    /// Run `read_file` against a scripted host returning `content`.
    async fn read(content: &[u8], args: Value) -> Result<String, crate::tool::ToolError> {
        let host = Arc::new(MockHost::new());
        host.push_file_read(content.to_vec());
        let (_i, c) = ctx_with(host).await;
        ReadFile.execute(args, &c).await
    }

    /// Split a paged result into (content, notice). Paged results always
    /// end with a blank line plus one `[...]` notice line.
    fn split_notice(out: &str) -> (&str, &str) {
        let idx = out.rfind("\n\n[").expect("paged output carries a notice");
        (&out[..idx + 1], &out[idx + 2..])
    }

    /// `[Lines a-b of N; ... offset=K ...]` → `Some(K)`, or `None` at EOF.
    fn next_offset(notice: &str) -> Option<usize> {
        let rest = notice.split("offset=").nth(1)?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        Some(digits.parse().unwrap())
    }

    fn assert_within_caps(out: &str) {
        assert!(out.len() <= READ_MAX_BYTES, "{} bytes", out.len());
        assert!(
            out.split('\n').count() <= READ_MAX_LINES,
            "{} lines",
            out.split('\n').count()
        );
    }

    #[test]
    fn read_file_descriptor_is_strict_with_nullable_paging_fields() {
        // Register through the real registry: under debug assertions it
        // validates strict-mode rules, and `definitions()` is the shape the
        // backends serialize.
        let registry = Arc::new(ToolRegistry::new());
        registry.register(ReadFile);
        let defs = crate::tool::ScopedTools::new(registry, None)
            .definitions(&crate::tool::ToolProfile::default());
        let def = defs.iter().find(|d| d.name == "read_file").unwrap();
        assert!(def.strict, "read_file stays strict-mode");
        let p = &def.parameters;
        assert_eq!(p["additionalProperties"], Value::Bool(false));
        let required: Vec<&str> = p["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, ["path", "offset", "limit"]);
        assert_eq!(p["properties"]["path"]["type"], "string");
        for field in ["offset", "limit"] {
            assert_eq!(
                p["properties"][field]["type"],
                serde_json::json!(["integer", "null"]),
                "{field}"
            );
        }
        assert!(def.description.contains("2000 lines"));
        assert!(def.description.contains("51200 bytes"));
    }

    #[tokio::test]
    async fn read_file_short_path_only_read_is_verbatim() {
        for body in [
            &b"a\nb\n"[..],
            b"a\r\nb\r\n",
            b"no newline",
            "h\u{e9}llo \u{1f600}\n".as_bytes(),
        ] {
            let out = read(body, serde_json::json!({ "path": "/f" }))
                .await
                .unwrap();
            assert_eq!(out.as_bytes(), body);
            // Explicit nulls (what a strict-mode model sends) behave the same.
            let out = read(
                body,
                serde_json::json!({ "path": "/f", "offset": null, "limit": null }),
            )
            .await
            .unwrap();
            assert_eq!(out.as_bytes(), body);
        }
    }

    #[tokio::test]
    async fn read_file_empty_file_is_explicit() {
        let out = read(b"", serde_json::json!({ "path": "/e" }))
            .await
            .unwrap();
        assert_eq!(out, "[Empty file: 0 lines.]");
        let err = read(b"", serde_json::json!({ "path": "/e", "offset": 2 }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("beyond the end"), "{err}");
    }

    #[tokio::test]
    async fn read_file_offset_past_eof_is_an_error_naming_total() {
        let err = read(
            b"1\n2\n3\n",
            serde_json::json!({ "path": "/f", "offset": 4 }),
        )
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("offset 4") && msg.contains("3 lines"), "{msg}");
    }

    #[tokio::test]
    async fn read_file_rejects_invalid_offset_and_limit_without_host_call() {
        for (field, bad) in [
            ("offset", serde_json::json!(0)),
            ("offset", serde_json::json!(-1)),
            ("offset", serde_json::json!(1.5)),
            ("offset", serde_json::json!(2.0)),
            ("offset", serde_json::json!("3")),
            ("limit", serde_json::json!(0)),
            ("limit", serde_json::json!(-5)),
            ("limit", serde_json::json!(0.5)),
            ("limit", serde_json::json!(true)),
        ] {
            let host = Arc::new(MockHost::new());
            let (_i, c) = ctx_with(host.clone()).await;
            let mut args = serde_json::json!({ "path": "/f" });
            args[field] = bad.clone();
            let err = ReadFile.execute(args, &c).await.unwrap_err();
            assert!(
                matches!(err, crate::tool::ToolError::InvalidArgument(_)),
                "{field}={bad}: {err}"
            );
            assert!(format!("{err}").contains(field), "{field}={bad}: {err}");
            assert!(host.recorded_calls().is_empty(), "{field}={bad}");
        }
    }

    #[tokio::test]
    async fn read_file_limit_and_offset_window_reports_range_and_continuation() {
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let out = read(
            body.as_bytes(),
            serde_json::json!({ "path": "/f", "offset": 3, "limit": 2 }),
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            "line 3\nline 4\n\n[Lines 3-4 of 10; stopped at the requested limit. Use offset=5 to continue.]"
        );
        let out = read(
            body.as_bytes(),
            serde_json::json!({ "path": "/f", "offset": 9 }),
        )
        .await
        .unwrap();
        assert_eq!(out, "line 9\nline 10\n\n[Lines 9-10 of 10; end of file.]");
    }

    #[tokio::test]
    async fn read_file_crlf_and_missing_final_newline_are_preserved() {
        let body = b"a\r\nb\r\nc";
        let out = read(body, serde_json::json!({ "path": "/f", "limit": 2 }))
            .await
            .unwrap();
        assert_eq!(
            out,
            "a\r\nb\r\n\n[Lines 1-2 of 3; stopped at the requested limit. Use offset=3 to continue.]"
        );
        let out = read(body, serde_json::json!({ "path": "/f", "offset": 3 }))
            .await
            .unwrap();
        assert_eq!(
            out,
            "c\n\n[Lines 3-3 of 3; end of file (the last line has no final newline).]"
        );
    }

    /// Page a file to EOF via the notices and return the reconstructed text.
    async fn reconstruct(body: &[u8], limit: Option<usize>) -> (String, usize) {
        let mut rebuilt = String::new();
        let mut offset = 1;
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 10_000, "paging did not terminate");
            let out = read(
                body,
                serde_json::json!({ "path": "/big", "offset": offset, "limit": limit }),
            )
            .await
            .unwrap();
            assert_within_caps(&out);
            let (content, notice) = split_notice(&out);
            assert!(notice.starts_with(&format!("[Lines {offset}-")), "{notice}");
            match next_offset(notice) {
                Some(next) => {
                    assert!(next > offset, "offset must advance: {notice}");
                    rebuilt.push_str(content);
                    offset = next;
                }
                None => {
                    assert!(notice.contains("end of file"), "{notice}");
                    if notice.contains("no final newline") {
                        rebuilt.push_str(content.strip_suffix('\n').unwrap());
                    } else {
                        rebuilt.push_str(content);
                    }
                    return (rebuilt, pages);
                }
            }
        }
    }

    #[tokio::test]
    async fn read_file_consecutive_pages_reconstruct_large_file_exactly() {
        // 5,000 numbered lines (~ 250 KB): exceeds both ceilings, so the
        // default page stops at the byte ceiling and continues cleanly.
        let body: String = (1..=5000)
            .map(|i| format!("{i:05} the quick brown fox jumps over the lazy dog\n"))
            .collect();
        let (rebuilt, pages) = reconstruct(body.as_bytes(), None).await;
        assert_eq!(rebuilt, body);
        assert!(pages >= 3, "{pages}");

        let first = read(body.as_bytes(), serde_json::json!({ "path": "/big" }))
            .await
            .unwrap();
        let (_, notice) = split_notice(&first);
        assert!(notice.contains("of 5000"), "{notice}");
        assert!(notice.contains("output ceiling"), "{notice}");

        // Same file with an explicit limit, and with no final newline.
        let (rebuilt, _) = reconstruct(body.as_bytes(), Some(777)).await;
        assert_eq!(rebuilt, body);
        let trimmed = body.trim_end_matches('\n');
        let (rebuilt, _) = reconstruct(trimmed.as_bytes(), None).await;
        assert_eq!(rebuilt, trimmed);
    }

    #[tokio::test]
    async fn read_file_line_ceiling_counts_the_notice() {
        let body = "x\n".repeat(READ_MAX_LINES);
        // Exactly READ_MAX_LINES short lines from the start fit verbatim.
        let out = read(body.as_bytes(), serde_json::json!({ "path": "/f" }))
            .await
            .unwrap();
        assert_eq!(out, body);
        // One more line forces paging; the page plus notice stays in the cap.
        let body = "x\n".repeat(READ_MAX_LINES + 1);
        let out = read(
            body.as_bytes(),
            serde_json::json!({ "path": "/f", "limit": 5000 }),
        )
        .await
        .unwrap();
        assert_within_caps(&out);
        let (_, notice) = split_notice(&out);
        assert_eq!(
            notice,
            format!(
                "[Lines 1-{n} of {t}; stopped at the {READ_MAX_LINES}-line output ceiling. Use offset={next} to continue.]",
                n = READ_MAX_LINES - NOTICE_LINES,
                t = READ_MAX_LINES + 1,
                next = READ_MAX_LINES - NOTICE_LINES + 1
            )
        );
    }

    #[tokio::test]
    async fn read_file_byte_ceiling_holds_with_multibyte_lines() {
        // 4-byte emoji lines of 1,001 bytes each: the byte cap binds long
        // before the line cap, and every cut is on a line boundary.
        let line = format!("{}\n", "\u{1f600}".repeat(250));
        let body = line.repeat(200);
        let out = read(body.as_bytes(), serde_json::json!({ "path": "/f" }))
            .await
            .unwrap();
        assert_within_caps(&out);
        let (content, notice) = split_notice(&out);
        assert!(
            notice.contains(&format!("{READ_MAX_BYTES}-byte output ceiling")),
            "{notice}"
        );
        assert_eq!(
            content.len() % line.len(),
            0,
            "page ends on a line boundary"
        );
        let shown = content.len() / line.len();
        assert!(
            notice.starts_with(&format!("[Lines 1-{shown} of 200;")),
            "{notice}"
        );
        assert!(
            notice.contains(&format!("offset={}", shown + 1)),
            "{notice}"
        );
        let (rebuilt, _) = reconstruct(body.as_bytes(), None).await;
        assert_eq!(rebuilt, body);
    }

    #[tokio::test]
    async fn read_file_oversized_line_is_reported_not_skipped() {
        let body = format!("short\n{}\nafter\n", "\u{e9}".repeat(40_000));
        // Paging from line 1 stops before the oversized line…
        let out = read(body.as_bytes(), serde_json::json!({ "path": "/f" }))
            .await
            .unwrap();
        assert_eq!(
            out,
            format!(
                "short\n\n[Lines 1-1 of 3; stopped at the {READ_MAX_BYTES}-byte output ceiling. Use offset=2 to continue.]"
            )
        );
        // …and asking for it reports its size with bounded-read guidance.
        let out = read(
            body.as_bytes(),
            serde_json::json!({ "path": "/f", "offset": 2 }),
        )
        .await
        .unwrap();
        assert_within_caps(&out);
        assert!(out.starts_with("[Line 2 of 3 is 80001 bytes"), "{out}");
        assert!(out.contains("not returned"), "{out}");
        assert!(out.contains("sed -n '2p'"), "{out}");
        assert!(out.contains("offset=3"), "{out}");
        assert!(!out.contains('\u{e9}'), "no partial line content");

        // Oversized last line: nothing to skip to.
        let body = "y".repeat(READ_MAX_BYTES + 1);
        let out = read(body.as_bytes(), serde_json::json!({ "path": "/f" }))
            .await
            .unwrap();
        assert_within_caps(&out);
        assert!(out.starts_with("[Line 1 of 1 is 51201 bytes"), "{out}");
        assert!(out.contains("it is the last line"), "{out}");
        assert!(!out.contains("offset="), "{out}");
    }

    #[tokio::test]
    async fn read_file_content_budget_boundary_is_reported_honestly() {
        let budget = READ_MAX_BYTES - NOTICE_RESERVE;
        for size in [budget, budget + 1, READ_MAX_BYTES] {
            let line = format!("{}\n", "x".repeat(size - 1));
            let body = format!("before\n{line}after\n");
            let out = read(
                body.as_bytes(),
                serde_json::json!({ "path": "/f", "offset": 2 }),
            )
            .await
            .unwrap();
            assert_within_caps(&out);
            if size == budget {
                let (content, notice) = split_notice(&out);
                assert_eq!(content, line);
                assert!(notice.contains("offset=3"), "{notice}");
            } else {
                assert!(
                    out.starts_with(&format!("[Line 2 of 3 is {size} bytes")),
                    "{out}"
                );
                assert!(
                    out.contains(&format!("over the {budget}-byte page content budget")),
                    "{out}"
                );
                assert!(
                    out.contains("not returned") && out.contains("offset=3"),
                    "{out}"
                );
                assert!(!out.contains("over the 51200-byte output ceiling"), "{out}");
            }
        }
    }

    #[tokio::test]
    async fn read_file_paging_keeps_lossy_utf8_decoding_policy() {
        let body = b"a\xff\r\nb";
        let out = read(body, serde_json::json!({ "path": "/f", "limit": 1 }))
            .await
            .unwrap();
        assert_eq!(
            out,
            "a\u{fffd}\r\n\n[Lines 1-1 of 2; stopped at the requested limit. Use offset=2 to continue.]"
        );
        let out = read(body, serde_json::json!({ "path": "/f", "offset": 2 }))
            .await
            .unwrap();
        assert_eq!(
            out,
            "b\n\n[Lines 2-2 of 2; end of file (the last line has no final newline).]"
        );
        let out = read(body, serde_json::json!({ "path": "/f" }))
            .await
            .unwrap();
        assert_eq!(out, "a\u{fffd}\r\nb");
    }

    #[tokio::test]
    async fn read_file_host_error_propagates() {
        let host = Arc::new(MockHost::new());
        host.push_err(crate::tool::ToolError::Execution(
            "Failed to read file: No such file or directory".into(),
        ));
        let (_i, c) = ctx_with(host).await;
        let err = ReadFile
            .execute(serde_json::json!({ "path": "/missing", "offset": 2 }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("No such file"), "{err}");
    }

    #[tokio::test]
    async fn read_file_native_host_enforces_read_grant_and_pages() {
        use crate::grants::{Allowlist, FsGrant, Grants};
        let allowed = tempfile::tempdir().unwrap();
        let denied = tempfile::tempdir().unwrap();
        let inside = allowed.path().join("in.txt");
        let outside = denied.path().join("out.txt");
        std::fs::write(&inside, "1\n2\n3\n").unwrap();
        std::fs::write(&outside, "secret\n").unwrap();

        let (_instance, session) = fresh_session().await;
        let mut c = tool_context_with_host(
            session,
            Arc::new(ToolRegistry::new()),
            Arc::new(crate::tool_host::NativeToolHost::new()),
        );
        c.grants = Grants {
            fs: Some(FsGrant {
                allow_read: Allowlist::Only(vec![allowed.path().display().to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let out = ReadFile
            .execute(
                serde_json::json!({ "path": inside.display().to_string(), "offset": 2, "limit": 1 }),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            "2\n\n[Lines 2-2 of 3; stopped at the requested limit. Use offset=3 to continue.]"
        );

        let err = ReadFile
            .execute(
                serde_json::json!({ "path": outside.display().to_string(), "offset": 1, "limit": 1 }),
                &c,
            )
            .await
            .unwrap_err();
        assert!(!format!("{err}").contains("secret"));
        assert!(
            format!("{err}").contains("outside the allowed paths"),
            "{err}"
        );

        let err = ReadFile
            .execute(
                serde_json::json!({ "path": allowed.path().join("nope").display().to_string() }),
                &c,
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("Failed to read file"), "{err}");
    }

    #[tokio::test]
    async fn write_file_missing_path_errors() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host).await;
        let err = WriteFile
            .execute(serde_json::json!({ "content": "hi" }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("path"));
    }

    #[tokio::test]
    async fn write_file_missing_content_errors() {
        let host = Arc::new(MockHost::new());
        let (_i, c) = ctx_with(host).await;
        let err = WriteFile
            .execute(serde_json::json!({ "path": "/x" }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("content"));
    }

    #[tokio::test]
    async fn write_file_forwards_path_and_content_and_reports_byte_count() {
        let host = Arc::new(MockHost::new());
        host.push_file_write();
        let (_i, c) = ctx_with(host.clone()).await;
        let out = WriteFile
            .execute(
                serde_json::json!({ "path": "/tmp/out", "content": "hi" }),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(out, "Wrote 2 bytes to /tmp/out");
        match host.last_call().unwrap() {
            Capability::FileWrite { path, content } => {
                assert_eq!(path, "/tmp/out");
                assert_eq!(content, "hi");
            }
            other => panic!("unexpected capability: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unexpected_host_result_variant_is_execution_error() {
        // Host returns a Shell result for a FileRead request — defensive path.
        let host = Arc::new(MockHost::new());
        host.push_shell("oops", "", 0);
        let (_i, c) = ctx_with(host).await;
        let err = ReadFile
            .execute(serde_json::json!({ "path": "/x" }), &c)
            .await
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("unexpected"));
    }
}
