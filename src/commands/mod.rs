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
//!
//! Submodules group handlers by family:
//! - `session`   — session CRUD, channels, compact/print, schedules, LLM config
//! - `agent`     — Living Agents participation + lifecycle (attach/detach/new/delete/import/share/...)
//! - `heartbeat` — per-session heartbeat rules

use crate::backends::BackendManager;
use crate::scheduler::Scheduler;
use crate::security::SecretStore;
use crate::server::Server;
use crate::types::ConversationId;

use std::sync::Arc;

mod agent;
mod heartbeat;
mod memory;
mod session;

/// Parsed, transport-neutral command intent.
pub enum Command {
    // --- Session management ---
    /// Enumerate all known sessions (TUI opens a picker; Matrix renders text).
    ListSessions,
    /// Create a fresh session and switch to it.
    NewSession,
    /// Resolve identifier (name | DB ID) and switch to it.
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

    // --- Matrix channel management ---
    /// List Matrix rooms currently attached to the current session.
    ListChannels,

    // --- Agents participating in the current session (Living Agents) ---
    /// Attach an agent (by name or DB ID) to the current session.
    AgentAdd(String),
    /// Detach an agent (by name or DB ID) from the current session.
    AgentRemove(String),
    /// List agents currently attached to the session.
    AgentsList,
    /// Designate the "host agent" — answers when no @mention pins a turn.
    /// `Some(ref)` sets it; `None` clears it.
    AgentSetHost(Option<String>),

    // --- Agent lifecycle (Living Agents Stage 6) ---
    /// Create a new Living Agent DB. Optional `overrides` apply to the
    /// `AgentDbConfig` before the DB is written — e.g. `role`, `model`,
    /// `max_iterations`, `tools`.
    AgentNew {
        name: String,
        overrides: Vec<(String, String)>,
    },
    /// Generate a DatabaseTicket URL for an agent DB so another peer can
    /// import it via `/agent import`.
    AgentShare(String),
    /// Sync an agent DB from a DatabaseTicket URL and register it locally.
    AgentImport(String),
    /// List every Living Agent this peer hosts (from the `agents` index).
    AgentHosted,
    /// Unregister a Living Agent locally (index + runtime registry). The
    /// agent DB is preserved for archive — memory and history stay readable.
    AgentDelete(String),
    /// Edit a single field on a Living Agent's DB config. Takes effect on
    /// the next message via Stage 8 hydration — no restart needed.
    AgentSet {
        agent_ref: String,
        field: String,
        value: String,
    },

    // --- Memory banks (Memory Banks Stage 9.D) ---
    /// Create a new Memory Bank DB on this peer.
    MemoryNew {
        name: String,
        description: Option<String>,
    },
    /// List every Memory Bank this peer hosts.
    MemoryList,
    /// Unregister a Memory Bank locally (index entry removed; DB preserved
    /// for archive, same semantics as `AgentDelete`).
    MemoryDelete(String),
    /// Grant an agent access to a memory bank. Writes the agent's pubkey
    /// to the bank's AuthSettings (authoritative) and mirrors a
    /// `MemoryBankRef` into the agent's `memory_banks` subtree (view).
    MemoryGrant {
        bank_ref: String,
        agent_ref: String,
        permission: crate::agent_db::BankPermission,
    },
    /// Revoke an agent's access to a memory bank. Reverse of MemoryGrant.
    MemoryRevoke {
        bank_ref: String,
        agent_ref: String,
    },
    /// Generate a `DatabaseTicket` URL for a memory bank so another peer
    /// can import it via `/memory import`.
    MemoryShare(String),
    /// Sync a memory bank from a `DatabaseTicket` URL and register it in
    /// this peer's memory-banks index.
    MemoryImport(String),

    // --- Heartbeat rules (Stage 4b) ---
    /// Add or upsert a heartbeat rule on the current session.
    HeartbeatAdd {
        id: String,
        cron: String,
        agent_ref: String,
        task: String,
    },
    /// Remove a heartbeat rule by id.
    HeartbeatRemove(String),
    /// List heartbeat rules on the current session.
    HeartbeatList,

    // --- Scheduler ---
    ListSchedules,
    TriggerSchedule(String),

    // --- LLM configuration (per-session) ---
    Model(Option<String>),
    Role(Option<(String, Option<String>)>),
    SetBackend {
        name: String,
        url: String,
        api_key: String,
    },
    ListBackends,

