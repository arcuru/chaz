//! Session, Matrix-channel, scheduler, and LLM-config handlers.
//!
//! These are the "operate on the current session" commands: list, create,
//! switch, info, name, share/sync, compact, print, channel listing,
//! scheduler pointers, and per-session LLM config (model/role/backend).

use crate::backends::{ChatContext, Message, MessageRole};
use crate::session::{EntryType, Session, SessionEntry};
use crate::types::ConversationId;

use eidetica::store::Table;

use super::{CommandContext, CommandOutcome, SessionInfo, SessionSwitch};

// -----------------------------------------------------------------------------
// Session CRUD
// -----------------------------------------------------------------------------

pub(super) async fn list_sessions(ctx: &CommandContext<'_>) -> CommandOutcome {
    let indices = match ctx.server.registry().list_sessions().await {
        Ok(b) => b,
        Err(e) => return CommandOutcome::Error(format!("Failed to list sessions: {e}")),
    };

    let mut sessions = Vec::new();
    for index in indices {
        let (entry_count, last_message, meta_name, meta_agent, cost_total, cost_reported, calls) =
            match ctx
                .server
                .registry()
                .open_session(&index.session_db_id)
                .await
            {
                Ok((conv_id, db)) => {
                    let session = Session::new(conv_id, db).await;
                    let meta = session.read_meta().await;
                    let entries = session.entries();
                    let count = entries.len();
                    let last = crate::session::summarize_last_message(entries);
                    let (cost_total, cost_reported, calls) =
                        crate::session::sum_session_cost(entries);
                    (
                        count,
                        last,
                        meta.name,
                        meta.agent_name,
                        cost_total,
                        cost_reported,
                        calls,
                    )
                }
                Err(_) => (0, None, None, None, 0.0, false, 0),
            };
        sessions.push(SessionInfo {
            session_db_id: index.session_db_id,
            agent_name: meta_agent,
            name: meta_name,
            entry_count,
            last_message,
            gateway: index.gateway,
            created_at: index.created_at,
            status: index.status,
            total_cost_usd: cost_total,
            cost_reported,
            llm_call_count: calls,
        });
    }

    // Most-recently created first, with legacy (created_at = None) sessions
    // sorted to the end so fresh sessions are always near the top.
    sessions.sort_by(|a, b| match (a.created_at, b.created_at) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.session_db_id.cmp(&b.session_db_id),
    });

    CommandOutcome::SessionsList(sessions)
}

pub(super) async fn new_session(ctx: &CommandContext<'_>) -> CommandOutcome {
    let (conv_id, db) = match ctx.server.registry().create_session(Some("tui")).await {
        Ok(r) => r,
        Err(e) => return CommandOutcome::Error(format!("Failed to create session: {e}")),
    };
    let session_db_id = db.root_id().to_string();
    let agent = ctx
        .server
        .registry()
        .resolve_agent(&session_db_id, None, ctx.server.agent_index())
        .await;
    CommandOutcome::SessionSwitched(Box::new(SessionSwitch {
        session_db_id,
        conv_id,
        db,
        agent_name: agent.name,
        session_name: None,
    }))
}

pub(super) async fn switch_session(identifier: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let (conv_id, db) = match ctx.server.registry().resolve_session(identifier).await {
        Ok(r) => r,
        Err(e) => return CommandOutcome::Error(format!("Failed to switch session: {e}")),
    };

    let session_db_id = db.root_id().to_string();
    let meta = crate::session::read_meta_from_db(&db).await;

    let agent = ctx
        .server
        .registry()
        .resolve_agent(&session_db_id, None, ctx.server.agent_index())
        .await;

    CommandOutcome::SessionSwitched(Box::new(SessionSwitch {
        session_db_id,
        conv_id,
        db,
        agent_name: agent.name,
        session_name: meta.name,
    }))
}

pub(super) async fn info(ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let entries = session.entries();
    let msg_count = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Message)
        .count();
    let tool_count = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::ToolCall)
        .count();
    let directive_count = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Directive)
        .count();
    let error_count = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Error)
        .count();
    let name_line = match ctx.session_name {
        Some(n) => format!("\nName: {n}"),
        None => String::new(),
    };
    let channels = ctx
        .server
        .registry()
        .matrix_channels_for_session(ctx.session_db_id)
        .await
        .unwrap_or_default();
    let channels_line = if channels.is_empty() {
        String::new()
    } else {
        format!("\nMatrix rooms: {}", channels.join(", "))
    };
    let usage_line = format_usage_summary(entries);
    CommandOutcome::Text(format!(
        "Session: {}{name_line}\nAgent: {}{channels_line}\nTotal entries: {}\nMessages: {msg_count} | Directives: {directive_count} | Tool calls: {tool_count} | Errors: {error_count}{usage_line}",
        ctx.session_db_id,
        ctx.current_agent,
        entries.len(),
    ))
}

