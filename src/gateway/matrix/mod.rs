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
use matrix_sdk::Room;
use matrix_sdk::ruma::OwnedEventId;
use matrix_sdk::ruma::events::reaction::OriginalSyncReactionEvent;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{error, info};

use commands::{get_backend, rate_limit};
use history::read_room_history;

/// Pending approval requests keyed by the Matrix event ID of the approval message.
/// When a user reacts or replies, we look up the decision channel here.
type PendingApprovals = Arc<Mutex<HashMap<OwnedEventId, oneshot::Sender<ApprovalDecision>>>>;

/// An approval request tagged with the room it belongs to.
struct RoomApprovalRequest {
    room_id: String,
    exchange: ApprovalExchange,
}

/// Create a per-room approval sender that forwards exchanges to the shared relay.
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

/// Run a transport-neutral command in the context of a Matrix room:
/// builds a `CommandContext` scoped to the room's session, dispatches, and
/// renders the outcome as a room message.
async fn dispatch_in_room(
    cmd: Command,
    room: Room,
    server: Arc<Server>,
    scheduler: Option<Arc<Scheduler>>,
    config: Arc<Config>,
    secrets: SecretStore,
) -> anyhow::Result<()> {
    let room_id = room.room_id().to_string();

    let backend = get_backend(&room, &config, &secrets, server.registry()).await;
    let (_conv_id, session_db) = server.registry().get_or_create_session_db(&room_id).await?;
    let agent = server.registry().resolve_agent(&room_id, None).await;
    let session_name = server
        .registry()
        .get_binding(&room_id)
        .await
        .and_then(|b| b.name);
    let config_roles = Some(get_role_names(config.roles.clone()));

    let ctx = CommandContext {
        server: &server,
        scheduler: scheduler.as_ref(),
        secrets: &secrets,
        backend: &backend,
        transport_id: &room_id,
        session_db: &session_db,
        current_agent: &agent.name,
        session_name: session_name.as_deref(),
        config_roles,
        default_role: config.role.as_deref(),
    };

    let outcome = shared_commands::dispatch(cmd, &ctx).await;
    render_outcome_to_room(&room, outcome).await;
    Ok(())
}

/// Render a dispatch outcome into a Matrix room as a notice / text message.
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
                        info.transport_id, name, agent, info.entry_count
                    ));
                    if let Some(preview) = &info.last_message {
                        s.push_str(&format!("\n    {preview}"));
                    }
                }
                s
            }
        }
        CommandOutcome::SessionSwitched(_) => {
            "!chaz Session switching is not supported from Matrix rooms — each room has its own session.".to_string()
        }
        CommandOutcome::Quit => return,
    };

    if let Err(e) = room.send(RoomMessageEventContent::notice_plain(text)).await {
        tracing::error!("Failed to send command response: {e}");
    }
}

/// Parse the argument portion of a Matrix command.
/// `text` looks like `!chaz <cmd> <args...>` — returns the joined args, trimmed.
fn matrix_args(text: &str) -> String {
    text.split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(" ")
}

