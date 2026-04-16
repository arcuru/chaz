//! Context window management — assembles LLM context within token budgets.
//!
//! The `ContextBuilder` replaces the previous `Session::build_context()` +
//! `context_to_messages()` pipeline. It takes session entries, agent config,
//! tool definitions, and a token budget, then produces a `Vec<RuntimeMessage>`
//! ready for the backend.
//!
//! Key behaviors:
//! - Respects `EntryType::Summary` as a context boundary (most recent Summary
//!   becomes the conversation start, older entries are excluded)
//! - Estimates token usage for system prompt, tool definitions, and messages
//! - Fills from newest messages backward until the budget is exhausted
//! - Always includes the system prompt and at least the most recent message

use crate::config::ContextConfig;
use crate::role::RoleDetails;
use crate::runtime::RuntimeMessage;
use crate::session::{EntryType, SessionEntry};
use crate::tool::ToolDefinition;

use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

/// Get the shared tokenizer instance (cl100k_base, used by GPT-4/GPT-4o).
///
/// Lazily initialized on first use. Falls back to char/4 heuristic if
/// tokenizer initialization fails (shouldn't happen with compiled-in data).
fn tokenizer() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

/// Estimate token count for a string using tiktoken (cl100k_base).
///
/// Falls back to chars/4 heuristic if the tokenizer is unavailable.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    match tokenizer() {
        Some(bpe) => bpe.encode_ordinary(text).len(),
        None => text.len().div_ceil(4),
    }
}

/// Estimate token overhead for a single tool definition (JSON schema).
fn estimate_tool_tokens(def: &ToolDefinition) -> usize {
    // Tool definitions include name, description, and JSON schema.
    // The schema gets serialized as JSON in the API request.
    let schema_str = serde_json::to_string(&def.parameters).unwrap_or_default();
    estimate_tokens(&def.name) + estimate_tokens(&def.description) + estimate_tokens(&schema_str)
        // Structural overhead: function object wrapper, type field, etc.
        + 15
}

/// Per-message framing overhead in tokens (role label, JSON structure).
const MESSAGE_OVERHEAD_TOKENS: usize = 8;

/// Assembled context ready for the runtime/backend.
pub struct AssembledContext {
    pub messages: Vec<RuntimeMessage>,
    /// Estimated total tokens used by this context (messages + system + tools).
    pub estimated_tokens: usize,
    /// Number of session entries that were included.
    pub entries_included: usize,
    /// Whether older messages were truncated to fit the budget.
    pub truncated: bool,
}

