//! Transport-neutral session command dispatch.
//!
//! Gateways (Matrix, TUI, future HTTP/etc.) parse their own syntax into a
//! `Command`, call `dispatch`, and render the `CommandOutcome` to their
//! transport. All the session/registry/scheduler/backend mutation logic
//! lives here — gateways are pure adapters.
//!
//! Transport-specific commands (e.g. Matrix room `rename`, TUI `/debug`)
//! stay in the gateway modules — this file is only for the session ops
//! that make sense across transports.

use crate::backends::{BackendManager, ChatContext, Message};
use crate::defaults::DEFAULT_CONFIG;
use crate::role::get_role_names;
use crate::scheduler::Scheduler;
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};
use crate::types::ConversationId;

use eidetica::store::Table;
use openai_api_rs::v1::chat_completion::MessageRole;
use std::sync::Arc;

/// Parsed, transport-neutral command intent.
pub enum Command {
    // --- Session management ---
    /// Enumerate all known sessions (TUI opens a picker; Matrix renders text).
    ListSessions,
    /// Resolve identifier (name | DB ID | transport ID) and switch to it.
    /// If the identifier is a fresh one, a new session is created.
    SwitchSession(String),
    /// Show info about the current session.
    Info,
    /// Give the current session a human-friendly alias.
    NameSession(String),
    /// Remove the current session's alias.
    ClearSessionName,
    /// Generate a shareable ticket URL for the current session.
    Share,
    /// Sync a remote session via ticket URL.
    Sync(String),
    /// Summarize and compact the current session's context.
    Compact,
    /// Dump the transcript of the current session.
    Print,

    // --- Scheduler ---
    ListSchedules,
    TriggerSchedule(String),

    // --- LLM configuration (per-session) ---
    /// Show current model + known models (None), or bind a model for this session (Some).
    Model(Option<String>),
    /// Show current role (None), select role (Some(name, None)),
    /// or define a new role (Some(name, Some(prompt))).
    Role(Option<(String, Option<String>)>),
    /// Register a custom backend for this session.
    SetBackend {
        name: String,
        url: String,
        api_key: String,
    },
    /// Show known backends + models (no current-model row).
    ListBackends,

    /// Signal to end the session (TUI quits; Matrix ignores).
    Quit,
}

/// Data about a session, used to render a picker (TUI) or a listing (Matrix).
pub struct SessionInfo {
    pub transport_id: String,
    pub agent_name: Option<String>,
    pub name: Option<String>,
    pub entry_count: usize,
    pub last_message: Option<String>,
}

/// Everything a command handler needs. Borrowed from the gateway.
pub struct CommandContext<'a> {
    pub server: &'a Arc<Server>,
    pub scheduler: Option<&'a Arc<Scheduler>>,
    pub secrets: &'a SecretStore,
    pub backend: &'a BackendManager,
    pub transport_id: &'a str,
    pub session_db: &'a eidetica::Database,
    pub current_agent: &'a str,
    pub session_name: Option<&'a str>,
    /// User-visible role names configured at the top level (for `Role(None)` listing).
    pub config_roles: Option<Vec<String>>,
    /// Default role name from config, used as a fallback for "current role".
    pub default_role: Option<&'a str>,
}

/// Payload for `CommandOutcome::SessionSwitched`.
pub struct SessionSwitch {
    pub transport_id: String,
    pub conv_id: ConversationId,
    pub db: eidetica::Database,
    pub agent_name: String,
    pub session_name: Option<String>,
}

/// Structured outcome of dispatching a command.
pub enum CommandOutcome {
    /// Plain text to show the user.
    Text(String),
    /// Error message.
    Error(String),
    /// List of sessions — TUI renders a modal picker, Matrix renders text.
    SessionsList(Vec<SessionInfo>),
    /// The command switched the active session — gateway re-registers and reloads.
    SessionSwitched(Box<SessionSwitch>),
    /// User requested to quit (TUI honors; Matrix ignores).
    Quit,
}

/// Run a parsed command against the given context.
pub async fn dispatch(cmd: Command, ctx: &CommandContext<'_>) -> CommandOutcome {
    match cmd {
        Command::ListSessions => list_sessions(ctx).await,
        Command::SwitchSession(id) => switch_session(&id, ctx).await,
        Command::Info => info(ctx).await,
        Command::NameSession(name) => name_session(&name, ctx).await,
        Command::ClearSessionName => clear_session_name(ctx).await,
        Command::Share => share(ctx).await,
        Command::Sync(ticket) => sync_ticket(&ticket, ctx).await,
        Command::Compact => compact(ctx).await,
        Command::Print => print_transcript(ctx).await,
        Command::ListSchedules => list_schedules(ctx).await,
        Command::TriggerSchedule(name) => trigger_schedule(&name, ctx).await,
        Command::Model(arg) => model(arg, ctx).await,
        Command::Role(arg) => role(arg, ctx).await,
        Command::SetBackend { name, url, api_key } => set_backend(&name, &url, &api_key, ctx).await,
        Command::ListBackends => list_backends(ctx).await,
        Command::Quit => CommandOutcome::Quit,
    }
}