impl Gateway for MatrixGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let config = Arc::new(self.config);

        let mut bot = Bot::new(BotConfig {
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
        })
        .await;

        if let Err(e) = bot.login().await {
            error!("Error logging in: {e}");
        }

        bot.join_rooms();

        if let Err(e) = bot.sync().await {
            tracing::warn!("Initial Matrix sync error: {e}");
        }

        info!("The client is ready! Listening to new messages…");

        // === Approval infrastructure ===
        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (approval_relay_tx, mut approval_relay_rx) = mpsc::channel::<RoomApprovalRequest>(64);

        // Spawn relay task: receives approval requests, sends notices to rooms,
        // stores pending decisions for reaction/command handling
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
                            // Deny by default if we can't ask
                            let _ = req.exchange.decision_tx.send(ApprovalDecision::Deny);
                        }
                    }
                }
            });
        }

        // Register reaction handler for approval decisions
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

        // Register approve/deny text commands
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

        // === Register commands (handled directly, not routed through server) ===

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

        let scheduler = self.scheduler.clone();
        let message_counts: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));

        // Helper to register a simple dispatch-based command.
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

        // Track which session DBs have gateway response callbacks registered
        let gateway_watched: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        {
            let config = config.clone();
            let counts = message_counts;
            let secrets = self.secrets.clone();
            let server = server.clone();
            let gateway_watched = gateway_watched.clone();
            let approval_relay_tx = approval_relay_tx.clone();
            let backfilled_rooms: Arc<Mutex<HashSet<String>>> =
                Arc::new(Mutex::new(HashSet::new()));
            let seen_events: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
            bot.register_text_handler(move |sender, body: String, room, event| {
                let config = config.clone();
                let backfilled_rooms = backfilled_rooms.clone();
                let seen_events = seen_events.clone();
                let counts = counts.clone();
                let secrets = secrets.clone();
                let server = server.clone();
                let gateway_watched = gateway_watched.clone();
                let approval_relay_tx = approval_relay_tx.clone();
                async move {
                    // Deduplicate events across sync restarts
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

                    // Read agent override from session registry binding
                    let agent_override = server
                        .registry()
                        .get_binding(&room_id)
                        .await
                        .and_then(|b| b.agent_name.clone());

                    let body = if body.starts_with("!chaz") {
                        body.trim_start_matches("!chaz").trim().to_string()
                    } else {
                        body
                    };

                    let backend = get_backend(&room, &config, &secrets, server.registry()).await;

                    // Get or create session DB
                    let (_conv_id, session_db) =
                        match server.registry().get_or_create_session_db(&room_id).await {
                            Ok(r) => r,
                            Err(e) => {
                                error!("Failed to get session DB: {e}");
                                return Ok(());
                            }
                        };

                    // Create per-room approval channel that feeds the shared relay
                    let approval_tx =
                        make_room_approval_tx(room_id.clone(), approval_relay_tx.clone());

                    // Register server callback (agent processing)
                    if let Err(e) = server
                        .register_session(
                            &room_id,
                            &session_db,
                            backend,
                            agent_override,
                            Some(approval_tx),
                        )
                        .await
                    {
                        error!("Failed to register session: {e}");
                        return Ok(());
                    }

                    // Register gateway callback (response delivery to Matrix room)
                    {
                        let db_id = session_db.root_id().to_string();
                        let mut watched = gateway_watched.lock().await;
                        if !watched.contains(&db_id) {
                            watched.insert(db_id);
                            drop(watched);

                            let matrix_room = room.clone();
                            let agents = server.agents().clone();
                            let rid = room_id.clone();
                            if let Err(e) =
                                session_db.on_local_write(move |_entry, db, _instance| {
                                    let matrix_room = matrix_room.clone();
                                    let agents = agents.clone();
                                    let db = db.clone();
                                    let rid = rid.clone();
                                    Box::pin(async move {
                                        // Read latest entry
                                        let session =
                                            Session::new(crate::types::ConversationId(rid), db)
                                                .await;
                                        if let Some(latest) = session.latest_entry() {
                                            // Send agent messages to Matrix
                                            if latest.entry_type == EntryType::Message
                                                && agents.get(&latest.sender).is_some()
                                            {
                                                info!(
                                                    "→ Matrix: {}",
                                                    latest.content.replace('\n', " ")
                                                );
                                                let content =
                                                    RoomMessageEventContent::text_markdown(
                                                        &latest.content,
                                                    );
                                                if let Err(e) = matrix_room.send(content).await {
                                                    tracing::error!(
                                                        "Failed to send to Matrix: {e}"
                                                    );
                                                }
                                            }
                                        }
                                        Ok(())
                                    })
                                })
                            {
                                error!("Failed to register gateway callback: {e}");
                            } else {
                                info!("Gateway watching session DB for {}", room_id);
                            }
                        }
                    }

                    // Backfill room history on first message per room
                    {
                        let mut backfilled = backfilled_rooms.lock().await;
                        if backfilled.insert(room_id.clone()) {
                            info!("Backfilling history for room {}", room_id);
                            let history = read_room_history(&room).await;
                            let mut session = Session::new(
                                crate::types::ConversationId(room_id.clone()),
                                session_db.clone(),
                            )
                            .await;
                            session.backfill(history).await;
                        }
                    }

                    // Write user entry to session DB — triggers server callback → agent runs
                    // Agent response → triggers gateway callback → sends to Matrix room
                    let mut session =
                        Session::new(crate::types::ConversationId(room_id), session_db).await;
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