/// Builds LLM context from session entries within a token budget.
pub struct ContextBuilder<'a> {
    entries: &'a [SessionEntry],
    agent_name: &'a str,
    role: Option<&'a RoleDetails>,
    tool_defs: &'a [ToolDefinition],
    config: &'a ContextConfig,
    /// Per-agent override for max context tokens
    max_context_tokens_override: Option<usize>,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(
        entries: &'a [SessionEntry],
        agent_name: &'a str,
        config: &'a ContextConfig,
    ) -> Self {
        Self {
            entries,
            agent_name,
            role: None,
            tool_defs: &[],
            config,
            max_context_tokens_override: None,
        }
    }

    pub fn with_role(mut self, role: Option<&'a RoleDetails>) -> Self {
        self.role = role;
        self
    }

    pub fn with_tools(mut self, tool_defs: &'a [ToolDefinition]) -> Self {
        self.tool_defs = tool_defs;
        self
    }

    pub fn with_max_tokens_override(mut self, max_tokens: Option<usize>) -> Self {
        self.max_context_tokens_override = max_tokens;
        self
    }

    /// Build the context, fitting messages within the token budget.
    pub fn build(self) -> AssembledContext {
        let max_tokens = self
            .max_context_tokens_override
            .unwrap_or(self.config.max_context_tokens);
        let budget = max_tokens.saturating_sub(self.config.reserved_output_tokens);

        let mut used_tokens: usize = 0;

        // 1. System prompt (always included)
        let system_prompt = self.role.map(|r| r.get_prompt()).unwrap_or_default();
        let system_tokens = if !system_prompt.is_empty() {
            estimate_tokens(&system_prompt) + MESSAGE_OVERHEAD_TOKENS
        } else {
            0
        };
        used_tokens += system_tokens;

        // 2. Tool definitions overhead
        let tool_tokens: usize = self.tool_defs.iter().map(estimate_tool_tokens).sum();
        used_tokens += tool_tokens;

        // 3. Find context boundary: most recent Summary entry
        let boundary_idx = self
            .entries
            .iter()
            .rposition(|e| e.entry_type == EntryType::Summary);

        // 4. Filter to contextable entries from boundary onward
        let start_idx = boundary_idx.unwrap_or(0);
        let contextable: Vec<(usize, &SessionEntry)> = self.entries[start_idx..]
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e.entry_type,
                    EntryType::Message | EntryType::Directive | EntryType::Summary
                )
            })
            .map(|(i, e)| (start_idx + i, e))
            .collect();

        // 5. Calculate token cost per entry
        let entry_costs: Vec<usize> = contextable
            .iter()
            .map(|(_, e)| estimate_tokens(&e.content) + MESSAGE_OVERHEAD_TOKENS)
            .collect();

        // 6. Fill from newest backward until budget exhausted.
        //    Always include the most recent message.
        let remaining_budget = budget.saturating_sub(used_tokens);
        let mut included_from = contextable.len(); // exclusive start index (we'll decrement)
        let mut message_tokens: usize = 0;

        for i in (0..contextable.len()).rev() {
            let cost = entry_costs[i];
            if message_tokens + cost > remaining_budget && i < contextable.len() - 1 {
                // Would exceed budget and we already have at least one message
                break;
            }
            message_tokens += cost;
            included_from = i;
            // If we just included a Summary, stop — it's the boundary
            if contextable[i].1.entry_type == EntryType::Summary {
                break;
            }
        }

        let truncated = included_from > 0;
        let included_entries = &contextable[included_from..];
        used_tokens += message_tokens;

        // 7. Assemble RuntimeMessages
        let mut messages = Vec::with_capacity(included_entries.len() + 1);

        if !system_prompt.is_empty() {
            messages.push(RuntimeMessage::System(system_prompt));
        }

        for (_, entry) in included_entries {
            let rm = if entry.sender == self.agent_name {
                RuntimeMessage::Assistant(entry.content.clone())
            } else {
                // Summary, Directive, and other-sender Messages all become user messages
                RuntimeMessage::User(entry.content.clone())
            };
            messages.push(rm);
        }

        AssembledContext {
            messages,
            estimated_tokens: used_tokens,
            entries_included: included_entries.len(),
            truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_entry(sender: &str, content: &str, entry_type: EntryType) -> SessionEntry {
        SessionEntry {
            sender: sender.to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
            entry_type,
        }
    }

    fn default_config() -> ContextConfig {
        ContextConfig {
            max_context_tokens: 1000,
            reserved_output_tokens: 100,
        }
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 0);
        // With tiktoken, "hello" is 1 token
        assert_eq!(estimate_tokens("hello"), 1);
        // A long repeated string should produce a reasonable token count
        let hundred_a = estimate_tokens(&"a".repeat(100));
        assert!(hundred_a > 0 && hundred_a < 100);
    }

    #[test]
    fn test_basic_context_assembly() {
        let entries = vec![
            make_entry("user", "Hello", EntryType::Message),
            make_entry("agent", "Hi there!", EntryType::Message),
            make_entry("user", "How are you?", EntryType::Message),
        ];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", &config).build();

        assert_eq!(result.entries_included, 3);
        assert!(!result.truncated);
        // System(none) + 3 messages
        assert_eq!(result.messages.len(), 3);
        assert!(matches!(&result.messages[0], RuntimeMessage::User(s) if s == "Hello"));
        assert!(matches!(&result.messages[1], RuntimeMessage::Assistant(s) if s == "Hi there!"));
        assert!(matches!(&result.messages[2], RuntimeMessage::User(s) if s == "How are you?"));
    }

    #[test]
    fn test_summary_boundary() {
        let entries = vec![
            make_entry("user", "Old message 1", EntryType::Message),
            make_entry("agent", "Old response 1", EntryType::Message),
            make_entry(
                "system",
                "Summary of earlier conversation",
                EntryType::Summary,
            ),
            make_entry("user", "New message", EntryType::Message),
            make_entry("agent", "New response", EntryType::Message),
        ];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", &config).build();

        // Should include: Summary + 2 new messages = 3 entries
        assert_eq!(result.entries_included, 3);
        assert!(matches!(
            &result.messages[0],
            RuntimeMessage::User(s) if s == "Summary of earlier conversation"
        ));
        assert!(matches!(&result.messages[1], RuntimeMessage::User(s) if s == "New message"));
        assert!(matches!(&result.messages[2], RuntimeMessage::Assistant(s) if s == "New response"));
    }

    #[test]
    fn test_filters_non_context_entries() {
        let entries = vec![
            make_entry("user", "Hello", EntryType::Message),
            make_entry("agent", "tool call", EntryType::ToolCall),
            make_entry("agent", "tool result", EntryType::ToolResult),
            make_entry("agent", "", EntryType::Ack),
            make_entry("agent", "error", EntryType::Error),
            make_entry("agent", "Response", EntryType::Message),
        ];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", &config).build();

        assert_eq!(result.entries_included, 2); // Only Message entries
        assert_eq!(result.messages.len(), 2);
    }

    #[test]
    fn test_budget_truncation() {
        // Create many messages that exceed the budget
        let mut entries = Vec::new();
        for i in 0..100 {
            entries.push(make_entry(
                if i % 2 == 0 { "user" } else { "agent" },
                &"x".repeat(200), // ~50 tokens each + overhead ≈ 58
                EntryType::Message,
            ));
        }
        // Budget: 1000 - 100 reserved = 900 tokens
        // Each message: ~58 tokens → fits ~15 messages
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", &config).build();

        assert!(result.truncated);
        assert!(result.entries_included < 100);
        assert!(result.estimated_tokens <= 900);
        // Most recent message should always be included
        assert!(matches!(
            &result.messages.last().unwrap(),
            RuntimeMessage::Assistant(_)
        ));
    }

    #[test]
    fn test_system_prompt_counted() {
        let entries = vec![make_entry("user", "Hello", EntryType::Message)];
        // Use a long enough prompt that it takes significant tokens
        let role = crate::role::RoleDetails::new_test("system", &"word ".repeat(500));
        let config = ContextConfig {
            max_context_tokens: 600,
            reserved_output_tokens: 50,
        };
        let result = ContextBuilder::new(&entries, "agent", &config)
            .with_role(Some(&role))
            .build();

        // System prompt takes significant tokens
        assert!(result.estimated_tokens > 100);
        assert_eq!(result.messages.len(), 2); // system + at least 1 message
        assert!(matches!(&result.messages[0], RuntimeMessage::System(_)));
    }

    #[test]
    fn test_tool_overhead_reduces_budget() {
        let entries: Vec<SessionEntry> = (0..50)
            .map(|i| {
                make_entry(
                    if i % 2 == 0 { "user" } else { "agent" },
                    &"x".repeat(100),
                    EntryType::Message,
                )
            })
            .collect();
        let config = default_config();

        // Without tools
        let result_no_tools = ContextBuilder::new(&entries, "agent", &config).build();

        // With tools (takes up budget)
        let tools = vec![ToolDefinition {
            name: "big_tool".to_string(),
            description: "A tool with a very long description. ".repeat(50),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "arg1": {"type": "string", "description": "A long description ".repeat(20)},
                    "arg2": {"type": "number", "description": "Another long description ".repeat(20)}
                }
            }),
        }];
        let result_with_tools = ContextBuilder::new(&entries, "agent", &config)
            .with_tools(&tools)
            .build();

        // Fewer messages should fit when tools eat into the budget
        assert!(result_with_tools.entries_included < result_no_tools.entries_included);
    }

    #[test]
    fn test_always_includes_last_message() {
        // Even with a tiny budget, the last message must be included
        let entries = vec![make_entry(
            "user",
            &"x".repeat(10000), // ~2500 tokens
            EntryType::Message,
        )];
        let config = ContextConfig {
            max_context_tokens: 200,
            reserved_output_tokens: 50,
        };
        let result = ContextBuilder::new(&entries, "agent", &config).build();

        assert_eq!(result.entries_included, 1);
        assert_eq!(result.messages.len(), 1);
    }

    #[test]
    fn test_directive_included_as_user() {
        let entries = vec![
            make_entry("scheduler", "Do the daily check", EntryType::Directive),
            make_entry("agent", "Done", EntryType::Message),
        ];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", &config).build();

        assert_eq!(result.entries_included, 2);
        assert!(matches!(
            &result.messages[0],
            RuntimeMessage::User(s) if s == "Do the daily check"
        ));
    }

    #[test]
    fn test_empty_session() {
        let entries: Vec<SessionEntry> = vec![];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", &config).build();

        assert_eq!(result.entries_included, 0);
        assert_eq!(result.messages.len(), 0);
        assert!(!result.truncated);
    }

    #[test]
    fn test_per_agent_token_override() {
        let entries: Vec<SessionEntry> = (0..50)
            .map(|i| {
                make_entry(
                    if i % 2 == 0 { "user" } else { "agent" },
                    &"x".repeat(100),
                    EntryType::Message,
                )
            })
            .collect();
        let config = default_config();

        let result_default = ContextBuilder::new(&entries, "agent", &config).build();
        let result_small = ContextBuilder::new(&entries, "agent", &config)
            .with_max_tokens_override(Some(300))
            .build();

        assert!(result_small.entries_included < result_default.entries_included);
    }
}