/// Roll up `ResponseMetadata` across every entry in the session and render
/// it as one or two extra lines for `/info`. Returns the empty string when
/// no entries carry metadata (legacy sessions or sessions whose backend
/// didn't surface usage), so the output stays clean for those cases.
fn format_usage_summary(entries: &[crate::session::SessionEntry]) -> String {
    let mut calls = 0u32;
    let mut prompt = 0u64;
    let mut completion = 0u64;
    let mut cached = 0u64;
    let mut cost: f64 = 0.0;
    let mut saw_cost = false;
    let mut models: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for entry in entries {
        let Some(m) = &entry.metadata else { continue };
        calls += 1;
        prompt += m.usage.prompt_tokens as u64;
        completion += m.usage.completion_tokens as u64;
        cached += m.usage.cached_tokens.unwrap_or(0) as u64;
        if let Some(c) = m.usage.cost_usd {
            cost += c;
            saw_cost = true;
        }
        if !m.model.is_empty() {
            *models.entry(m.model.clone()).or_insert(0) += 1;
        }
    }
    if calls == 0 {
        return String::new();
    }
    let cached_part = if cached > 0 {
        format!(" ({cached} cached)")
    } else {
        String::new()
    };
    let cost_part = if saw_cost {
        format!(" | ${cost:.4}")
    } else {
        String::new()
    };
    let mut out = format!(
        "\nLLM usage: {calls} call{} | {prompt} prompt + {completion} completion{cached_part}{cost_part}",
        if calls == 1 { "" } else { "s" }
    );
    if !models.is_empty() {
        let mut pairs: Vec<(String, u32)> = models.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let rendered: Vec<String> = pairs
            .into_iter()
            .map(|(name, n)| format!("{name} ({n})"))
            .collect();
        out.push_str(&format!("\nModels: {}", rendered.join(", ")));
    }
    out
}

pub(super) async fn list_costs(ctx: &CommandContext<'_>) -> CommandOutcome {
    let registry = ctx.server.registry();
    match crate::session::usage::collect_usage(registry, &Default::default()).await {
        Ok(rollup) => CommandOutcome::Text(crate::session::usage::render_text(&rollup)),
        Err(e) => CommandOutcome::Error(format!("Failed to collect usage: {e}")),
    }
}

pub(super) async fn name_session(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    if name.is_empty() {
        return CommandOutcome::Error("Usage: name <alias>".to_string());
    }
    match ctx
        .server
        .registry()
        .set_session_name(ctx.session_db_id, name.to_string())
        .await
    {
        Ok(()) => CommandOutcome::Text(format!("Session named '{name}'. Use it with join {name}.")),
        Err(e) => CommandOutcome::Error(format!("Failed to name session: {e}")),
    }
}

pub(super) async fn clear_session_name(ctx: &CommandContext<'_>) -> CommandOutcome {
    match ctx
        .server
        .registry()
        .clear_session_name(ctx.session_db_id)
        .await
    {
        Ok(()) => CommandOutcome::Text("Session name cleared.".to_string()),
        Err(e) => CommandOutcome::Error(format!("Failed to clear name: {e}")),
    }
}

pub(super) async fn share(ctx: &CommandContext<'_>) -> CommandOutcome {
    let instance = ctx.server.registry().instance();
    if instance.sync().is_none() {
        return CommandOutcome::Error("Sync not enabled".to_string());
    }
    let db_id = ctx.session_db.root_id().clone();
    let ticket = match ctx.server.registry().share_for(&db_id).await {
        Ok(t) => t,
        Err(e) => return CommandOutcome::Error(format!("Failed to share session: {e}")),
    };
    CommandOutcome::Text(format!(
        "Share this ticket to sync the current session:\n\n{ticket}"
    ))
}

/// Disable sync on the current session so this peer stops serving it.
pub(super) async fn unshare(ctx: &CommandContext<'_>) -> CommandOutcome {
    let db_id = ctx.session_db.root_id().clone();
    match ctx.server.registry().disable_sync_for(&db_id).await {
        Ok(()) => CommandOutcome::Text(
            "Sync disabled for this session — it is no longer shared.".to_string(),
        ),
        Err(e) => CommandOutcome::Error(format!("Failed to disable sync: {e}")),
    }
}