    Quit,
}

/// Data about a session, used to render a picker (TUI) or a listing (Matrix).
pub struct SessionInfo {
    pub session_db_id: String,
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
    /// The eidetica root ID of the currently active session.
    pub session_db_id: &'a str,
    pub session_db: &'a eidetica::Database,
    pub current_agent: &'a str,
    pub session_name: Option<&'a str>,
    pub config_roles: Option<Vec<String>>,
    pub default_role: Option<&'a str>,
}

pub struct SessionSwitch {
    pub session_db_id: String,
    pub conv_id: ConversationId,
    pub db: eidetica::Database,
    pub agent_name: String,
    pub session_name: Option<String>,
}

pub enum CommandOutcome {
    Text(String),
    Error(String),
    SessionsList(Vec<SessionInfo>),
    SessionSwitched(Box<SessionSwitch>),
    Quit,
}

pub async fn dispatch(cmd: Command, ctx: &CommandContext<'_>) -> CommandOutcome {
    match cmd {
        Command::ListSessions => session::list_sessions(ctx).await,
        Command::NewSession => session::new_session(ctx).await,
        Command::SwitchSession(id) => session::switch_session(&id, ctx).await,
        Command::Info => session::info(ctx).await,
        Command::NameSession(name) => session::name_session(&name, ctx).await,
        Command::ClearSessionName => session::clear_session_name(ctx).await,
        Command::Share => session::share(ctx).await,
        Command::Sync(ticket) => session::sync_ticket(&ticket, ctx).await,
        Command::Compact => session::compact(ctx).await,
        Command::Print => session::print_transcript(ctx).await,
        Command::ListChannels => session::list_channels(ctx).await,
        Command::AgentAdd(r) => agent::agent_add(&r, ctx).await,
        Command::AgentRemove(r) => agent::agent_remove(&r, ctx).await,
        Command::AgentsList => agent::agents_list(ctx).await,
        Command::AgentSetHost(arg) => agent::agent_set_host(arg.as_deref(), ctx).await,
        Command::AgentNew { name, overrides } => agent::agent_new(&name, &overrides, ctx).await,
        Command::AgentShare(r) => agent::agent_share(&r, ctx).await,
        Command::AgentImport(t) => agent::agent_import(&t, ctx).await,
        Command::AgentHosted => agent::agent_hosted(ctx).await,
        Command::AgentDelete(r) => agent::agent_delete(&r, ctx).await,
        Command::AgentSet {
            agent_ref,
            field,
            value,
        } => agent::agent_set(&agent_ref, &field, &value, ctx).await,
        Command::MemoryNew { name, description } => {
            memory::memory_new(&name, description.as_deref(), ctx).await
        }
        Command::MemoryList => memory::memory_list(ctx).await,
        Command::MemoryDelete(r) => memory::memory_delete(&r, ctx).await,
        Command::MemoryGrant {
            bank_ref,
            agent_ref,
            permission,
        } => memory::memory_grant(&bank_ref, &agent_ref, permission, ctx).await,
        Command::MemoryRevoke {
            bank_ref,
            agent_ref,
        } => memory::memory_revoke(&bank_ref, &agent_ref, ctx).await,
        Command::MemoryShare(r) => memory::memory_share(&r, ctx).await,
        Command::MemoryImport(t) => memory::memory_import(&t, ctx).await,
        Command::HeartbeatAdd {
            id,
            cron,
            agent_ref,
            task,
        } => heartbeat::heartbeat_add(&id, &cron, &agent_ref, &task, ctx).await,
        Command::HeartbeatRemove(id) => heartbeat::heartbeat_remove(&id, ctx).await,
        Command::HeartbeatList => heartbeat::heartbeat_list(ctx).await,
        Command::ListSchedules => session::list_schedules(ctx).await,
        Command::TriggerSchedule(name) => session::trigger_schedule(&name, ctx).await,
        Command::Model(arg) => session::model(arg, ctx).await,
        Command::Role(arg) => session::role(arg, ctx).await,
        Command::SetBackend { name, url, api_key } => {
            session::set_backend(&name, &url, &api_key, ctx).await
        }
        Command::ListBackends => session::list_backends(ctx).await,
        Command::Quit => CommandOutcome::Quit,
    }
}
