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
//! - `session` — session CRUD, channels, compact/print, schedules, LLM config
//! - `agent`   — Living Agents participation + lifecycle (attach/detach/new/delete/import/share/...)
//!
//! Scheduling used to live here as session "heartbeat rules"; it now
//! lives in [`crate::extensions::schedule`] as agent-owned schedules.
//! There is no detach/delete sweep — a schedule whose owning agent is
//! gone self-skips at fire time.

use crate::backends::BackendManager;
use crate::security::SecretStore;
use crate::server::Server;
use crate::types::ConversationId;

use std::sync::Arc;

mod agent;
mod extensions;
mod session;
mod sharing;

pub use extensions::{ExtensionsAction, split_ext_scope};

/// User-visible permission level for co-ownership grants on an Agent DB
/// (Co-owned Agents Stage 10). Stays separate from eidetica's `Permission`
/// so the CLI grammar is stable if eidetica's type evolves (e.g. more
/// Admin tiers). `Admin` grants `Permission::Admin(1)` — the creator's
/// `Admin(0)` remains exclusive and ungrantable via `/agent invite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoOwnerPermission {
    Admin,
    Write,
    Read,
}

/// Parse a CLI permission token (admin | write | read). Empty token =>
/// `Admin` (the sensible default for `/agent invite`). Shared by TUI and
/// Matrix parsers so the grammar stays in one place.
pub fn parse_permission_token(tok: &str) -> Option<CoOwnerPermission> {
    match tok.to_ascii_lowercase().as_str() {
        "" | "admin" | "a" => Some(CoOwnerPermission::Admin),
        "write" | "w" => Some(CoOwnerPermission::Write),
        "read" | "r" => Some(CoOwnerPermission::Read),
        _ => None,
    }
}

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
    /// Stop sharing the current session — disable sync so the source peer
    /// stops serving it to ticket holders. Does not revoke any keys that
    /// may already be held by other peers.
    SessionUnshare,
    /// Sync a remote session via ticket URL.
    Sync(String),
    /// Summarize and compact the current session's context.
    Compact,
    /// Dump the transcript of the current session.
    Print,
    /// Aggregate LLM usage and cost across all sessions.
    ListCosts,

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
    /// Multi-agent chat-room status: roster, host, burst-budget state.
    AgentRoom,
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
    /// Request access to an agent DB via eidetica's bootstrap workflow. If
    /// the requester's pubkey is already authorized (preseed via
    /// `/agent invite`) the sync proceeds and the agent is registered
    /// locally. Otherwise a pending bootstrap request is queued for the
    /// owner to handle via `/sharing approve`. Default permission: write.
    AgentImport {
        ticket: String,
        permission: CoOwnerPermission,
    },
    /// List every Living Agent this peer hosts (from the `agents` index).
    AgentHosted,
    /// Unregister a Living Agent locally (index + runtime registry). The
    /// agent DB is preserved for archive — memory and history stay readable.
    AgentDelete(String),
    /// Stop sharing an agent DB — disable sync so this peer stops serving
    /// it. Does not revoke any keys held by peers who already imported it.
    AgentUnshare(String),
    /// Edit a single field on a Living Agent's DB config. Takes effect on
    /// the next message via Stage 8 hydration — no restart needed.
    AgentSet {
        agent_ref: String,
        field: String,
        value: String,
    },
    /// Print this peer's default pubkey so an agent owner can paste it
    /// into `/agent invite` on their peer (Co-owned Agents Stage 10).
    Pubkey,
    /// Grant a remote peer's pubkey access to an agent DB — admin (default),
    /// write, or read. Owner stays Admin(0); co-owner becomes Admin(1) and
    /// below.
    AgentInvite {
        agent_ref: String,
        pubkey: String,
        permission: CoOwnerPermission,
    },
    /// Revoke a previously-invited pubkey on an agent DB. Historical
    /// entries signed by the key remain verifiable; no new writes.
    AgentRevokePeer {
        agent_ref: String,
        pubkey: String,
    },

    // --- Sharing queue (Co-owned Stage 11) ---
    /// List bootstrap requests on this peer's `_sync` DB that are still
    /// pending an admin's approval. Owner-side surface for `/sharing
    /// requests`. The output lists each request with the resource kind
    /// (agent/bank/session) and display name when known.
    SharingRequests,
    /// Approve a queued bootstrap request. Grants the requester's pubkey
    /// the permission they asked for on the target DB. Requires this
    /// peer to hold an Admin key on that DB. After approval, the
    /// requester must re-run their `/agent import` (or `/memory import`,
    /// or `/sync`) to actually pull the entries — eidetica doesn't push.
    SharingApprove(String),
    /// Reject a queued bootstrap request. Marks it Rejected; the
    /// requester's bootstrap retries will keep failing for the lifetime
    /// of the request entry.
    SharingReject(String),
    /// List every database this peer is currently sharing (sync enabled),
    /// grouped by kind (agent / memory bank / session). Shows display
    /// names when available and root IDs for unambiguous identification.
    SharingStatus,

    // --- Extension framework control ---
    /// Built-in `/extensions` command. Controls per-session activation
    /// and settings for the compile-time extensions registered on the
    /// hub. Not an extension command — removing the framework's own
    /// control surface would be a footgun.
    Extensions(ExtensionsAction),

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

    /// Slash command registered by a chaz extension (see `crate::extension`).
    /// Gateways produce this variant when a `/foo` doesn't match any
    /// built-in. `dispatch` looks `name` up in `Server::extensions().commands`
    /// and routes there; an unregistered name yields an "Unknown command"
    /// error.
    Extension {
        name: String,
        args: String,
    },
}