// -----------------------------------------------------------------------------
// Session ops
// -----------------------------------------------------------------------------

async fn list_sessions(ctx: &CommandContext<'_>) -> CommandOutcome {
    let bindings = match ctx.server.registry().list_sessions().await {
        Ok(b) => b,
        Err(e) => return CommandOutcome::Error(format!("Failed to list sessions: {e}")),
    };

    let mut sessions = Vec::new();
    for binding in bindings {
        let (entry_count, last_message) = match ctx
            .server
            .registry()
            .get_or_create_session_db(&binding.transport_id)
            .await
        {
            Ok((conv_id, db)) => {
                let session = Session::new(conv_id, db).await;
                let count = session.entries().len();
                let last = session
                    .entries()
                    .iter()
                    .rev()
                    .find(|e| e.entry_type == EntryType::Message)
                    .map(|e| {
                        let preview = e.content.lines().next().unwrap_or("");
                        if preview.len() > 60 {
                            format!("{}: {}…", e.sender, &preview[..60])
                        } else {
                            format!("{}: {}", e.sender, preview)
                        }
                    });
                (count, last)
            }
            Err(_) => (0, None),
        };
        sessions.push(SessionInfo {
            transport_id: binding.transport_id,
            agent_name: binding.agent_name,
            name: binding.name,
            entry_count,
            last_message,
        });
    }

    CommandOutcome::SessionsList(sessions)
}

async fn switch_session(identifier: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let (transport_id, conv_id, db) = match ctx.server.registry().resolve_session(identifier).await
    {
        Ok(r) => r,
        Err(e) => return CommandOutcome::Error(format!("Failed to switch session: {e}")),
    };

    let session_name = ctx
        .server
        .registry()
        .get_binding(&transport_id)
        .await
        .and_then(|b| b.name);

    let agent = ctx
        .server
        .registry()
        .resolve_agent(&transport_id, None)
        .await;

    CommandOutcome::SessionSwitched(Box::new(SessionSwitch {
        transport_id,
        conv_id,
        db,
        agent_name: agent.name,
        session_name,
    }))
}

async fn info(ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.transport_id.to_string()),
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
    let db_id = ctx.session_db.root_id().to_string();
    let name_line = match ctx.session_name {
        Some(n) => format!("\nName: {n}"),
        None => String::new(),
    };
    CommandOutcome::Text(format!(
        "Session: {}{name_line}\nAgent: {}\nDatabase ID: {db_id}\nTotal entries: {}\nMessages: {msg_count} | Directives: {directive_count} | Tool calls: {tool_count} | Errors: {error_count}",
        ctx.transport_id,
        ctx.current_agent,
        entries.len(),
    ))
}

async fn name_session(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    if name.is_empty() {
        return CommandOutcome::Error("Usage: name <alias>".to_string());
    }
    match ctx
        .server
        .registry()
        .set_session_name(ctx.transport_id, name.to_string())
        .await
    {
        Ok(()) => CommandOutcome::Text(format!("Session named '{name}'. Use it with join {name}.")),
        Err(e) => CommandOutcome::Error(format!("Failed to name session: {e}")),
    }
}

async fn clear_session_name(ctx: &CommandContext<'_>) -> CommandOutcome {
    match ctx
        .server
        .registry()
        .clear_session_name(ctx.transport_id)
        .await
    {
        Ok(()) => CommandOutcome::Text("Session name cleared.".to_string()),
        Err(e) => CommandOutcome::Error(format!("Failed to clear name: {e}")),
    }
}

async fn share(ctx: &CommandContext<'_>) -> CommandOutcome {
    let instance = ctx.server.registry().instance();
    let Some(sync) = instance.sync() else {
        return CommandOutcome::Error("Sync not enabled".to_string());
    };
    let mut ticket = eidetica::sync::DatabaseTicket::new(ctx.session_db.root_id().clone());
    if let Ok(addresses) = sync.get_all_server_addresses().await {
        for (transport_type, address) in addresses {
            ticket.add_address(eidetica::sync::Address::new(transport_type, address));
        }
    }
    CommandOutcome::Text(format!(
        "Share this ticket to sync the current session:\n\n{ticket}"
    ))
}

async fn sync_ticket(ticket_str: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let instance = ctx.server.registry().instance();
    let Some(sync) = instance.sync() else {
        return CommandOutcome::Error("Sync not enabled".to_string());
    };
    let ticket: eidetica::sync::DatabaseTicket = match ticket_str.parse() {
        Ok(t) => t,
        Err(e) => return CommandOutcome::Error(format!("Invalid ticket: {e}")),
    };
    let db_id = ticket.database_id().clone();
    match sync.sync_with_ticket(&ticket).await {
        Ok(()) => {
            CommandOutcome::Text(format!("Synced database {db_id}. Use sessions to find it."))
        }
        Err(e) => CommandOutcome::Error(format!("Sync failed: {e}")),
    }
}