pub(super) async fn sync_ticket(ticket_str: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let ticket: eidetica::sync::DatabaseTicket = match ticket_str.parse() {
        Ok(t) => t,
        Err(e) => return CommandOutcome::Error(format!("Invalid ticket: {e}")),
    };
    let db_id = ticket.database_id().clone();
    // Sessions don't have a Read mode today (no read-only spectator UX), so
    // /sync always requests Write. If the requester's pubkey is preseeded
    // the sync proceeds; otherwise eidetica queues a bootstrap request.
    match ctx
        .server
        .registry()
        .request_db_access(&ticket, eidetica::auth::types::Permission::Write(10))
        .await
    {
        Ok(crate::session::BootstrapOutcome::Approved) => {}
        Ok(crate::session::BootstrapOutcome::Pending {
            request_id,
            message: _,
        }) => {
            return CommandOutcome::Text(format!(
                "Bootstrap request {request_id} pending the owner's approval. \
                 Re-run `/sync <ticket>` after they run `/sharing approve {request_id}`."
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Bootstrap failed: {e}")),
    }
    if let Err(e) = ctx.server.registry().enable_sync_for(&db_id).await {
        return CommandOutcome::Error(format!(
            "Synced {db_id} but failed to enable ongoing sync: {e}"
        ));
    }
    CommandOutcome::Text(format!("Synced database {db_id}. Use sessions to find it."))
}

pub(super) async fn compact(ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let entries: Vec<&SessionEntry> = session
        .entries()
        .iter()
        .filter(|e| {
            matches!(
                e.entry_type,
                EntryType::Message | EntryType::Directive | EntryType::Summary
            )
        })
        .collect();
    if entries.len() < 3 {
        return CommandOutcome::Error(
            "Not enough messages to compact (need at least 3)".to_string(),
        );
    }

    let mut transcript = String::new();
    for entry in &entries {
        let role_label = if entry.sender == ctx.current_agent {
            "assistant"
        } else {
            &entry.sender
        };
        let type_label = match entry.entry_type {
            EntryType::Summary => " [previous summary]",
            EntryType::Directive => " [directive]",
            _ => "",
        };
        transcript.push_str(&format!("{role_label}{type_label}: {}\n\n", entry.content));
    }

    let system_prompt = "You are a conversation summarizer. Produce a thorough, structured summary of the conversation below. Include: key topics discussed, decisions made, tasks completed or in progress, important facts and state, and any open questions. The summary replaces older messages in the context window, so it must be complete enough for the assistant to continue working without the original messages.".to_string();

    let chat_ctx = ChatContext {
        messages: vec![
            Message::new(MessageRole::System, system_prompt),
            Message::new(
                MessageRole::User,
                format!(
                    "Summarize this conversation:\n\n{transcript}\n\n\
                     Produce a structured summary that captures everything needed to continue the conversation."
                ),
            ),
        ],
        model: None,
        system_prompt: None,
    };

    let summary = match ctx.backend.execute(&chat_ctx).await {
        Ok(s) => s,
        Err(e) => return CommandOutcome::Error(format!("LLM summarization failed: {e}")),
    };

    let entry = SessionEntry {
        sender: "system".to_string(),
        content: summary.clone(),
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Summary,
        metadata: None,
    };

    let write = async {
        let txn = ctx.session_db.new_transaction().await?;
        let store = txn.get_store::<Table<SessionEntry>>("entries").await?;
        store.insert(entry).await?;
        txn.commit().await?;
        Ok::<_, anyhow::Error>(())
    };
    if let Err(e) = write.await {
        return CommandOutcome::Error(format!("Failed to write summary: {e}"));
    }

    CommandOutcome::Text(format!(
        "Session compacted. Summary ({} chars) written.",
        summary.len()
    ))
}

pub(super) async fn print_transcript(ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let mut buf = String::new();
    for entry in session.entries() {
        let label: &str = if entry.sender == ctx.current_agent {
            "assistant"
        } else {
            entry.sender.as_str()
        };
        let type_label = match entry.entry_type {
            EntryType::Directive => " [directive]",
            EntryType::Summary => " [summary]",
            EntryType::Error => " [error]",
            _ => "",
        };
        if matches!(
            entry.entry_type,
            EntryType::Message | EntryType::Directive | EntryType::Summary | EntryType::Error
        ) {
            buf.push_str(&format!("{label}{type_label}: {}\n", entry.content));
        }
    }
    if buf.is_empty() {
        CommandOutcome::Text("(empty)".to_string())
    } else {
        CommandOutcome::Text(buf)
    }
}

// -----------------------------------------------------------------------------
// Matrix channels (listing is transport-neutral; attach/detach are per-gateway)
// -----------------------------------------------------------------------------

pub(super) async fn list_channels(ctx: &CommandContext<'_>) -> CommandOutcome {
    match ctx
        .server
        .registry()
        .matrix_channels_for_session(ctx.session_db_id)
        .await
    {
        Ok(channels) if channels.is_empty() => {
            CommandOutcome::Text("No Matrix rooms attached to this session.".to_string())
        }
        Ok(channels) => CommandOutcome::Text(format!(
            "Matrix rooms attached to this session:\n  {}",
            channels.join("\n  ")
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to list channels: {e}")),
    }
}

// -----------------------------------------------------------------------------
// LLM config (per-session)
// -----------------------------------------------------------------------------

pub(super) async fn model(arg: Option<String>, ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    match arg {
        None => {
            let meta = session.read_meta().await;
            let current = meta
                .model
                .or_else(|| ctx.backend.default_model())
                .unwrap_or_else(|| "unknown".to_string());
            let mut msg = format!(
                "Current Model: {current}\n\nKnown Backends:\n{}",
                ctx.backend.list_known_backends().join("\n")
            );
            msg.push_str("\n\nKnown Models:\n");
            msg.push_str(&ctx.backend.list_known_models().join("\n"));
            CommandOutcome::Text(msg)
        }
        Some(m) => {
            let note = if ctx.backend.is_known_model(&m) {
                format!("Model set to \"{m}\"")
            } else {
                match ctx.backend.validate_model(&m) {
                    Ok(()) => format!(
                        "Model set to \"{m}\" (not in known list — verify your backend supports it)"
                    ),
                    Err(e) => return CommandOutcome::Error(e),
                }
            };
            let m_clone = m.clone();
            if let Err(e) = session.update_meta(|meta| meta.model = Some(m_clone)).await {
                return CommandOutcome::Error(format!("Failed to set model: {e}"));
            }
            CommandOutcome::Text(note)
        }
    }
}

pub(super) async fn role(
    arg: Option<(String, Option<String>)>,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    match arg {
        None => {
            let meta = session.read_meta().await;
            let current_role = meta.role_name.unwrap_or_else(|| "none".to_string());
            let role_prompt = meta.role_prompt.as_deref().unwrap_or("(none)");
            let msg = format!(
                "Current Role: {current_role}\nPrompt: {role_prompt}\n\n\
                 Roles are deprecated. Use per-agent system_prompt: /agent set <name> system_prompt <text>"
            );
            CommandOutcome::Text(msg)
        }
        Some((name, prompt)) => {
            let name_clone = name.clone();
            let prompt_clone = prompt.clone();
            if let Err(e) = session
                .update_meta(|meta| {
                    meta.role_name = Some(name_clone);
                    if let Some(p) = prompt_clone {
                        meta.role_prompt = Some(p);
                    }
                })
                .await
            {
                return CommandOutcome::Error(format!("Failed to set role: {e}"));
            }
            CommandOutcome::Text(format!("Role set to \"{name}\""))
        }
    }
}

pub(super) async fn set_backend(
    name: &str,
    url: &str,
    api_key: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let ref_id = format!("session:{}:{name}", ctx.session_db_id);
    ctx.secrets
        .insert(ref_id.clone(), api_key.to_string())
        .await;
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let name_owned = name.to_string();
    let url_owned = url.to_string();
    let ref_id_clone = ref_id.clone();
    if let Err(e) = session
        .update_meta(|meta| {
            meta.backend_name = Some(name_owned);
            meta.backend_url = Some(url_owned);
            meta.backend_key_ref = Some(ref_id_clone);
        })
        .await
    {
        return CommandOutcome::Error(format!("Failed to set backend: {e}"));
    }
    CommandOutcome::Text(format!("Successfully added backend {name}"))
}

pub(super) async fn list_backends(ctx: &CommandContext<'_>) -> CommandOutcome {
    let msg = format!(
        "Known Backends:\n{}\n\nKnown Models:\n{}",
        ctx.backend.list_known_backends().join("\n"),
        ctx.backend.list_known_models().join("\n")
    );
    CommandOutcome::Text(msg)
}