/// Built-in slash command names (without leading `/`). Used at hub
/// construction time to reserve names that extensions cannot shadow.
pub const BUILTIN_COMMAND_NAMES: &[&str] = &[
    "quit",
    "exit",
    "q",
    "sessions",
    "s",
    "share",
    "unshare",
    "compact",
    "schedules",
    "info",
    "costs",
    "print",
    "backends",
    "new",
    "name",
    "rename",
    "role",
    "model",
    "channels",
    "agents",
    "pubkey",
    "help",
    "?",
    "agent",
    "extensions",
    "sharing",
    "sync",
    "use",
    "switch",
];

/// Data about a session, used to render a picker (TUI) or a listing (Matrix).
pub struct SessionInfo {
    pub session_db_id: String,
    pub agent_name: Option<String>,
    pub name: Option<String>,
    pub entry_count: usize,
    pub last_message: Option<String>,
    /// Normalized gateway-of-origin from the session catalog.
    pub gateway: crate::session::GatewayKind,
    /// Catalog creation timestamp. `None` for sessions that predate the
    /// catalog (legacy rows in the routing index).
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: crate::session::SessionStatus,
    /// Sum of `cost_usd` across every assistant entry with `ResponseMetadata`.
    /// `cost_reported` distinguishes "$0.00 because no calls had cost data"
    /// from "$0.00 because every call was free".
    pub total_cost_usd: f64,
    pub cost_reported: bool,
    /// Number of assistant messages with recorded metadata. Useful for
    /// distinguishing "no LLM activity" from "LLM activity but uncosted".
    pub llm_call_count: u32,
}