async fn compact(ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.transport_id.to_string()),
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
            Message::new(MessageRole::system, system_prompt),
            Message::new(
                MessageRole::user,
                format!(
                    "Summarize this conversation:\n\n{transcript}\n\n\
                     Produce a structured summary that captures everything needed to continue the conversation."
                ),
            ),
        ],
        model: None,
        role: None,
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

async fn print_transcript(ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.transport_id.to_string()),
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
// Scheduler
// -----------------------------------------------------------------------------

async fn list_schedules(ctx: &CommandContext<'_>) -> CommandOutcome {
    let Some(sched) = ctx.scheduler else {
        return CommandOutcome::Text("No scheduler configured.".to_string());
    };
    let schedules = sched.list().await;
    if schedules.is_empty() {
        return CommandOutcome::Text("No schedules configured.".to_string());
    }
    let mut msg = String::from("Schedules:\n");
    for s in &schedules {
        let status = if s.enabled { "enabled" } else { "disabled" };
        let last = s
            .last_run
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "never".to_string());
        let next = s
            .next_run
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "n/a".to_string());
        msg.push_str(&format!(
            "\n  {} [{}]\n    session: {}\n    task: {}\n    last: {} | next: {}\n",
            s.name, status, s.session, s.task, last, next
        ));
    }
    CommandOutcome::Text(msg)
}

async fn trigger_schedule(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let Some(sched) = ctx.scheduler else {
        return CommandOutcome::Error("No scheduler configured.".to_string());
    };
    match sched.trigger(name).await {
        Ok(()) => CommandOutcome::Text(format!("Triggered schedule '{name}'.")),
        Err(e) => CommandOutcome::Error(format!("Failed to trigger '{name}': {e}")),
    }
}

// -----------------------------------------------------------------------------
// LLM config
// -----------------------------------------------------------------------------

async fn model(arg: Option<String>, ctx: &CommandContext<'_>) -> CommandOutcome {
    match arg {
        None => {
            let current = ctx
                .server
                .registry()
                .get_binding(ctx.transport_id)
                .await
                .and_then(|b| b.model)
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
            if let Err(e) = ctx
                .server
                .registry()
                .update_binding(ctx.transport_id, |b| {
                    b.model = Some(m_clone);
                })
                .await
            {
                return CommandOutcome::Error(format!("Failed to set model: {e}"));
            }
            CommandOutcome::Text(note)
        }
    }
}

async fn role(arg: Option<(String, Option<String>)>, ctx: &CommandContext<'_>) -> CommandOutcome {
    match arg {
        None => {
            let binding = ctx.server.registry().get_binding(ctx.transport_id).await;
            let current_role = binding
                .as_ref()
                .and_then(|b| b.role_name.clone())
                .or_else(|| ctx.default_role.map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown".to_string());
            let config_roles = ctx.config_roles.clone().unwrap_or_default();
            let default_roles = get_role_names(DEFAULT_CONFIG.roles.clone());
            let mut msg = format!("Current Role: {current_role}");
            if !config_roles.is_empty() {
                msg.push_str(&format!(
                    "\n\nConfigured Roles:\n{}",
                    config_roles.join("\n")
                ));
            }
            if !default_roles.is_empty() {
                msg.push_str(&format!("\n\nBuiltin Roles:\n{}", default_roles.join("\n")));
            }
            CommandOutcome::Text(msg)
        }
        Some((name, prompt)) => {
            let name_clone = name.clone();
            let prompt_clone = prompt.clone();
            if let Err(e) = ctx
                .server
                .registry()
                .update_binding(ctx.transport_id, |b| {
                    b.role_name = Some(name_clone);
                    if let Some(p) = prompt_clone {
                        b.role_prompt = Some(p);
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

async fn set_backend(
    name: &str,
    url: &str,
    api_key: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let ref_id = format!("session:{}:{name}", ctx.transport_id);
    ctx.secrets
        .insert(ref_id.clone(), api_key.to_string())
        .await;
    let name_owned = name.to_string();
    let url_owned = url.to_string();
    let ref_id_clone = ref_id.clone();
    if let Err(e) = ctx
        .server
        .registry()
        .update_binding(ctx.transport_id, |b| {
            b.backend_name = Some(name_owned);
            b.backend_url = Some(url_owned);
            b.backend_key_ref = Some(ref_id_clone);
        })
        .await
    {
        return CommandOutcome::Error(format!("Failed to set backend: {e}"));
    }
    CommandOutcome::Text(format!("Successfully added backend {name}"))
}

async fn list_backends(ctx: &CommandContext<'_>) -> CommandOutcome {
    let msg = format!(
        "Known Backends:\n{}\n\nKnown Models:\n{}",
        ctx.backend.list_known_backends().join("\n"),
        ctx.backend.list_known_models().join("\n")
    );
    CommandOutcome::Text(msg)
}
