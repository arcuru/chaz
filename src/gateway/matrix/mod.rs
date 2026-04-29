mod commands;
mod history;

use crate::commands::{self as shared_commands, Command, CommandContext, CommandOutcome};
use crate::config::Config;
use crate::gateway::{ApprovalDecision, ApprovalExchange, Gateway};
use crate::role::get_role_names;
use crate::scheduler::Scheduler;
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};

use headjack::*;
use matrix_sdk::ruma::events::reaction::OriginalSyncReactionEvent;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::OwnedEventId;
use matrix_sdk::Room;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{error, info};

use commands::{get_backend, rate_limit};
use history::read_room_history;

type PendingApprovals = Arc<Mutex<HashMap<OwnedEventId, oneshot::Sender<ApprovalDecision>>>>;

struct RoomApprovalRequest {
    room_id: String,
    exchange: ApprovalExchange,
}

fn make_room_approval_tx(
    room_id: String,
    relay_tx: mpsc::Sender<RoomApprovalRequest>,
) -> mpsc::Sender<ApprovalExchange> {
    let (tx, mut rx) = mpsc::channel::<ApprovalExchange>(8);
    tokio::spawn(async move {
        while let Some(exchange) = rx.recv().await {
            let _ = relay_tx
                .send(RoomApprovalRequest {
                    room_id: room_id.clone(),
                    exchange,
                })
                .await;
        }
    });
    tx
}

pub struct MatrixGateway {
    config: Config,
    secrets: SecretStore,
    scheduler: Option<Arc<Scheduler>>,
}

impl MatrixGateway {
    pub fn new(config: Config, secrets: SecretStore) -> anyhow::Result<Self> {
        if config.homeserver_url.is_empty() {
            anyhow::bail!("homeserver_url is required for Matrix gateway");
        }
        if config.username.is_empty() {
            anyhow::bail!("username is required for Matrix gateway");
        }
        Ok(Self {
            config,
            secrets,
            scheduler: None,
        })
    }

    pub fn with_scheduler(mut self, scheduler: Option<Arc<Scheduler>>) -> Self {
        self.scheduler = scheduler;
        self
    }
}

/// Install a response-delivery callback on a session DB that forwards any
/// agent `Message` entries to the given Matrix room. Idempotent: the
/// `attached` set is the caller's dedupe gate.
fn attach_response_callback(
    session_db: &eidetica::Database,
    room: Room,
    agents: Arc<crate::agent::AgentRegistry>,
) -> anyhow::Result<()> {
    let session_db_id = session_db.root_id().to_string();
    session_db.on_local_write(move |_entry, db, _instance| {
        let room = room.clone();
        let agents = agents.clone();
        let db = db.clone();
        let sid = session_db_id.clone();
        Box::pin(async move {
            let session = Session::new(crate::types::ConversationId(sid), db).await;
            if let Some(latest) = session.latest_entry() {
                if latest.entry_type == EntryType::Message && agents.get(&latest.sender).is_some() {
                    info!(
                        "→ Matrix({}): {}",
                        room.room_id(),
                        latest.content.replace('\n', " ")
                    );
                    let content = RoomMessageEventContent::text_markdown(&latest.content);
                    if let Err(e) = room.send(content).await {
                        tracing::error!("Failed to send to Matrix: {e}");
                    }
                }
            }
            Ok(())
        })
    })?;
    Ok(())
}

/// Dispatch a shared command in the context of a Matrix room.
async fn dispatch_in_room(
    cmd: Command,
    room: Room,
    server: Arc<Server>,
    scheduler: Option<Arc<Scheduler>>,
    config: Arc<Config>,
    secrets: SecretStore,
) -> anyhow::Result<()> {
    let room_id = room.room_id().to_string();

    let (_conv_id, session_db) = server
        .registry()
        .get_or_create_matrix_session(&room_id)
        .await?;
    let session_db_id = session_db.root_id().to_string();
    let backend = get_backend(&room, &config, &secrets, server.registry()).await;
    let meta = crate::session::read_meta_from_db(&session_db).await;
    let agent = server
        .registry()
        .resolve_agent(&session_db_id, None, server.agent_index())
        .await;
    let config_roles = Some(get_role_names(config.roles.clone()));

    let ctx = CommandContext {
        server: &server,
        scheduler: scheduler.as_ref(),
        secrets: &secrets,
        backend: &backend,
        session_db_id: &session_db_id,
        session_db: &session_db,
        current_agent: &agent.name,
        session_name: meta.name.as_deref(),
        config_roles,
        default_role: config.role.as_deref(),
    };

    let outcome = shared_commands::dispatch(cmd, &ctx).await;
    render_outcome_to_room(&room, outcome).await;
    Ok(())
}