/// Everything a command handler needs. Borrowed from the gateway.
pub struct CommandContext<'a> {
    pub server: &'a Arc<Server>,
    pub secrets: &'a SecretStore,
    pub backend: &'a BackendManager,
    /// The eidetica root ID of the currently active session.
    pub session_db_id: &'a str,
    pub session_db: &'a eidetica::Database,
    pub current_agent: &'a str,
    pub session_name: Option<&'a str>,
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
        Command::ListCosts => session::list_costs(ctx).await,
        Command::NameSession(name) => session::name_session(&name, ctx).await,
        Command::ClearSessionName => session::clear_session_name(ctx).await,
        Command::Share => session::share(ctx).await,
        Command::SessionUnshare => session::unshare(ctx).await,
        Command::Sync(ticket) => session::sync_ticket(&ticket, ctx).await,
        Command::Compact => session::compact(ctx).await,
        Command::Print => session::print_transcript(ctx).await,
        Command::ListChannels => session::list_channels(ctx).await,
        Command::AgentAdd(r) => agent::agent_add(&r, ctx).await,
        Command::AgentRemove(r) => agent::agent_remove(&r, ctx).await,
        Command::AgentsList => agent::agents_list(ctx).await,
        Command::AgentRoom => agent::agent_room(ctx).await,
        Command::AgentSetHost(arg) => agent::agent_set_host(arg.as_deref(), ctx).await,
        Command::AgentNew { name, overrides } => agent::agent_new(&name, &overrides, ctx).await,
        Command::AgentShare(r) => agent::agent_share(&r, ctx).await,
        Command::AgentUnshare(r) => agent::agent_unshare(&r, ctx).await,
        Command::AgentImport { ticket, permission } => {
            agent::agent_import(&ticket, permission, ctx).await
        }
        Command::AgentHosted => agent::agent_hosted(ctx).await,
        Command::AgentDelete(r) => agent::agent_delete(&r, ctx).await,
        Command::AgentSet {
            agent_ref,
            field,
            value,
        } => agent::agent_set(&agent_ref, &field, &value, ctx).await,
        Command::Pubkey => agent::pubkey(ctx).await,
        Command::AgentInvite {
            agent_ref,
            pubkey,
            permission,
        } => agent::agent_invite(&agent_ref, &pubkey, permission, ctx).await,
        Command::AgentRevokePeer { agent_ref, pubkey } => {
            agent::agent_revoke_peer(&agent_ref, &pubkey, ctx).await
        }
        Command::SharingRequests => sharing::sharing_requests(ctx).await,
        Command::SharingApprove(id) => sharing::sharing_approve(&id, ctx).await,
        Command::SharingReject(id) => sharing::sharing_reject(&id, ctx).await,
        Command::SharingStatus => sharing::sharing_status(ctx).await,
        Command::Extensions(action) => extensions::dispatch(action, ctx).await,
        Command::Model(arg) => session::model(arg, ctx).await,
        Command::Role(arg) => session::role(arg, ctx).await,
        Command::SetBackend { name, url, api_key } => {
            session::set_backend(&name, &url, &api_key, ctx).await
        }
        Command::ListBackends => session::list_backends(ctx).await,
        Command::Quit => CommandOutcome::Quit,
        Command::Extension { name, args } => dispatch_extension(&name, &args, ctx).await,
    }
}

async fn dispatch_extension(name: &str, args: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    use crate::extension::{ExtensionCommandOutcome, HookContext};
    use crate::session::Session;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let hub = ctx.server.extensions();
    if !hub.has_command(name) {
        return CommandOutcome::Error(format!(
            "Unknown command: /{name}. Type /help for available commands."
        ));
    }

    let active_extensions = ctx.server.active_extensions_for(ctx.session_db_id).await;

    // Surface a clearer error when the command exists but its owner
    // extension is inactive on this session — otherwise the dispatch
    // would just return None and the user sees a misleading "unknown
    // command" message.
    if let Some(owner) = hub.command_owner(name)
        && !active_extensions.contains(owner)
    {
        return CommandOutcome::Error(format!(
            "/{name} is provided by the '{owner}' extension, which is not \
             active on this session. Use `/extensions add {owner}` to enable it."
        ));
    }

    let conv_id = crate::types::ConversationId(ctx.session_db_id.to_string());
    let session = Session::new(conv_id, ctx.session_db.clone()).await;
    let hook_ctx = HookContext {
        agent_name: ctx.current_agent.to_string(),
        model: None,
        call_depth: 0,
        session: Arc::new(Mutex::new(session)),
        active_extensions,
    };

    match hub.try_dispatch_command(name, args, &hook_ctx).await {
        Some(ExtensionCommandOutcome::Text(s)) => CommandOutcome::Text(s),
        Some(ExtensionCommandOutcome::Error(s)) => CommandOutcome::Error(s),
        None => CommandOutcome::Error(format!("Unknown command: /{name}")),
    }
}
