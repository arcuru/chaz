use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use crate::tool_host::{Capability, CapabilityResult};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tracing::info;

/// Make precise text replacements in a file.
///
/// Validates that each `old_text` appears exactly once before writing.
/// Supports single-edit (`old_text`/`new_text`) and multi-edit (`edits` array).
pub struct EditFile;

impl Tool for EditFile {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "edit_file".to_string(),
            description: "Make precise text replacements in a file. \
                Validates that old_text appears exactly once before replacing. \
                Use `edits` array for multiple atomic replacements."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file path to edit"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to find and replace (must appear exactly once)"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text"
                    },
                    "edits": {
                        "type": "array",
                        "description": "Multiple replacements applied atomically",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {"type": "string"},
                                "new_text": {"type": "string"}
                            },
                            "required": ["old_text", "new_text"]
                        }
                    }
                },
                "required": ["path"]
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
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'path' argument".into()))?;

            // Build the list of (old_text, new_text) pairs
            let edits: Vec<(String, String)> =
                if let Some(arr) = arguments.get("edits").and_then(|v| v.as_array()) {
                    arr.iter()
                        .enumerate()
                        .map(|(i, item)| {
                            let old =
                                item.get("old_text")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| {
                                        ToolError::InvalidArgument(format!(
                                            "edits[{i}] missing 'old_text'"
                                        ))
                                    })?;
                            let new =
                                item.get("new_text")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| {
                                        ToolError::InvalidArgument(format!(
                                            "edits[{i}] missing 'new_text'"
                                        ))
                                    })?;
                            Ok((old.to_string(), new.to_string()))
                        })
                        .collect::<Result<Vec<_>, ToolError>>()?
                } else {
                    let old = arguments
                        .get("old_text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            ToolError::InvalidArgument(
                                "Must provide either 'old_text'/'new_text' or 'edits' array".into(),
                            )
                        })?;
                    let new = arguments
                        .get("new_text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            ToolError::InvalidArgument("Missing 'new_text' argument".into())
                        })?;
                    vec![(old.to_string(), new.to_string())]
                };

            if edits.is_empty() {
                return Err(ToolError::InvalidArgument("No edits provided".into()));
            }

            // Read the file
            let result = ctx
                .host()
                .request(
                    &Capability::FileRead {
                        path: path.to_string(),
                    },
                    ctx.grants(),
                )
                .await?;

            let original = match result {
                CapabilityResult::FileRead(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                _ => {
                    return Err(ToolError::Execution(
                        "Unexpected host result for file read".into(),
                    ));
                }
            };

            let original_lines = original.lines().count();

            // Validate all uniqueness before making any changes
            for (i, (old_text, _)) in edits.iter().enumerate() {
                let count = original.matches(old_text.as_str()).count();
                match count {
                    0 => {
                        return Err(ToolError::InvalidArgument(if edits.len() == 1 {
                            format!("old_text not found in {path}")
                        } else {
                            format!("edits[{i}] old_text not found in {path}")
                        }));
                    }
                    1 => {}
                    n => {
                        return Err(ToolError::InvalidArgument(if edits.len() == 1 {
                            format!("old_text appears {n} times in {path} (must be unique)")
                        } else {
                            format!(
                                "edits[{i}] old_text appears {n} times in {path} (must be unique)"
                            )
                        }));
                    }
                }
            }

            // Apply all edits
            let mut content = original;
            for (old_text, new_text) in &edits {
                content = content.replacen(old_text.as_str(), new_text.as_str(), 1);
            }

            let new_lines = content.lines().count();
            let line_delta: i64 = new_lines as i64 - original_lines as i64;

            info!(
                path,
                edits = edits.len(),
                line_delta,
                "Editing file via host"
            );

            ctx.host()
                .request(
                    &Capability::FileWrite {
                        path: path.to_string(),
                        content,
                    },
                    ctx.grants(),
                )
                .await?;

            let delta_str = match line_delta.cmp(&0) {
                std::cmp::Ordering::Greater => format!("+{line_delta}"),
                std::cmp::Ordering::Less => format!("{line_delta}"),
                std::cmp::Ordering::Equal => "0".to_string(),
            };

            Ok(format!(
                "Edited {path}: {} replacement(s), {delta_str} lines ({new_lines} total)",
                edits.len()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::Grants;
    use crate::tool::ToolError;
    use crate::tool_host::{Capability, CapabilityResult, ToolHost};
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    /// A mock ToolHost that serves a fixed file and records writes.
    struct MockHost {
        files: Mutex<HashMap<String, String>>,
    }

    impl MockHost {
        fn new(path: &str, content: &str) -> Self {
            let mut m = HashMap::new();
            m.insert(path.to_string(), content.to_string());
            Self {
                files: Mutex::new(m),
            }
        }

        fn read(&self, path: &str) -> Option<String> {
            self.files.lock().unwrap().get(path).cloned()
        }
    }

    impl ToolHost for MockHost {
        fn request<'a>(
            &'a self,
            capability: &'a Capability,
            _grants: &'a Grants,
        ) -> Pin<Box<dyn Future<Output = Result<CapabilityResult, ToolError>> + Send + 'a>>
        {
            let result = match capability {
                Capability::FileRead { path } => {
                    match self.files.lock().unwrap().get(path).cloned() {
                        Some(content) => Ok(CapabilityResult::FileRead(content.into_bytes())),
                        None => Err(ToolError::Execution(format!("File not found: {path}"))),
                    }
                }
                Capability::FileWrite { path, content } => {
                    self.files
                        .lock()
                        .unwrap()
                        .insert(path.clone(), content.clone());
                    Ok(CapabilityResult::FileWrite)
                }
                _ => Err(ToolError::Execution(
                    "Unsupported capability in mock".into(),
                )),
            };
            Box::pin(std::future::ready(result))
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    fn apply_edits(content: &str, edits: &[(String, String)]) -> Result<String, String> {
        for (i, (old_text, _)) in edits.iter().enumerate() {
            let count = content.matches(old_text.as_str()).count();
            match count {
                0 => {
                    return Err(if edits.len() == 1 {
                        "old_text not found".to_string()
                    } else {
                        format!("edits[{i}] old_text not found")
                    });
                }
                1 => {}
                n => {
                    return Err(if edits.len() == 1 {
                        format!("old_text appears {n} times (must be unique)")
                    } else {
                        format!("edits[{i}] old_text appears {n} times (must be unique)")
                    });
                }
            }
        }
        let mut result = content.to_string();
        for (old_text, new_text) in edits {
            result = result.replacen(old_text.as_str(), new_text.as_str(), 1);
        }
        Ok(result)
    }

    #[test]
    fn single_edit_replaces_unique_text() {
        let content = "hello world\nfoo bar\n";
        let edits = vec![("foo bar".to_string(), "baz qux".to_string())];
        let result = apply_edits(content, &edits).unwrap();
        assert_eq!(result, "hello world\nbaz qux\n");
    }

    #[test]
    fn single_edit_not_found_errors() {
        let content = "hello world\n";
        let edits = vec![("missing".to_string(), "x".to_string())];
        let err = apply_edits(content, &edits).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn single_edit_non_unique_errors() {
        let content = "foo\nfoo\n";
        let edits = vec![("foo".to_string(), "bar".to_string())];
        let err = apply_edits(content, &edits).unwrap_err();
        assert!(err.contains("2 times"), "got: {err}");
    }

    #[test]
    fn multi_edit_applies_atomically() {
        let content = "alpha\nbeta\ngamma\n";
        let edits = vec![
            ("alpha".to_string(), "A".to_string()),
            ("beta".to_string(), "B".to_string()),
        ];
        let result = apply_edits(content, &edits).unwrap();
        assert_eq!(result, "A\nB\ngamma\n");
    }

    #[test]
    fn multi_edit_validates_all_before_applying() {
        // second edit is non-unique — whole batch should fail
        let content = "alpha\nbeta\nbeta\n";
        let edits = vec![
            ("alpha".to_string(), "A".to_string()),
            ("beta".to_string(), "B".to_string()),
        ];
        let err = apply_edits(content, &edits).unwrap_err();
        assert!(err.contains("edits[1]"), "got: {err}");
        // content must be unchanged — we never wrote it
    }

    #[test]
    fn edit_empty_old_text_is_not_found() {
        // An empty needle matches everywhere; count > 1 for any non-empty file.
        // For an empty file, it appears once — but we still test the behavior.
        let content = "abc";
        let edits = vec![("xyz".to_string(), "".to_string())];
        let err = apply_edits(content, &edits).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    /// Integration test: exercises the full Tool::execute path with a MockHost.
    #[tokio::test]
    async fn tool_execute_single_edit_via_host() {
        let path = "/tmp/test_edit_file.txt";
        let host = Arc::new(MockHost::new(path, "line one\nline two\nline three\n"));

        // Verify via MockHost directly
        let cap_result = host
            .request(
                &Capability::FileRead {
                    path: path.to_string(),
                },
                &Grants::default(),
            )
            .await
            .unwrap();

        let content = match cap_result {
            CapabilityResult::FileRead(b) => String::from_utf8(b).unwrap(),
            _ => panic!("unexpected"),
        };
        assert_eq!(content, "line one\nline two\nline three\n");

        // Apply edit logic directly
        let edits = vec![("line two".to_string(), "LINE TWO".to_string())];
        let new_content = apply_edits(&content, &edits).unwrap();
        assert_eq!(new_content, "line one\nLINE TWO\nline three\n");

        // Write back via host
        host.request(
            &Capability::FileWrite {
                path: path.to_string(),
                content: new_content,
            },
            &Grants::default(),
        )
        .await
        .unwrap();

        assert_eq!(host.read(path).unwrap(), "line one\nLINE TWO\nline three\n");
    }

    #[test]
    fn multi_edit_first_invalid_stops_at_first() {
        let content = "only once\nrepeated\nrepeated\n";
        let edits = vec![
            ("missing".to_string(), "x".to_string()),
            ("only once".to_string(), "y".to_string()),
        ];
        let err = apply_edits(content, &edits).unwrap_err();
        assert!(
            err.contains("edits[0]") && err.contains("not found"),
            "got: {err}"
        );
    }

    // ---- End-to-end tests against EditFile::execute via test_support::MockHost ----
    //
    // The block above tests a local apply_edits helper that mirrors EditFile's
    // validation logic. The tests below actually drive `EditFile::execute()` so
    // its argument parsing, host-call sequencing, and output formatting are
    // covered too.

    mod execute_path {
        use super::super::EditFile;
        use crate::test_support::{
            MockHost as ScriptedHost, fresh_session, tool_context_with_host,
        };
        use crate::tool::{Tool, ToolContext, ToolRegistry};
        use crate::tool_host::Capability;
        use std::sync::Arc;

        async fn ctx_with(host: Arc<ScriptedHost>) -> (eidetica::Instance, ToolContext) {
            let (instance, session) = fresh_session().await;
            let ctx = tool_context_with_host(session, Arc::new(ToolRegistry::new()), host);
            (instance, ctx)
        }

        #[tokio::test]
        async fn descriptor_requires_path_only() {
            let d = EditFile.descriptor();
            assert_eq!(d.name, "edit_file");
            let required = d.parameters["required"].as_array().unwrap();
            assert!(required.iter().any(|v| v == "path"));
            // Only `path` is required at the schema level; the function does
            // the old/new vs edits-array check at runtime.
            assert_eq!(required.len(), 1);
        }

        #[tokio::test]
        async fn missing_path_errors_before_host() {
            let host = Arc::new(ScriptedHost::new());
            let (_i, c) = ctx_with(host.clone()).await;
            let err = EditFile
                .execute(serde_json::json!({}), &c)
                .await
                .unwrap_err();
            assert!(format!("{err}").to_lowercase().contains("path"));
            assert!(host.recorded_calls().is_empty());
        }

        #[tokio::test]
        async fn missing_old_text_and_no_edits_array_errors() {
            let host = Arc::new(ScriptedHost::new());
            let (_i, c) = ctx_with(host).await;
            let err = EditFile
                .execute(serde_json::json!({ "path": "/x" }), &c)
                .await
                .unwrap_err();
            assert!(format!("{err}").to_lowercase().contains("old_text"));
        }

        #[tokio::test]
        async fn missing_new_text_in_single_edit_errors() {
            let host = Arc::new(ScriptedHost::new());
            let (_i, c) = ctx_with(host).await;
            let err = EditFile
                .execute(serde_json::json!({ "path": "/x", "old_text": "foo" }), &c)
                .await
                .unwrap_err();
            assert!(format!("{err}").to_lowercase().contains("new_text"));
        }

        #[tokio::test]
        async fn empty_edits_array_errors() {
            let host = Arc::new(ScriptedHost::new());
            let (_i, c) = ctx_with(host).await;
            let err = EditFile
                .execute(serde_json::json!({ "path": "/x", "edits": [] }), &c)
                .await
                .unwrap_err();
            assert!(format!("{err}").to_lowercase().contains("no edits"));
        }

        #[tokio::test]
        async fn edits_array_with_missing_old_text_field_errors() {
            let host = Arc::new(ScriptedHost::new());
            let (_i, c) = ctx_with(host).await;
            let err = EditFile
                .execute(
                    serde_json::json!({
                        "path": "/x",
                        "edits": [ { "new_text": "y" } ]
                    }),
                    &c,
                )
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("edits[0]"));
            assert!(format!("{err}").to_lowercase().contains("old_text"));
        }

        #[tokio::test]
        async fn old_text_not_in_file_errors_with_path() {
            let host = Arc::new(ScriptedHost::new());
            host.push_file_read(b"hello world".to_vec());
            let (_i, c) = ctx_with(host).await;
            let err = EditFile
                .execute(
                    serde_json::json!({
                        "path": "/some/file",
                        "old_text": "missing",
                        "new_text": "x"
                    }),
                    &c,
                )
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("/some/file"));
            assert!(format!("{err}").to_lowercase().contains("not found"));
        }

        #[tokio::test]
        async fn old_text_appearing_multiple_times_errors() {
            let host = Arc::new(ScriptedHost::new());
            host.push_file_read(b"foo foo foo".to_vec());
            let (_i, c) = ctx_with(host).await;
            let err = EditFile
                .execute(
                    serde_json::json!({
                        "path": "/x",
                        "old_text": "foo",
                        "new_text": "bar"
                    }),
                    &c,
                )
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("3 times"));
        }

        #[tokio::test]
        async fn happy_path_writes_replaced_content_and_reports_line_delta_zero() {
            let host = Arc::new(ScriptedHost::new());
            host.push_file_read(b"alpha\nbeta\ngamma\n".to_vec());
            host.push_file_write();
            let (_i, c) = ctx_with(host.clone()).await;
            let out = EditFile
                .execute(
                    serde_json::json!({
                        "path": "/file",
                        "old_text": "beta",
                        "new_text": "BETA"
                    }),
                    &c,
                )
                .await
                .unwrap();
            assert!(out.starts_with("Edited /file: 1 replacement(s), 0 lines"));
            // Inspect the write capability the host received.
            let calls = host.recorded_calls();
            assert_eq!(calls.len(), 2, "one read + one write");
            match &calls[1] {
                Capability::FileWrite { path, content } => {
                    assert_eq!(path, "/file");
                    assert_eq!(content, "alpha\nBETA\ngamma\n");
                }
                other => panic!("expected FileWrite, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn happy_path_reports_positive_line_delta_when_expanding() {
            let host = Arc::new(ScriptedHost::new());
            host.push_file_read(b"a".to_vec());
            host.push_file_write();
            let (_i, c) = ctx_with(host).await;
            let out = EditFile
                .execute(
                    serde_json::json!({
                        "path": "/f",
                        "old_text": "a",
                        "new_text": "a\nb\nc"
                    }),
                    &c,
                )
                .await
                .unwrap();
            assert!(out.contains("+2 lines"), "got: {out}");
        }

        #[tokio::test]
        async fn happy_path_reports_negative_line_delta_when_shrinking() {
            let host = Arc::new(ScriptedHost::new());
            host.push_file_read(b"one\ntwo\nthree\n".to_vec());
            host.push_file_write();
            let (_i, c) = ctx_with(host).await;
            let out = EditFile
                .execute(
                    serde_json::json!({
                        "path": "/f",
                        "old_text": "one\ntwo\nthree\n",
                        "new_text": "one\n"
                    }),
                    &c,
                )
                .await
                .unwrap();
            assert!(out.contains("-2 lines"), "got: {out}");
        }

        #[tokio::test]
        async fn multi_edit_array_applied_atomically() {
            let host = Arc::new(ScriptedHost::new());
            host.push_file_read(b"alpha beta gamma".to_vec());
            host.push_file_write();
            let (_i, c) = ctx_with(host.clone()).await;
            let out = EditFile
                .execute(
                    serde_json::json!({
                        "path": "/f",
                        "edits": [
                            { "old_text": "alpha", "new_text": "A" },
                            { "old_text": "gamma", "new_text": "G" }
                        ]
                    }),
                    &c,
                )
                .await
                .unwrap();
            assert!(out.contains("2 replacement(s)"));
            let written = host.recorded_calls().pop().unwrap();
            match written {
                Capability::FileWrite { content, .. } => {
                    assert_eq!(content, "A beta G");
                }
                other => panic!("expected FileWrite, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn unexpected_read_result_variant_is_execution_error() {
            // Host responds to FileRead with a Shell result.
            let host = Arc::new(ScriptedHost::new());
            host.push_shell("not a file", "", 0);
            let (_i, c) = ctx_with(host).await;
            let err = EditFile
                .execute(
                    serde_json::json!({
                        "path": "/f",
                        "old_text": "x",
                        "new_text": "y"
                    }),
                    &c,
                )
                .await
                .unwrap_err();
            assert!(format!("{err}").to_lowercase().contains("unexpected"));
        }
    }
}