async fn render_outcome_to_room(room: &Room, outcome: CommandOutcome) {
    let text = match outcome {
        CommandOutcome::Text(t) => t,
        CommandOutcome::Error(e) => format!("!chaz Error: {e}"),
        CommandOutcome::SessionsList(list) => {
            if list.is_empty() {
                "No sessions found.".to_string()
            } else {
                let mut s = String::from("Sessions:");
                for info in &list {
                    let agent = info.agent_name.as_deref().unwrap_or("default");
                    let name = info
                        .name
                        .as_deref()
                        .map(|n| format!(" \"{n}\""))
                        .unwrap_or_default();
                    s.push_str(&format!(
                        "\n  {}{} ({}, {} entries)",
                        info.session_db_id, name, agent, info.entry_count
                    ));
                    if let Some(preview) = &info.last_message {
                        s.push_str(&format!("\n    {preview}"));
                    }
                }
                s
            }
        }
        CommandOutcome::SessionSwitched(_) => {
            "!chaz To bind this room to a different session, use `!chaz attach <session>`."
                .to_string()
        }
        CommandOutcome::Quit => return,
    };

    if let Err(e) = room.send(RoomMessageEventContent::notice_plain(text)).await {
        tracing::error!("Failed to send command response: {e}");
    }
}

/// Parse the argument portion of a Matrix command.
fn matrix_args(text: &str) -> String {
    text.split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse the positional form of `!chaz heartbeat add` (mirrors TUI syntax):
/// `<id> <sec> <min> <hour> <dom> <mon> <dow> <agent_ref> <task…>`.
/// Returns `None` if any field is missing — the caller renders a usage hint.
fn parse_heartbeat_add(rest: &str) -> Option<Command> {
    let mut tokens = rest.split_whitespace();
    let id = tokens.next()?;
    let c1 = tokens.next()?;
    let c2 = tokens.next()?;
    let c3 = tokens.next()?;
    let c4 = tokens.next()?;
    let c5 = tokens.next()?;
    let c6 = tokens.next()?;
    let agent_ref = tokens.next()?;
    let task = tokens.collect::<Vec<_>>().join(" ");
    if task.is_empty() {
        return None;
    }
    Some(Command::HeartbeatAdd {
        id: id.to_string(),
        cron: format!("{c1} {c2} {c3} {c4} {c5} {c6}"),
        agent_ref: agent_ref.to_string(),
        task,
    })
}

impl Gateway for MatrixGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let config = Arc::new(self.config);

        let mut bot = BotConfig {
            command_prefix: None,
            room_size_limit: config.room_size_limit,
            login: Login {
                homeserver_url: config.homeserver_url.clone(),
                username: config.username.clone(),
                password: config.password.clone(),
            },
            name: Some("chaz".to_string()),
            allow_list: config.allow_list.clone(),
            state_dir: config.state_dir.clone(),
        }
        .login()
        .await?;

        bot.join_rooms();

        if let Err(e) = bot.sync().await {
            tracing::warn!("Initial Matrix sync error: {e}");
        }

        info!("The client is ready! Listening to new messages…");

        // === Approval infrastructure ===
        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (approval_relay_tx, mut approval_relay_rx) = mpsc::channel::<RoomApprovalRequest>(64);

        {
            let pending = pending_approvals.clone();
            let client = bot.client().clone();
            tokio::spawn(async move {
                while let Some(req) = approval_relay_rx.recv().await {
                    let room_id_parsed = match matrix_sdk::ruma::RoomId::parse(&req.room_id) {
                        Ok(id) => id,
                        Err(_) => continue,
                    };
                    let Some(room) = client.get_room(&room_id_parsed) else {
                        continue;
                    };

                    let info = &req.exchange.info;
                    let notice = format!(
                        "🔒 **Tool approval required**\n\n\
                         **Tool:** `{}`\n\
                         **Risk:** {:?}\n\
                         **Args:** `{}`\n\n\
                         React: ✅ approve · ❌ deny · ⏭ approve all\n\
                         Or reply: `!chaz approve` / `!chaz deny`",
                        info.name, info.risk_level, info.arguments_display
                    );
                    let content = RoomMessageEventContent::text_markdown(notice);
                    match room.send(content).await {
                        Ok(response) => {
                            let mut p = pending.lock().await;
                            p.insert(response.event_id, req.exchange.decision_tx);
                        }
                        Err(e) => {
                            tracing::error!("Failed to send approval request: {e}");
                            let _ = req.exchange.decision_tx.send(ApprovalDecision::Deny);
                        }
                    }
                }
            });
        }

        {
            let pending = pending_approvals.clone();
            bot.client().add_event_handler(
                move |event: OriginalSyncReactionEvent, room: matrix_sdk::Room| {
                    let pending = pending.clone();
                    async move {
                        let relates_to = &event.content.relates_to;
                        let decision = match relates_to.key.as_str() {
                            "✅" => Some(ApprovalDecision::Approve),
                            "❌" => Some(ApprovalDecision::Deny),
                            "⏭" | "⏭️" => Some(ApprovalDecision::ApproveAll),
                            _ => None,
                        };
                        if let Some(decision) = decision {
                            let event_id = &relates_to.event_id;
                            let mut p = pending.lock().await;
                            if let Some(tx) = p.remove(event_id) {
                                info!(
                                    "Approval decision via reaction in {}: {:?}",
                                    room.room_id(),
                                    decision
                                );
                                let _ = tx.send(decision);
                            }
                        }
                    }
                },
            );
        }

        {
            let pending = pending_approvals.clone();
            bot.register_text_command(
                "approve",
                "".to_string(),
                "Approve the pending tool call".to_string(),
                move |_, _, room| {
                    let pending = pending.clone();
                    async move {
                        let mut p = pending.lock().await;
                        if let Some(event_id) = p.keys().next().cloned() {
                            if let Some(tx) = p.remove(&event_id) {
                                let _ = tx.send(ApprovalDecision::Approve);
                                room.send(RoomMessageEventContent::notice_plain("✅ Approved"))
                                    .await
                                    .unwrap();
                            }
                        } else {
                            room.send(RoomMessageEventContent::notice_plain(
                                "No pending approval requests",
                            ))
                            .await
                            .unwrap();
                        }
                        Ok(())
                    }
                },
            )
            .await;
        }

        {
            let pending = pending_approvals.clone();
            bot.register_text_command(
                "deny",
                "".to_string(),
                "Deny the pending tool call".to_string(),
                move |_, _, room| {
                    let pending = pending.clone();
                    async move {
                        let mut p = pending.lock().await;
                        if let Some(event_id) = p.keys().next().cloned() {
                            if let Some(tx) = p.remove(&event_id) {
                                let _ = tx.send(ApprovalDecision::Deny);
                                room.send(RoomMessageEventContent::notice_plain("❌ Denied"))
                                    .await
                                    .unwrap();
                            }
                        } else {
                            room.send(RoomMessageEventContent::notice_plain(
                                "No pending approval requests",
                            ))
                            .await
                            .unwrap();
                        }
                        Ok(())
                    }
                },
            )
            .await;
        }

        let scheduler = self.scheduler.clone();
        let message_counts: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));

        // Track which session DBs have the Matrix response callback installed.
        // Keyed by session_db_id because a single session may be attached to
        // multiple rooms (fan-out delivery).
        let attached_sessions: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        macro_rules! register_shared {
            ($name:expr, $usage:expr, $desc:expr, |$text_ident:ident| $cmd_expr:expr) => {{
                let server = server.clone();
                let scheduler = scheduler.clone();
                let config = config.clone();
                let secrets = self.secrets.clone();
                bot.register_text_command(
                    $name,
                    $usage,
                    $desc.to_string(),
                    move |_, $text_ident, room| {
                        let server = server.clone();
                        let scheduler = scheduler.clone();
                        let config = config.clone();
                        let secrets = secrets.clone();
                        let cmd: Option<Command> = $cmd_expr;
                        async move {
                            if let Some(cmd) = cmd {
                                if let Err(e) =
                                    dispatch_in_room(cmd, room, server, scheduler, config, secrets)
                                        .await
                                {
                                    tracing::error!("Command dispatch failed: {e}");
                                }
                            }
                            Ok(())
                        }
                    },
                )
                .await;
            }};
        }

        // --- Session ops (parity with TUI) ---
        register_shared!(
            "sessions",
            "".to_string(),
            "List all known sessions",
            |_t| { Some(Command::ListSessions) }
        );
        register_shared!("info", "".to_string(), "Show current session info", |_t| {
            Some(Command::Info)
        });
        register_shared!(
            "name",
            "[<alias>]".to_string(),
            "Set (or clear, with no arg) a human-friendly alias for this session",
            |text| {
                let arg = matrix_args(&text);
                if arg.trim().is_empty() {
                    Some(Command::ClearSessionName)
                } else {
                    Some(Command::NameSession(arg.trim().to_string()))
                }
            }
        );
        register_shared!(
            "share",
            "".to_string(),
            "Generate a shareable ticket for the current session",
            |_t| { Some(Command::Share) }
        );
        register_shared!(
            "unshare",
            "".to_string(),
            "Stop sharing the current session",
            |_t| { Some(Command::SessionUnshare) }
        );
        register_shared!(
            "sync",
            "<ticket>".to_string(),
            "Sync a remote session via ticket URL",
            |text| {
                let arg = matrix_args(&text);
                if arg.trim().is_empty() {
                    None
                } else {
                    Some(Command::Sync(arg.trim().to_string()))
                }
            }
        );
        register_shared!(
            "compact",
            "".to_string(),
            "Summarize and compact conversation history",
            |_t| { Some(Command::Compact) }
        );
        register_shared!("print", "".to_string(), "Print the transcript", |_t| {
            Some(Command::Print)
        });

        // --- Living Agents: per-session agent participation ---
        register_shared!(
            "agents",
            "".to_string(),
            "List agents attached to this session",
            |_t| { Some(Command::AgentsList) }
        );
        register_shared!(
            "heartbeat",
            "add|remove|list [args]".to_string(),
            "Manage heartbeat rules on this session",
            |text| {
                let arg = matrix_args(&text);
                let trimmed = arg.trim();
                let mut parts = trimmed.splitn(2, char::is_whitespace);
                let sub = parts.next().unwrap_or("").trim();
                let rest = parts.next().unwrap_or("").trim();
                match sub {
                    "list" | "" => Some(Command::HeartbeatList),
                    "remove" | "rm" if !rest.is_empty() => {
                        Some(Command::HeartbeatRemove(rest.to_string()))
                    }
                    "add" => parse_heartbeat_add(rest),
                    _ => None,
                }
            }
        );
        register_shared!(
            "agent",
            "add|remove|host|list|hosted|new|delete|share|import|set|invite|revoke-peer <arg>"
                .to_string(),
            "Attach/detach, manage host, list, create/delete/share/import/edit/invite/revoke a Living Agent",
            |text| {
                let arg = matrix_args(&text);
                let mut parts = arg.trim().splitn(2, char::is_whitespace);
                let sub = parts.next().unwrap_or("").trim();
                let rest = parts.next().unwrap_or("").trim();
                match sub {
                    "add" if !rest.is_empty() => Some(Command::AgentAdd(rest.to_string())),
                    "remove" | "rm" if !rest.is_empty() => {
                        Some(Command::AgentRemove(rest.to_string()))
                    }
                    "host" => Some(Command::AgentSetHost(if rest.is_empty() {
                        None
                    } else {
                        Some(rest.to_string())
                    })),
                    "list" | "" => Some(Command::AgentsList),
                    "hosted" => Some(Command::AgentHosted),
                    "new" if !rest.is_empty() => (|| {
                        let mut toks = rest.split_whitespace();
                        let name = toks.next().unwrap_or("").to_string();
                        if name.is_empty() {
                            return None;
                        }
                        let mut overrides = Vec::new();
                        for tok in toks {
                            match tok.split_once('=') {
                                Some((k, v)) if !k.is_empty() => {
                                    overrides.push((k.to_string(), v.to_string()))
                                }
                                _ => return None,
                            }
                        }
                        Some(Command::AgentNew { name, overrides })
                    })(),
                    "delete" | "del" if !rest.is_empty() => {
                        Some(Command::AgentDelete(rest.to_string()))
                    }
                    "share" if !rest.is_empty() => Some(Command::AgentShare(rest.to_string())),
                    "unshare" if !rest.is_empty() => {
                        Some(Command::AgentUnshare(rest.to_string()))
                    }
                    "import" if !rest.is_empty() => (|| {
                        let mut parts = rest.splitn(2, char::is_whitespace);
                        let ticket = parts.next().unwrap_or("").trim();
                        let perm_tok = parts.next().unwrap_or("").trim();
                        if ticket.is_empty() {
                            return None;
                        }
                        let permission = match perm_tok {
                            "" => crate::commands::CoOwnerPermission::Write,
                            other => crate::commands::parse_permission_token(other)?,
                        };
                        Some(Command::AgentImport {
                            ticket: ticket.to_string(),
                            permission,
                        })
                    })(),
                    "set" if !rest.is_empty() => {
                        let mut parts = rest.splitn(3, char::is_whitespace);
                        let agent_ref = parts.next().unwrap_or("").trim();
                        let field = parts.next().unwrap_or("").trim();
                        let value = parts.next().unwrap_or("").trim();
                        if agent_ref.is_empty() || field.is_empty() || value.is_empty() {
                            None
                        } else {
                            Some(Command::AgentSet {
                                agent_ref: agent_ref.to_string(),
                                field: field.to_string(),
                                value: value.to_string(),
                            })
                        }
                    }
                    "invite" if !rest.is_empty() => {
                        let mut parts = rest.splitn(3, char::is_whitespace);
                        let agent_ref = parts.next().unwrap_or("").trim();
                        let pubkey = parts.next().unwrap_or("").trim();
                        let perm = parts.next().unwrap_or("").trim();
                        match crate::commands::parse_permission_token(perm) {
                            Some(permission) if !agent_ref.is_empty() && !pubkey.is_empty() => {
                                Some(Command::AgentInvite {
                                    agent_ref: agent_ref.to_string(),
                                    pubkey: pubkey.to_string(),
                                    permission,
                                })
                            }
                            _ => None,
                        }
                    }
                    "revoke-peer" if !rest.is_empty() => {
                        let mut parts = rest.splitn(2, char::is_whitespace);
                        let agent_ref = parts.next().unwrap_or("").trim();
                        let pubkey = parts.next().unwrap_or("").trim();
                        if agent_ref.is_empty() || pubkey.is_empty() {
                            None
                        } else {
                            Some(Command::AgentRevokePeer {
                                agent_ref: agent_ref.to_string(),
                                pubkey: pubkey.to_string(),
                            })
                        }
                    }
                    _ => None,
                }
            }
        );
        register_shared!(
            "pubkey",
            "".to_string(),
            "Show this peer's default pubkey (for /agent invite on another peer)",
            |_t| { Some(Command::Pubkey) }
        );
        register_shared!(
            "memory",
            "new|list|delete|grant|revoke|share|import <arg>".to_string(),
            "Manage memory banks: create/list/delete/grant/revoke/share/import",
            |text| {
                let arg = matrix_args(&text);
                let mut parts = arg.trim().splitn(2, char::is_whitespace);
                let sub = parts.next().unwrap_or("").trim();
                let rest = parts.next().unwrap_or("").trim();
                match sub {
                    "list" | "" => Some(Command::MemoryList),
                    "new" if !rest.is_empty() => {
                        let (name, desc) = match rest.split_once(char::is_whitespace) {
                            Some((n, r)) => (n.trim(), Some(r.trim().to_string())),
                            None => (rest, None),
                        };
                        if name.is_empty() {
                            None
                        } else {
                            Some(Command::MemoryNew {
                                name: name.to_string(),
                                description: desc.filter(|s| !s.is_empty()),
                            })
                        }
                    }
                    "delete" | "del" if !rest.is_empty() => {
                        Some(Command::MemoryDelete(rest.to_string()))
                    }
                    "grant" if !rest.is_empty() => (|| {
                        let mut gparts = rest.splitn(3, char::is_whitespace);
                        let bank = gparts.next().unwrap_or("").trim();
                        let agent = gparts.next().unwrap_or("").trim();
                        let perm = gparts.next().unwrap_or("").trim();
                        let permission = match perm.to_ascii_lowercase().as_str() {
                            "read" | "r" => crate::agent_db::BankPermission::Read,
                            "write" | "w" => crate::agent_db::BankPermission::Write,
                            _ => return None,
                        };
                        if bank.is_empty() || agent.is_empty() {
                            return None;
                        }
                        Some(Command::MemoryGrant {
                            bank_ref: bank.to_string(),
                            agent_ref: agent.to_string(),
                            permission,
                        })
                    })(),
                    "revoke" if !rest.is_empty() => {
                        let mut rparts = rest.splitn(2, char::is_whitespace);
                        let bank = rparts.next().unwrap_or("").trim();
                        let agent = rparts.next().unwrap_or("").trim();
                        if bank.is_empty() || agent.is_empty() {
                            None
                        } else {
                            Some(Command::MemoryRevoke {
                                bank_ref: bank.to_string(),
                                agent_ref: agent.to_string(),
                            })
                        }
                    }
                    "share" if !rest.is_empty() => Some(Command::MemoryShare(rest.to_string())),
                    "unshare" if !rest.is_empty() => {
                        Some(Command::MemoryUnshare(rest.to_string()))
                    }
                    "import" if !rest.is_empty() => (|| {
                        let mut parts = rest.splitn(2, char::is_whitespace);
                        let ticket = parts.next().unwrap_or("").trim();
                        let perm_tok = parts.next().unwrap_or("").trim();
                        if ticket.is_empty() {
                            return None;
                        }
                        let permission = match perm_tok {
                            "" => crate::commands::CoOwnerPermission::Write,
                            other => crate::commands::parse_permission_token(other)?,
                        };
                        Some(Command::MemoryImport {
                            ticket: ticket.to_string(),
                            permission,
                        })
                    })(),
                    _ => None,
                }
            }
        );

        // --- Bootstrap-queue surface (Co-owned Stage 11) ---
        register_shared!(
            "sharing",
            "status | requests | approve <id> | reject <id>".to_string(),
            "Inspect shared DBs / manage bootstrap requests across agent/bank/session DBs",
            |text| {
                let arg = matrix_args(&text);
                let mut parts = arg.trim().splitn(2, char::is_whitespace);
                let sub = parts.next().unwrap_or("").trim();
                let rest = parts.next().unwrap_or("").trim();
                match sub {
                    "" | "status" => Some(Command::SharingStatus),
                    "requests" | "list" => Some(Command::SharingRequests),
                    "approve" if !rest.is_empty() => {
                        Some(Command::SharingApprove(rest.to_string()))
                    }
                    "reject" if !rest.is_empty() => Some(Command::SharingReject(rest.to_string())),
                    _ => None,
                }
            }
        );

        // --- Matrix channel ops (attach/detach are gateway-local so we can
        //     install the response callback on the new session immediately) ---
        {
            let server = server.clone();
            let secrets = self.secrets.clone();
            let config = config.clone();
            let attached_sessions = attached_sessions.clone();
            bot.register_text_command(
                "attach",
                "<session>".to_string(),
                "Bind this room to a specific session (by name or DB ID)".to_string(),
                move |_, text, room| {
                    let server = server.clone();
                    let secrets = secrets.clone();
                    let config = config.clone();
                    let attached_sessions = attached_sessions.clone();
                    async move {
                        let arg = matrix_args(&text);
                        let arg = arg.trim();
                        if arg.is_empty() {
                            let _ = room
                                .send(RoomMessageEventContent::notice_plain(
                                    "Usage: !chaz attach <session-name-or-id>",
                                ))
                                .await;
                            return Ok(());
                        }
                        let room_id = room.room_id().to_string();
                        let (_cv, target_db) = match server.registry().resolve_session(arg).await {
                            Ok(r) => r,
                            Err(e) => {
                                let _ = room
                                    .send(RoomMessageEventContent::notice_plain(format!(
                                        "!chaz Error: unknown session '{arg}': {e}"
                                    )))
                                    .await;
                                return Ok(());
                            }
                        };
                        let target_sid = target_db.root_id().to_string();
                        if let Err(e) = server
                            .registry()
                            .attach_matrix_room(&room_id, &target_sid)
                            .await
                        {
                            let _ = room
                                .send(RoomMessageEventContent::notice_plain(format!(
                                    "!chaz Error: failed to attach: {e}"
                                )))
                                .await;
                            return Ok(());
                        }

                        // Install the response callback on the newly-attached
                        // session so future writes (including scheduler fires)
                        // reach this room.
                        let backend =
                            get_backend(&room, &config, &secrets, server.registry()).await;
                        let agent_override = crate::session::read_meta_from_db(&target_db)
                            .await
                            .agent_name;
                        let _ = server
                            .register_session(&target_db, backend, agent_override, None)
                            .await;
                        let mut attached = attached_sessions.lock().await;
                        if attached.insert(target_sid.clone()) {
                            drop(attached);
                            if let Err(e) = attach_response_callback(
                                &target_db,
                                room.clone(),
                                server.agents_arc(),
                            ) {
                                error!("Failed to attach response callback: {e}");
                            }
                        }

                        let _ = room
                            .send(RoomMessageEventContent::notice_plain(format!(
                                "Attached this room to session {target_sid}."
                            )))
                            .await;
                        Ok(())
                    }
                },
            )
            .await;
        }

        {
            let server = server.clone();
            bot.register_text_command(
                "detach",
                "".to_string(),
                "Detach this room from its session".to_string(),
                move |_, _text, room| {
                    let server = server.clone();
                    async move {
                        let room_id = room.room_id().to_string();
                        match server.registry().detach_matrix_room(&room_id).await {
                            Ok(()) => {
                                let _ = room
                                    .send(RoomMessageEventContent::notice_plain(
                                        "Room detached. Future messages will create a fresh session.",
                                    ))
                                    .await;
                            }
                            Err(e) => {
                                let _ = room
                                    .send(RoomMessageEventContent::notice_plain(format!(
                                        "!chaz Error: {e}"
                                    )))
                                    .await;
                            }
                        }
                        Ok(())
                    }
                },
            )
            .await;
        }

        register_shared!(
            "channels",
            "".to_string(),
            "List Matrix rooms attached to this session",
            |_t| { Some(Command::ListChannels) }
        );

        // --- Scheduler ---
        register_shared!(
            "schedules",
            "".to_string(),
            "List configured schedules",
            |_t| { Some(Command::ListSchedules) }
        );
        register_shared!(
            "run",
            "<name>".to_string(),
            "Trigger a schedule immediately",
            |text| {
                let arg = matrix_args(&text);
                if arg.trim().is_empty() {
                    None
                } else {
                    Some(Command::TriggerSchedule(arg.trim().to_string()))
                }
            }
        );

        // --- LLM config ---
        register_shared!(
            "model",
            "[<model>]".to_string(),
            "Show or set the model",
            |text| {
                let arg = matrix_args(&text);
                let arg = arg.trim();
                Some(Command::Model(if arg.is_empty() {
                    None
                } else {
                    Some(arg.to_string())
                }))
            }
        );
        register_shared!(
            "role",
            "[<role> [<prompt>]]".to_string(),
            "Show, select, or define a role",
            |text| {
                let rest = matrix_args(&text);
                let rest = rest.trim();
                if rest.is_empty() {
                    Some(Command::Role(None))
                } else {
                    let mut parts = rest.splitn(2, char::is_whitespace);
                    let name = parts.next().unwrap_or("").trim().to_string();
                    let prompt = parts.next().map(|s| s.trim().to_string());
                    Some(Command::Role(Some((name, prompt))))
                }
            }
        );
        register_shared!(
            "backend",
            "<name> <api_base> <api_key>".to_string(),
            "Register a custom backend for this session",
            |text| {
                let mut parts = text.split_whitespace().skip(2);
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(n), Some(u), Some(k)) => Some(Command::SetBackend {
                        name: n.to_string(),
                        url: u.to_string(),
                        api_key: k.to_string(),
                    }),
                    _ => None,
                }
            }
        );
        register_shared!(
            "backends",
            "".to_string(),
            "List known backends + models",
            |_t| { Some(Command::ListBackends) }
        );
        register_shared!(
            "list",
            "".to_string(),
            "List available models (alias of backends)",
            |_t| { Some(Command::ListBackends) }
        );

        // --- Matrix-only commands ---
        bot.register_text_command(
            "party",
            "".to_string(),
            "Party!".to_string(),
            |_, _, room| async move {
                let content = RoomMessageEventContent::notice_plain(".🎉🎊🥳 let's PARTY!! 🥳🎊🎉");
                room.send(content).await.unwrap();
                Ok(())
            },
        )
        .await;

        {
            let config = config.clone();
            let counts = message_counts.clone();
            let secrets = self.secrets.clone();
            let registry = server.registry_arc();
            bot.register_text_command(
                "send",
                "<message>".to_string(),
                "Send a message without context".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let counts = counts.clone();
                    let secrets = secrets.clone();
                    let registry = registry.clone();
                    async move {
                        commands::send(sender, text, room, &config, &counts, &secrets, &registry)
                            .await
                    }
                },
            )
            .await;
        }

        bot.register_text_command(
            "clear",
            "".to_string(),
            "Ignore all messages before this point".to_string(),
            |_, _, room| async move {
                room.send(RoomMessageEventContent::notice_plain(
                    "!chaz clear: All messages before this will be ignored",
                ))
                .await
                .unwrap();
                Ok(())
            },
        )
        .await;

        {
            let config = config.clone();
            let counts = message_counts.clone();
            let secrets = self.secrets.clone();
            let registry = server.registry_arc();
            bot.register_text_command(
                "rename",
                "".to_string(),
                "Rename the room and set the topic based on the chat content".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let counts = counts.clone();
                    let secrets = secrets.clone();
                    let registry = registry.clone();
                    async move {
                        commands::rename(sender, text, room, &config, &counts, &secrets, &registry)
                            .await
                    }
                },
            )
            .await;
        }

        // === Text handler — bridges Matrix events to session DB entries ===

        // --- Startup: attach response callbacks + server processing to every
        //     existing Matrix channel for which the bot is joined to the room.
        //     This is what makes scheduled-session responses actually deliver
        //     when no user has recently spoken in the room. ---
        {
            let server = server.clone();
            let client = bot.client().clone();
            let attached_sessions = attached_sessions.clone();
            let config = config.clone();
            let secrets = self.secrets.clone();
            tokio::spawn(async move {
                match server.registry().list_matrix_channels().await {
                    Ok(channels) => {
                        for (room_id, session_db_id) in channels {
                            attach_existing_channel(
                                &server,
                                &client,
                                &attached_sessions,
                                &config,
                                &secrets,
                                &room_id,
                                &session_db_id,
                            )
                            .await;
                        }
                    }
                    Err(e) => error!("Failed to list matrix channels at startup: {e}"),
                }
            });
        }

        {
            let config = config.clone();
            let counts = message_counts;
            let secrets = self.secrets.clone();
            let server = server.clone();
            let approval_relay_tx = approval_relay_tx.clone();
            let backfilled_rooms: Arc<Mutex<HashSet<String>>> =
                Arc::new(Mutex::new(HashSet::new()));
            let seen_events: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
            let attached_sessions = attached_sessions.clone();
            bot.register_text_handler(move |sender, body: String, room, event| {
                let config = config.clone();
                let backfilled_rooms = backfilled_rooms.clone();
                let seen_events = seen_events.clone();
                let counts = counts.clone();
                let secrets = secrets.clone();
                let server = server.clone();
                let approval_relay_tx = approval_relay_tx.clone();
                let attached_sessions = attached_sessions.clone();
                async move {
                    {
                        let mut seen = seen_events.lock().await;
                        if !seen.insert(event.event_id.to_string()) {
                            return Ok(());
                        }
                    }
                    let is_direct =
                        room.is_direct().await.unwrap_or(false) || room.joined_members_count() < 3;

                    let mentions_bot = event
                        .content
                        .mentions
                        .as_ref()
                        .map(|mentions| {
                            mentions
                                .user_ids
                                .iter()
                                .any(|mention| mention == room.client().user_id().unwrap())
                        })
                        .unwrap_or(false);

                    if !(is_direct || body.starts_with("!chaz") || mentions_bot) {
                        return Ok(());
                    }

                    if rate_limit(&room, &sender, &config, &counts).await {
                        return Ok(());
                    }

                    let room_id = room.room_id().to_string();

                    let body = if body.starts_with("!chaz") {
                        body.trim_start_matches("!chaz").trim().to_string()
                    } else {
                        body
                    };

                    let backend = get_backend(&room, &config, &secrets, server.registry()).await;

                    let (_conv_id, session_db) = match server
                        .registry()
                        .get_or_create_matrix_session(&room_id)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            error!("Failed to get session for {room_id}: {e}");
                            return Ok(());
                        }
                    };
                    let session_db_id = session_db.root_id().to_string();

                    // Read agent override from session meta
                    let agent_override = crate::session::read_meta_from_db(&session_db)
                        .await
                        .agent_name;

                    let approval_tx =
                        make_room_approval_tx(room_id.clone(), approval_relay_tx.clone());

                    if let Err(e) = server
                        .register_session(&session_db, backend, agent_override, Some(approval_tx))
                        .await
                    {
                        error!("Failed to register session: {e}");
                        return Ok(());
                    }

                    // Install response callback if we haven't already.
                    {
                        let mut attached = attached_sessions.lock().await;
                        if attached.insert(session_db_id.clone()) {
                            drop(attached);
                            if let Err(e) = attach_response_callback(
                                &session_db,
                                room.clone(),
                                server.agents_arc(),
                            ) {
                                error!("Failed to register response callback: {e}");
                            } else {
                                info!(
                                    session_db_id = %session_db_id,
                                    room_id = %room_id,
                                    "Matrix response callback installed"
                                );
                            }
                        }
                    }

                    // Backfill room history on first message per room
                    {
                        let mut backfilled = backfilled_rooms.lock().await;
                        if backfilled.insert(room_id.clone()) {
                            info!("Backfilling history for room {room_id}");
                            let history = read_room_history(&room).await;
                            let mut session = Session::new(
                                crate::types::ConversationId(session_db_id.clone()),
                                session_db.clone(),
                            )
                            .await;
                            session.backfill(history).await;
                        }
                    }

                    // Write user entry to session DB — triggers server → agent → response
                    let mut session =
                        Session::new(crate::types::ConversationId(session_db_id), session_db).await;
                    session
                        .add_entry(SessionEntry {
                            sender: sender.to_string(),
                            content: body,
                            timestamp: chrono::Utc::now(),
                            entry_type: EntryType::Message,
                        })
                        .await;

                    Ok(())
                }
            });
        }

        // Retry loop for transient sync errors
        loop {
            match bot.run().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    error!("Matrix sync error (retrying in 5s): {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }
}

/// Install server processing + response-delivery for a persisted channel at
/// startup. Skips rooms the bot isn't joined to, or sessions that fail to open.
///
/// Without an active user in the room, we pass no approval channel — scheduled
/// Directives fire autonomously. When the user next speaks, the text handler
/// re-registers the session with an approval channel bound to that message.
async fn attach_existing_channel(
    server: &Arc<Server>,
    client: &matrix_sdk::Client,
    attached_sessions: &Arc<Mutex<HashSet<String>>>,
    config: &Arc<Config>,
    secrets: &SecretStore,
    room_id: &str,
    session_db_id: &str,
) {
    let Ok(room_id_parsed) = matrix_sdk::ruma::RoomId::parse(room_id) else {
        return;
    };
    let Some(room) = client.get_room(&room_id_parsed) else {
        tracing::debug!(room_id, "Not joined to room; skipping channel attach");
        return;
    };

    let Ok((_conv_id, session_db)) = server.registry().open_session(session_db_id).await else {
        tracing::warn!(session_db_id, "Stale matrix channel — session not openable");
        return;
    };

    let agent_override = crate::session::read_meta_from_db(&session_db)
        .await
        .agent_name;
    let backend = get_backend(&room, config, secrets, server.registry()).await;
    if let Err(e) = server
        .register_session(&session_db, backend, agent_override, None)
        .await
    {
        error!(session_db_id, "Failed to register session at startup: {e}");
        return;
    }

    {
        let mut attached = attached_sessions.lock().await;
        if !attached.insert(session_db_id.to_string()) {
            return;
        }
    }

    if let Err(e) = attach_response_callback(&session_db, room, server.agents_arc()) {
        error!("Failed to attach response callback at startup: {e}");
    } else {
        info!(
            session_db_id,
            room_id, "Matrix channel attached at startup (server + response callbacks installed)"
        );
    }
}
