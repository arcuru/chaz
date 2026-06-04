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
use crate::extension::ExtensionHub;
use crate::runtime::RuntimeMessage;
use crate::session::{EntryType, SessionEntry};
use crate::tool::ToolDefinition;
use eidetica::Database;
use std::sync::Arc;

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

/// Build the multi-agent room note for `agent_name`, given the full
/// participant roster (which normally includes `agent_name` itself).
///
/// Returns `None` when fewer than two agents are attached or no
/// participant other than `agent_name` exists — single-agent sessions
/// get no note, so their system prompt stays byte-identical (cache-safe).
/// The roster is rendered in the given order, self excluded, deduped
/// case-insensitively. A stable roster yields a byte-identical note
/// every turn; only a membership change perturbs it (one-turn re-cache).
fn room_note(participants: &[String], agent_name: &str) -> Option<String> {
    if participants.len() < 2 {
        return None;
    }
    let mut others: Vec<&str> = Vec::new();
    for p in participants {
        if p.eq_ignore_ascii_case(agent_name) {
            continue;
        }
        if others.iter().any(|o| o.eq_ignore_ascii_case(p)) {
            continue;
        }
        others.push(p);
    }
    if others.is_empty() {
        return None;
    }
    let list = others
        .iter()
        .map(|n| format!("@{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "You are in a shared session with other agents: {list}. \
         To address a participant directly, @mention them by display name. \
         They will see your message and may reply. Messages with no @mention \
         are not routed to other agents."
    ))
}

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
    system_prompt: &'a str,
    tool_defs: &'a [ToolDefinition],
    config: &'a ContextConfig,
    /// Per-agent override for max context tokens
    max_context_tokens_override: Option<usize>,
    /// Display names of every agent attached to this session (including
    /// `agent_name` itself). When more than one agent is attached, a
    /// standard "room note" listing the *other* participants and the
    /// `@mention` convention is appended to the system prompt.
    room_participants: &'a [String],
    /// ExtensionHub for system prompt augmentation (skills, memory, etc.).
    extension_hub: Option<Arc<ExtensionHub>>,
    /// Session DB passed through to the hub for per-session provider resolution.
    session_db: Option<&'a Database>,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(
        entries: &'a [SessionEntry],
        agent_name: &'a str,
        system_prompt: &'a str,
        config: &'a ContextConfig,
    ) -> Self {
        Self {
            entries,
            agent_name,
            system_prompt,
            tool_defs: &[],
            config,
            max_context_tokens_override: None,
            room_participants: &[],
            extension_hub: None,
            session_db: None,
        }
    }

    /// Supply the session DB for per-session extension provider resolution.
    pub fn with_session_db(mut self, db: &'a Database) -> Self {
        self.session_db = Some(db);
        self
    }

    /// Supply the full roster of agents attached to the session (including
    /// this agent). With >1 participant, a room note is appended to the
    /// system prompt so agents learn the `@mention` convention without
    /// per-system-prompt editing.
    pub fn with_room_participants(mut self, participants: &'a [String]) -> Self {
        self.room_participants = participants;
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
    pub fn with_extension_hub(mut self, hub: Arc<ExtensionHub>) -> Self {
        self.extension_hub = Some(hub);
        self
    }

    /// Build the context, fitting messages within the token budget.
    pub async fn build(self) -> AssembledContext {
        let max_tokens = self
            .max_context_tokens_override
            .unwrap_or(self.config.max_context_tokens);
        let budget = max_tokens.saturating_sub(self.config.reserved_output_tokens);

        let mut used_tokens: usize = 0;

        // 1. System prompt (always included). The caller provides the
        //    agent's system_prompt directly — no snapshot lookup needed.
        let mut system_prompt = self.system_prompt.to_string();

        // Multi-agent room note. Appended so it stays current as membership
        // changes and keeps single-agent sessions byte-identical. See
        // `docs/src/design/autonomous_agents.md`.
        if let Some(note) = room_note(self.room_participants, self.agent_name) {
            if system_prompt.is_empty() {
                system_prompt = note;
            } else {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&note);
            }
        }

        // 1.5. Extensions: skills, memory, etc. inject augmentations.
        let recent_text: Vec<String> = self
            .entries
            .iter()
            .rev()
            .take(10)
            .filter(|e| matches!(e.entry_type, EntryType::Message | EntryType::Directive))
            .map(|e| e.content.clone())
            .collect();
        if let Some(ref hub) = self.extension_hub {
            let augmentation = hub
                .augment_system_prompt(self.agent_name, &recent_text, None, self.session_db)
                .await;
            if !augmentation.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&augmentation);
            }
        }

        let system_tokens = if !system_prompt.is_empty() {
            estimate_tokens(&system_prompt) + MESSAGE_OVERHEAD_TOKENS
        } else {
            0
        };
        used_tokens += system_tokens;

        // 2. Tool definitions overhead
        let tool_tokens: usize = self.tool_defs.iter().map(estimate_tool_tokens).sum();
        used_tokens += tool_tokens;

        // 2.5. Resolve context tails up front so their tokens count
        // against the budget *before* we decide how many transcript
        // messages to keep. Otherwise a large recall payload could push
        // the assembled context past the model's window.
        let tail_text = if let Some(ref hub) = self.extension_hub {
            let t = hub
                .context_tails(self.agent_name, &recent_text, None, self.session_db)
                .await;
            if t.is_empty() { None } else { Some(t) }
        } else {
            None
        };
        let tail_tokens = match &tail_text {
            Some(t) => estimate_tokens(t) + MESSAGE_OVERHEAD_TOKENS,
            None => 0,
        };
        used_tokens += tail_tokens;

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

        // 8. Context tails — memory surfacing, etc. Appended after the
        //    conversation messages. Tail text and tokens were resolved
        //    in step 2.5 so they were already deducted from the
        //    message budget.
        if let Some(t) = tail_text {
            messages.push(RuntimeMessage::User(t));
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
            metadata: None,
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

    #[tokio::test]
    async fn test_basic_context_assembly() {
        let entries = vec![
            make_entry("user", "Hello", EntryType::Message),
            make_entry("agent", "Hi there!", EntryType::Message),
            make_entry("user", "How are you?", EntryType::Message),
        ];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;

        assert_eq!(result.entries_included, 3);
        assert!(!result.truncated);
        // System(none) + 3 messages
        assert_eq!(result.messages.len(), 3);
        assert!(matches!(&result.messages[0], RuntimeMessage::User(s) if s == "Hello"));
        assert!(matches!(&result.messages[1], RuntimeMessage::Assistant(s) if s == "Hi there!"));
        assert!(matches!(&result.messages[2], RuntimeMessage::User(s) if s == "How are you?"));
    }
    #[tokio::test]
    async fn test_summary_boundary() {
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
        let result = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;

        // Should include: Summary + 2 new messages = 3 entries
        assert_eq!(result.entries_included, 3);
        assert!(matches!(
            &result.messages[0],
            RuntimeMessage::User(s) if s == "Summary of earlier conversation"
        ));
        assert!(matches!(&result.messages[1], RuntimeMessage::User(s) if s == "New message"));
        assert!(matches!(&result.messages[2], RuntimeMessage::Assistant(s) if s == "New response"));
    }

    #[tokio::test]
    async fn test_filters_non_context_entries() {
        let entries = vec![
            make_entry("user", "Hello", EntryType::Message),
            make_entry("agent", "tool call", EntryType::ToolCall),
            make_entry("agent", "tool result", EntryType::ToolResult),
            make_entry("agent", "", EntryType::Ack),
            make_entry("agent", "error", EntryType::Error),
            make_entry("agent", "Response", EntryType::Message),
        ];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;

        assert_eq!(result.entries_included, 2); // Only Message entries
        assert_eq!(result.messages.len(), 2);
    }

    #[tokio::test]
    async fn test_budget_truncation() {
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
        let result = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;

        assert!(result.truncated);
        assert!(result.entries_included < 100);
        assert!(result.estimated_tokens <= 900);
        // Most recent message should always be included
        assert!(matches!(
            &result.messages.last().unwrap(),
            RuntimeMessage::Assistant(_)
        ));
    }

    #[tokio::test]
    async fn test_system_prompt_counted() {
        let entries = vec![make_entry("user", "Hello", EntryType::Message)];
        // Use a long enough prompt that it takes significant tokens
        let prompt = "word ".repeat(500);
        let config = ContextConfig {
            max_context_tokens: 600,
            reserved_output_tokens: 50,
        };
        let result = ContextBuilder::new(&entries, "agent", &prompt, &config)
            .build()
            .await;

        // System prompt takes significant tokens
        assert!(result.estimated_tokens > 100);
        assert_eq!(result.messages.len(), 2); // system + at least 1 message
        assert!(matches!(&result.messages[0], RuntimeMessage::System(_)));
    }

    #[tokio::test]
    async fn test_tool_overhead_reduces_budget() {
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
        let result_no_tools = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;

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
            strict: false,
        }];
        let result_with_tools = ContextBuilder::new(&entries, "agent", "", &config)
            .with_tools(&tools)
            .build()
            .await;

        // Fewer messages should fit when tools eat into the budget
        assert!(result_with_tools.entries_included < result_no_tools.entries_included);
    }

    #[tokio::test]
    async fn test_always_includes_last_message() {
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
        let result = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;

        assert_eq!(result.entries_included, 1);
        assert_eq!(result.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_directive_included_as_user() {
        let entries = vec![
            make_entry("scheduler", "Do the daily check", EntryType::Directive),
            make_entry("agent", "Done", EntryType::Message),
        ];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;

        assert_eq!(result.entries_included, 2);
        assert!(matches!(
            &result.messages[0],
            RuntimeMessage::User(s) if s == "Do the daily check"
        ));
    }

    #[tokio::test]
    async fn test_empty_session() {
        let entries: Vec<SessionEntry> = vec![];
        let config = default_config();
        let result = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;

        assert_eq!(result.entries_included, 0);
        assert_eq!(result.messages.len(), 0);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn test_per_agent_token_override() {
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

        let result_default = ContextBuilder::new(&entries, "agent", "", &config)
            .build()
            .await;
        let result_small = ContextBuilder::new(&entries, "agent", "", &config)
            .with_max_tokens_override(Some(300))
            .build()
            .await;

        assert!(result_small.entries_included < result_default.entries_included);
    }

    #[test]
    fn room_note_only_for_multi_agent_and_excludes_self() {
        // Single-agent (or empty) roster → no note (cache-safe).
        assert!(room_note(&[], "alpha").is_none());
        assert!(room_note(&["alpha".to_string()], "alpha").is_none());

        // Roster of one *other* agent.
        let note = room_note(&["alpha".to_string(), "beta".to_string()], "alpha").unwrap();
        assert!(note.contains("@beta"));
        assert!(!note.contains("@alpha"), "self must be excluded: {note}");

        // Order preserved, self excluded mid-list, case-insensitive dedup.
        let roster = vec![
            "beta".to_string(),
            "Alpha".to_string(), // self, different case
            "gamma".to_string(),
            "BETA".to_string(), // dup of beta
        ];
        let note = room_note(&roster, "alpha").unwrap();
        assert!(note.contains("@beta, @gamma"), "got: {note}");
        assert!(!note.to_lowercase().contains("@alpha"));

        // Roster size ≥2 but every entry is self → no note.
        assert!(room_note(&["alpha".to_string(), "ALPHA".to_string()], "alpha").is_none());
    }

    #[tokio::test]
    async fn room_note_appended_to_system_prompt_when_multi_agent() {
        let config = ContextConfig::default();
        let entries = vec![make_entry("patrick", "hi", EntryType::Message)];
        let prompt = "You are Alpha.";

        let solo = ContextBuilder::new(&entries, "alpha", prompt, &config)
            .build()
            .await;
        let roster = vec!["alpha".to_string(), "beta".to_string()];
        let room = ContextBuilder::new(&entries, "alpha", prompt, &config)
            .with_room_participants(&roster)
            .build()
            .await;

        let sys = |c: &AssembledContext| match c.messages.first() {
            Some(RuntimeMessage::System(s)) => s.clone(),
            _ => String::new(),
        };
        assert!(sys(&solo).contains("You are Alpha."));
        assert!(
            !sys(&solo).contains("@beta"),
            "single-agent prompt must stay unchanged"
        );
        assert!(sys(&room).contains("You are Alpha."));
        assert!(sys(&room).contains("@beta"));
    }
}
