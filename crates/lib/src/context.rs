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
use crate::session::{EntryType, SessionEntry, TurnTranscriptMessage, TurnTranscriptRecord};
use crate::tool::{NO_REPLY_TOOL, ToolDefinition};
use eidetica::Database;
use std::collections::HashSet;
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
         Only @mention another agent by display name when you want their \
         input or action. A mention wakes that agent for a normal local \
         reply. Messages with no @mention do not wake other agents."
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
    tool_history: Option<&'a [Vec<TurnTranscriptRecord>]>,
    agent_name: &'a str,
    system_prompt: &'a str,
    /// Tool definitions for this turn. Also the set that `requires_tools`
    /// on a skill is matched against, so a builder left without
    /// [`Self::with_tools`] suppresses every skill that declares one.
    tool_defs: &'a [ToolDefinition],
    config: &'a ContextConfig,
    /// Per-agent override for max context tokens
    max_context_tokens_override: Option<usize>,
    /// Display names of every agent attached to this session (including
    /// `agent_name` itself). When more than one agent is attached, a
    /// standard "room note" listing the *other* participants and the
    /// `@mention` convention is appended to the system prompt.
    room_participants: &'a [String],
    attachment: Option<(&'a str, &'a str, &'a str)>,
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
            tool_history: None,
            agent_name,
            system_prompt,
            tool_defs: &[],
            config,
            max_context_tokens_override: None,
            room_participants: &[],
            attachment: None,
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

    /// An external attachment is a session property, never inferred from the
    /// text of the current input.
    pub fn with_attachment(mut self, attachment: &'a crate::session::TransportAttachment) -> Self {
        self.attachment = Some((
            &attachment.transport,
            &attachment.login_id,
            &attachment.channel,
        ));
        self
    }

    #[cfg(test)]
    pub fn with_matrix_binding(mut self, login: &'a str, room: &'a str) -> Self {
        self.attachment = Some(("matrix", login, room));
        self
    }

    pub fn with_tool_history(mut self, history: &'a [Vec<TurnTranscriptRecord>]) -> Self {
        self.tool_history = Some(history);
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

        if let Some((transport, login, room)) = self.attachment {
            if transport == "matrix" {
                system_prompt.push_str(&format!(
                "\n\nThis session is attached to Matrix room {room} on login {login}. \
                 Incoming Matrix messages may be observed without waking you. \
                 On a turn triggered by a transport_message from this Matrix room, a normal final \
                 is posted to that room under your Matrix identity and recorded locally. If you \
                 have nothing useful to say, call no_reply({{}}) as the sole terminal action; \
                 that turn will have no final or room post. Do not use empty final text to stay quiet. \
                 A final on a TUI, schedule, or local-agent turn stays local, even in this session. \
                 The explicit matrix__send tool can post proactively; if you use it to reply during \
                 a Matrix-origin turn, the later final stays local and will not post twice. \
                 JSON kind envelopes label context, not a response format. A local agent @mention \
                 wakes the mentioned agent for a normal local final."
                ));
            } else {
                system_prompt.push_str(&format!(
                    "\n\nThis session is attached to {transport} channel {room} on login {login}. \
                     External observations may supply context without waking you. \
                     A normal final responding to an external-origin turn is posted only to its source; \
                     a final from the TUI, a schedule, or a local agent stays local. \
                     Call no_reply({{}}) as the sole terminal action to end without a final or external post. \
                     JSON kind envelopes label context, not a response format."
                ));
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
            let available_tool_names: Vec<String> = self
                .tool_defs
                .iter()
                .map(|tool| tool.name.clone())
                .collect();
            let augmentation = hub
                .augment_system_prompt(
                    self.agent_name,
                    &recent_text,
                    &available_tool_names,
                    None,
                    self.session_db,
                )
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
            // Turn boundary: also refresh the extension status outputs the
            // frontend renders. Orthogonal to the prompt, so kept out of
            // the token budget below.
            if let Some(db) = self.session_db {
                hub.refresh_status_outputs(self.agent_name, db).await;
            }
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
                ) || matches!(
                    e.bridge_role(),
                    Some(
                        crate::session::BridgeEventRole::Observation
                            | crate::session::BridgeEventRole::Outbound
                    )
                )
            })
            .map(|(i, e)| (start_idx + i, e))
            .collect();

        // 5. Render trusted routing separately from the untrusted body, then
        // budget against exactly what the model will see.
        let sent: HashSet<&str> = self
            .entries
            .iter()
            .filter(|e| e.bridge_role() == Some(crate::session::BridgeEventRole::Receipt))
            .filter_map(|e| e.routing.as_ref()?.reply_to.as_deref())
            .collect();
        let rendered: Vec<String> = contextable
            .iter()
            .map(|(_, e)| render_entry(e, self.attachment, &sent))
            .collect();
        let entry_costs: Vec<usize> = rendered
            .iter()
            .map(|text| estimate_tokens(text) + MESSAGE_OVERHEAD_TOKENS)
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

        // Give visible conversation priority; replay complete native groups from
        // newest to oldest only with the remaining token budget.
        let mut replay = vec![Vec::new(); self.entries.len()];
        if let Some(history) = self.tool_history.filter(|h| h.len() == self.entries.len()) {
            let mut spare = remaining_budget.saturating_sub(message_tokens);
            for (index, _) in included_entries.iter().rev() {
                if let Some(group) = replay_turn(&history[*index], spare) {
                    let cost = group.iter().map(message_cost).sum::<usize>();
                    spare -= cost;
                    used_tokens += cost;
                    replay[*index] = group;
                }
            }
        }

        for (offset, (index, entry)) in included_entries.iter().enumerate() {
            let text = rendered[included_from + offset].clone();
            let rm = if entry.sender == self.agent_name
                && entry.bridge_role() != Some(crate::session::BridgeEventRole::Observation)
            {
                RuntimeMessage::Assistant(text)
            } else {
                RuntimeMessage::User(text)
            };
            messages.push(rm);
            messages.append(&mut replay[*index]);
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

/// JSON framing escapes content so an in-body provenance header cannot become
/// a routing field. Only the session entry's typed routing is authoritative.
fn render_entry(
    entry: &SessionEntry,
    attachment: Option<(&str, &str, &str)>,
    sent: &HashSet<&str>,
) -> String {
    use crate::session::BridgeEventRole;
    if entry.bridge_role() == Some(BridgeEventRole::Outbound) {
        let destination = entry.routing.as_ref().and_then(|r| r.destinations.first());
        return serde_json::json!({"kind": if entry.entry_type == EntryType::MatrixSend { "matrix_send" } else { "bridge_outbound" },
            "sender": entry.sender, "transport": destination.map(|d| d.transport.as_str()),
            "room": destination.map(|d| d.channel.as_str()), "login": destination.map(|d| d.login_id.as_str()),
            "delivery": if destination.and_then(|d| d.message_id.as_deref()).is_some_and(|id| sent.contains(id)) { "sent" }
                else { "pending" }, "body": entry.bridge_body()}).to_string();
    }
    if let Some(source) = entry.routing.as_ref().and_then(|r| r.source.as_ref()) {
        return serde_json::json!({"kind": if entry.bridge_role() == Some(BridgeEventRole::Observation) {
                if entry.entry_type == EntryType::MatrixObserved { "matrix_observed" } else { "bridge_observation" }
            } else { "transport_message" },
            "transport": source.transport, "login": source.login_id, "room": source.channel,
            "sender": source.sender, "body": if entry.bridge_role() == Some(BridgeEventRole::Observation) { entry.bridge_body().unwrap_or_default() } else { entry.content.clone() }}).to_string();
    }
    if entry.entry_type == EntryType::Message {
        if let Some(id) = entry
            .routing
            .as_ref()
            .and_then(|routing| routing.outbound_id.as_deref())
        {
            return serde_json::json!({"kind": if attachment.is_some_and(|a| a.0 == "matrix") { "matrix_reply" } else { "bridge_reply" },
                "sender": entry.sender, "delivery": if sent.contains(id) { "sent" } else { "pending" },
                "body": entry.content}).to_string();
        }
        if attachment.is_some() {
            return serde_json::json!({"kind": "local_message", "sender": entry.sender, "body": entry.content}).to_string();
        }
    }
    entry.content.clone()
}

// Reject interrupted or malformed groups instead of inventing tool results.
fn replay_turn(records: &[TurnTranscriptRecord], budget: usize) -> Option<Vec<RuntimeMessage>> {
    if records.is_empty() || budget == 0 {
        return None;
    }
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|r| r.sequence);
    let mut messages = Vec::new();
    let mut previous = None;
    let mut model_sequence = None;
    let mut terminal = false;
    let mut i = 0;
    while i < sorted.len() {
        let record = &sorted[i];
        if record.sequence != previous.map_or(0, |p| p + 1)
            || record.attempt_id != sorted[0].attempt_id
            || record.request_id != sorted[0].request_id
        {
            return None;
        }
        previous = Some(record.sequence);
        let TurnTranscriptMessage::ModelResponse {
            model_sequence: seq,
            content,
            tool_calls,
            provider_extra,
            terminal: end,
            ..
        } = &record.message
        else {
            return None;
        };
        if *seq != model_sequence.map_or(0, |p| p + 1) {
            return None;
        }
        model_sequence = Some(*seq);
        i += 1;
        if *end {
            // Terminal no_reply has no tool result and is not replayed.
            // Completed tool exchanges earlier in the turn still belong in context.
            let silent = tool_calls.len() == 1 && tool_calls[0].name == NO_REPLY_TOOL;
            if (!tool_calls.is_empty() && !silent) || i != sorted.len() {
                return None;
            }
            terminal = true;
            break;
        }
        if tool_calls.is_empty() {
            return None;
        }
        messages.push(RuntimeMessage::AssistantToolCalls {
            content: content.clone(),
            tool_calls: tool_calls.clone(),
            provider_extra: provider_extra.clone(),
        });
        for (call_index, call) in tool_calls.iter().enumerate() {
            let result = sorted.get(i)?;
            let TurnTranscriptMessage::ToolResult {
                model_sequence,
                call_index: index,
                call_id,
                name,
                output,
                ..
            } = &result.message
            else {
                return None;
            };
            if result.sequence != previous? + 1
                || result.attempt_id != record.attempt_id
                || result.request_id != record.request_id
                || *model_sequence != *seq
                || *index != call_index
                || call_id != &call.id
                || name != &call.name
            {
                return None;
            }
            previous = Some(result.sequence);
            messages.push(RuntimeMessage::ToolResult {
                call_id: call.id.clone(),
                content: crate::runtime::wrap_tool_output(
                    &call.name,
                    &crate::runtime::bound_tool_output(output),
                ),
            });
            i += 1;
        }
    }
    if !terminal || messages.is_empty() {
        return None;
    }
    if messages.iter().map(message_cost).sum::<usize>() > budget {
        for msg in &mut messages {
            if let RuntimeMessage::ToolResult { content, .. } = msg {
                let start = content.find('\n')? + 1;
                let output = content[start..].strip_suffix("\n</tool_output>")?;
                let preview = crate::util::truncate_chars(output, 256);
                *content = format!(
                    "{}{}\n[prior tool output truncated for context]\n</tool_output>",
                    &content[..start],
                    preview
                );
            }
        }
    }
    (messages.iter().map(message_cost).sum::<usize>() <= budget).then_some(messages)
}

fn message_cost(msg: &RuntimeMessage) -> usize {
    let text = match msg {
        RuntimeMessage::AssistantToolCalls {
            content,
            tool_calls,
            provider_extra,
        } => {
            format!(
                "{}{}{}",
                content.as_deref().unwrap_or_default(),
                serde_json::to_string(tool_calls).unwrap_or_default(),
                serde_json::Value::Object(provider_extra.clone())
            )
        }
        RuntimeMessage::ToolResult { call_id, content } => format!("{call_id}{content}"),
        _ => return 0,
    };
    estimate_tokens(&text) + MESSAGE_OVERHEAD_TOKENS
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
            routing: None,
        }
    }

    fn default_config() -> ContextConfig {
        ContextConfig {
            max_context_tokens: 1000,
            reserved_output_tokens: 100,
        }
    }

    fn records(output: &str) -> Vec<TurnTranscriptRecord> {
        let request_id = crate::session::TurnRequestId::parse("request");
        let mk = |sequence, message| TurnTranscriptRecord {
            request_id: request_id.clone(),
            attempt_id: "complete".into(),
            sequence,
            timestamp: Utc::now(),
            message,
        };
        vec![
            mk(
                0,
                TurnTranscriptMessage::ModelResponse {
                    model_sequence: 0,
                    content: None,
                    tool_calls: vec![crate::runtime::ToolCallRequest {
                        id: "id".into(),
                        name: "search".into(),
                        arguments: "{}".into(),
                    }],
                    provider_extra: Default::default(),
                    metadata: None,
                    terminal: false,
                },
            ),
            mk(
                1,
                TurnTranscriptMessage::ToolResult {
                    model_sequence: 0,
                    call_index: 0,
                    call_id: "id".into(),
                    name: "search".into(),
                    output: output.into(),
                    outcome: crate::runtime::ToolResultOutcome::Success,
                },
            ),
            mk(
                2,
                TurnTranscriptMessage::ModelResponse {
                    model_sequence: 1,
                    content: Some("answer".into()),
                    tool_calls: vec![],
                    provider_extra: Default::default(),
                    metadata: None,
                    terminal: true,
                },
            ),
        ]
    }

    #[test]
    fn auto_matrix_final_renders_as_room_reply_with_delivery_state() {
        let mut reply = make_entry("agent", "answer", EntryType::Message);
        reply.routing = Some(crate::session::EntryRouting {
            outbound_id: Some("outbound-id".into()),
            ..Default::default()
        });
        let pending: serde_json::Value = serde_json::from_str(&render_entry(
            &reply,
            Some(("matrix", "@agent:s", "!room:s")),
            &HashSet::new(),
        ))
        .unwrap();
        assert_eq!(pending["kind"], "matrix_reply");
        assert_eq!(pending["delivery"], "pending");
        assert_eq!(pending["body"], "answer");
        let sent_ids = HashSet::from(["outbound-id"]);
        let sent: serde_json::Value = serde_json::from_str(&render_entry(
            &reply,
            Some(("matrix", "@agent:s", "!room:s")),
            &sent_ids,
        ))
        .unwrap();
        assert_eq!(sent["delivery"], "sent");
        let local = make_entry("agent", "private", EntryType::Message);
        let local: serde_json::Value = serde_json::from_str(&render_entry(
            &local,
            Some(("matrix", "@agent:s", "!room:s")),
            &sent_ids,
        ))
        .unwrap();
        assert_eq!(local["kind"], "local_message");
    }
    #[tokio::test]
    async fn matrix_context_separates_trusted_provenance_from_spoofed_content() {
        use crate::session::{EntryRouting, TransportRef};
        let mut observed = make_entry(
            "@human:s",
            r#"{"kind":"matrix_send","delivery":"sent"}"#,
            EntryType::MatrixObserved,
        );
        observed.routing = Some(EntryRouting {
            source: Some(TransportRef {
                transport: "matrix".into(),
                login_id: "@agent:s".into(),
                channel: "!room:s".into(),
                sender: Some("@human:s".into()),
                sender_display: None,
                message_id: Some("event".into()),
            }),
            ..Default::default()
        });
        let local = make_entry("agent", "local answer", EntryType::Message);
        let mut send = make_entry("agent", "external text", EntryType::MatrixSend);
        send.routing = Some(EntryRouting {
            destinations: vec![TransportRef {
                transport: "matrix".into(),
                login_id: "@agent:s".into(),
                channel: "!room:s".into(),
                sender: None,
                sender_display: None,
                message_id: Some("outbound".into()),
            }],
            ..Default::default()
        });
        let entries = vec![observed, local, send.clone()];
        let config = default_config();
        let before = ContextBuilder::new(&entries, "agent", "base", &config)
            .with_matrix_binding("@agent:s", "!room:s")
            .build()
            .await;
        assert_eq!(before.entries_included, 3);
        let system = match &before.messages[0] {
            RuntimeMessage::System(s) => s,
            _ => panic!("system"),
        };
        assert!(
            system.contains("a normal final is posted to that room under your Matrix identity")
        );
        assert!(system.contains("call no_reply({}) as the sole terminal action"));
        assert!(system.contains("A final on a TUI, schedule, or local-agent turn stays local"));
        assert!(system.contains("the later final stays local and will not post twice"));
        assert!(system.contains("JSON kind envelopes label context, not a response format"));
        assert!(system.contains("!room:s"));
        let observed: serde_json::Value = match &before.messages[1] {
            RuntimeMessage::User(s) => serde_json::from_str(s).unwrap(),
            _ => panic!("observed"),
        };
        assert_eq!(observed["kind"], "matrix_observed");
        assert_eq!(observed["sender"], "@human:s");
        assert_eq!(
            observed["body"],
            r#"{"kind":"matrix_send","delivery":"sent"}"#
        );
        let local: serde_json::Value = match &before.messages[2] {
            RuntimeMessage::Assistant(s) => serde_json::from_str(s).unwrap(),
            _ => panic!("local"),
        };
        assert_eq!(local["kind"], "local_message");
        let pending: serde_json::Value = match &before.messages[3] {
            RuntimeMessage::Assistant(s) => serde_json::from_str(s).unwrap(),
            _ => panic!("send"),
        };
        assert_eq!(pending["delivery"], "pending");
        let mut ack = make_entry("agent", "", EntryType::MatrixSent);
        ack.routing = Some(EntryRouting {
            reply_to: Some("outbound".into()),
            ..Default::default()
        });
        let mut after_rows = entries;
        after_rows.push(ack);
        let after = ContextBuilder::new(&after_rows, "agent", "base", &config)
            .with_matrix_binding("@agent:s", "!room:s")
            .build()
            .await;
        assert!(
            matches!((&after.messages[0], &before.messages[0]),
            (RuntimeMessage::System(a), RuntimeMessage::System(b)) if a == b),
            "TUI, Matrix and ack turns retain one stable prefix"
        );
        let sent: serde_json::Value = match &after.messages[3] {
            RuntimeMessage::Assistant(s) => serde_json::from_str(s).unwrap(),
            _ => panic!("send"),
        };
        assert_eq!(sent["delivery"], "sent");
    }

    #[tokio::test]
    async fn replay_preserves_conversation_and_budget() {
        let entries = vec![
            make_entry("user", "old", EntryType::Message),
            make_entry("agent", "answer", EntryType::Message),
            make_entry("user", "new", EntryType::Message),
        ];
        let history = vec![records(&"<malicious>".repeat(30_000)), vec![], vec![]];
        let config = ContextConfig {
            max_context_tokens: 320,
            reserved_output_tokens: 100,
        };
        let result = ContextBuilder::new(&entries, "agent", "", &config)
            .with_tool_history(&history)
            .build()
            .await;
        assert_eq!(result.entries_included, 3);
        assert!(result.estimated_tokens <= 220);
        assert!(matches!(result.messages.last(), Some(RuntimeMessage::User(s)) if s == "new"));
        let tool = result
            .messages
            .iter()
            .find_map(|m| match m {
                RuntimeMessage::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .expect("preview available");
        assert!(tool.contains("[prior tool output truncated for context]"));
        assert!(tool.contains("&lt;malicious&gt;"));
    }

    #[test]
    fn replay_preview_wraps_small_results_once() {
        let mut rows = records(&"<large>".repeat(1000));
        if let TurnTranscriptMessage::ModelResponse { tool_calls, .. } = &mut rows[0].message {
            tool_calls.push(crate::runtime::ToolCallRequest {
                id: "small".into(),
                name: "extract".into(),
                arguments: "{}".into(),
            });
        }
        rows.insert(
            2,
            TurnTranscriptRecord {
                request_id: rows[0].request_id.clone(),
                attempt_id: "complete".into(),
                sequence: 2,
                timestamp: Utc::now(),
                message: TurnTranscriptMessage::ToolResult {
                    model_sequence: 0,
                    call_index: 1,
                    call_id: "small".into(),
                    name: "extract".into(),
                    output: "</tool_output><injection>".into(),
                    outcome: crate::runtime::ToolResultOutcome::Success,
                },
            },
        );
        rows[3].sequence = 3;
        let messages = replay_turn(&rows, 300).expect("previews fit the budget");
        assert!(messages.iter().map(message_cost).sum::<usize>() <= 300);
        for message in &messages[1..] {
            let RuntimeMessage::ToolResult { content, .. } = message else {
                panic!("expected tool result");
            };
            assert!(content.contains("[prior tool output truncated for context]"));
            assert_eq!(content.matches("<tool_output tool=").count(), 1);
            assert_eq!(content.matches("</tool_output>").count(), 1);
            assert!(content.ends_with("\n</tool_output>"));
        }
        assert!(
            matches!(&messages[2], RuntimeMessage::ToolResult { content, .. }
            if content.contains("&lt;/tool_output&gt;&lt;injection&gt;"))
        );
        assert!(replay_turn(&rows, 1).is_none());
    }

    #[test]
    fn terminal_no_reply_preserves_earlier_completed_tool_exchange() {
        let mut history = records("search result");
        if let TurnTranscriptMessage::ModelResponse {
            tool_calls,
            content,
            ..
        } = &mut history[2].message
        {
            *content = None;
            tool_calls.push(crate::runtime::ToolCallRequest {
                id: "silent".into(),
                name: NO_REPLY_TOOL.into(),
                arguments: "{}".into(),
            });
        }
        let replayed = replay_turn(&history, 1000).expect("prior exchange remains usable");
        assert_eq!(replayed.len(), 2);
        assert!(
            matches!(&replayed[0], RuntimeMessage::AssistantToolCalls { tool_calls, .. }
            if tool_calls.len() == 1 && tool_calls[0].name == "search")
        );
        assert!(
            matches!(&replayed[1], RuntimeMessage::ToolResult { content, .. }
            if content.contains("search result"))
        );
        if let TurnTranscriptMessage::ModelResponse { tool_calls, .. } = &mut history[2].message {
            tool_calls[0].name = "ordinary_tool".into();
        }
        assert!(
            replay_turn(&history, 1000).is_none(),
            "other terminal calls are incomplete"
        );
    }

    #[test]
    fn malformed_or_incomplete_turns_never_replay() {
        let valid = records("ok");
        assert_eq!(replay_turn(&valid, 1000).unwrap().len(), 2);
        let mut missing = valid.clone();
        missing.remove(1);
        assert!(replay_turn(&missing, 1000).is_none());
        let mut wrong = valid.clone();
        if let TurnTranscriptMessage::ToolResult { call_id, .. } = &mut wrong[1].message {
            *call_id = "wrong".into();
        }
        assert!(replay_turn(&wrong, 1000).is_none());
        let mut retry = valid.clone();
        retry[1].attempt_id = "interrupted".into();
        assert!(replay_turn(&retry, 1000).is_none());
        assert!(replay_turn(&valid[..2], 1000).is_none());
        assert!(replay_turn(&[], 1000).is_none());
    }

    #[test]
    fn missing_first_tool_exchange_never_replays() {
        let mut complete = records("first");
        complete.pop();
        let mut suffix = records("second");
        for record in &mut suffix {
            record.sequence += 2;
            match &mut record.message {
                TurnTranscriptMessage::ModelResponse { model_sequence, .. }
                | TurnTranscriptMessage::ToolResult { model_sequence, .. } => *model_sequence += 1,
            }
        }
        complete.extend(suffix);
        assert_eq!(replay_turn(&complete, 1000).unwrap().len(), 4);
        // The remaining exchange is paired and terminal, but its front is missing.
        assert!(replay_turn(&complete[2..], 1000).is_none());
    }

    #[test]
    fn replay_requires_each_front_anchor() {
        for (sequence_offset, model_offset) in [(2, 0), (0, 1)] {
            let mut partial = records("ok");
            for record in &mut partial {
                record.sequence += sequence_offset;
                match &mut record.message {
                    TurnTranscriptMessage::ModelResponse { model_sequence, .. }
                    | TurnTranscriptMessage::ToolResult { model_sequence, .. } => {
                        *model_sequence += model_offset;
                    }
                }
            }
            assert!(
                replay_turn(&partial, 1000).is_none(),
                "replayed with sequence offset {sequence_offset} and model offset {model_offset}"
            );
        }
    }

    #[tokio::test]
    async fn summary_excludes_covered_tool_history() {
        let entries = vec![
            make_entry("user", "old", EntryType::Message),
            make_entry("system", "summary", EntryType::Summary),
            make_entry("user", "new", EntryType::Message),
        ];
        let history = vec![records("old result"), vec![], vec![]];
        let config = default_config();
        let context = ContextBuilder::new(&entries, "agent", "", &config)
            .with_tool_history(&history)
            .build()
            .await;
        assert_eq!(context.entries_included, 2);
        assert_eq!(context.messages.len(), 2);
        assert!(matches!(&context.messages[0], RuntimeMessage::User(s) if s == "summary"));
    }

    #[test]
    fn parallel_results_keep_call_order_and_ids() {
        let mut rows = records("one");
        if let TurnTranscriptMessage::ModelResponse { tool_calls, .. } = &mut rows[0].message {
            tool_calls.push(crate::runtime::ToolCallRequest {
                id: "second".into(),
                name: "extract".into(),
                arguments: "{}".into(),
            });
        }
        rows.insert(
            2,
            TurnTranscriptRecord {
                request_id: rows[0].request_id.clone(),
                attempt_id: "complete".into(),
                sequence: 2,
                timestamp: Utc::now(),
                message: TurnTranscriptMessage::ToolResult {
                    model_sequence: 0,
                    call_index: 1,
                    call_id: "second".into(),
                    name: "extract".into(),
                    output: "two".into(),
                    outcome: crate::runtime::ToolResultOutcome::Success,
                },
            },
        );
        rows[3].sequence = 3;
        let messages = replay_turn(&rows, 1000).unwrap();
        assert_eq!(messages.len(), 3);
        assert!(
            matches!(&messages[0], RuntimeMessage::AssistantToolCalls { tool_calls, .. }
            if tool_calls.len() == 2)
        );
        assert!(
            matches!(&messages[1], RuntimeMessage::ToolResult { call_id, .. } if call_id == "id")
        );
        assert!(
            matches!(&messages[2], RuntimeMessage::ToolResult { call_id, .. } if call_id == "second")
        );
        rows[2].sequence = 4;
        assert!(replay_turn(&rows, 1000).is_none());
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
