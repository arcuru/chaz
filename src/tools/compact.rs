use crate::session::{EntryType, SessionEntry};
use crate::tool::{Tool, ToolContext, ToolDescriptor};
use chrono::Utc;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Compact the conversation history by writing a summary entry.
///
/// The agent provides a summary of the conversation so far. This is written
/// as a `Summary` entry to the session. Future context builds will treat the
/// most recent Summary as the start boundary, effectively compacting older
/// messages out of the context window.
pub struct Compact;

impl Tool for Compact {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "compact".to_string(),
            description: "Compact conversation history by writing a summary. Call this when the conversation is getting long to preserve context window space. Provide a thorough summary of the conversation so far — everything before this summary will be excluded from future context. Include key facts, decisions, ongoing tasks, and any state the agent needs to continue working.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "A thorough summary of the conversation so far. Include: key facts discussed, decisions made, tasks in progress, and any state needed to continue."
                    }
                },
                "required": ["summary"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let summary = arguments
                .get("summary")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'summary' argument".to_string())?;

            if summary.trim().is_empty() {
                return Err("Summary cannot be empty".into());
            }

            let entry = SessionEntry {
                sender: ctx.agent_name.clone(),
                content: summary.to_string(),
                timestamp: Utc::now(),
                entry_type: EntryType::Summary,
                metadata: None,
            };

            let mut session = ctx.session.lock().await;
            session.add_entry(entry).await;

            let entry_count = session.entries().len();
            Ok(format!(
                "Context compacted. Summary written to session. Session has {entry_count} total entries; \
                 future context builds will start from this summary."
            ))
        })
    }
}
