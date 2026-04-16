mod commands;
mod history;

use crate::config::Config;
use crate::gateway::{ApprovalDecision, ApprovalExchange, Gateway};
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};

use headjack::*;
use matrix_sdk::ruma::events::reaction::OriginalSyncReactionEvent;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::OwnedEventId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{error, info};

use commands::{get_backend, get_context, rate_limit};
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
}

impl MatrixGateway {
    pub fn new(config: Config, secrets: SecretStore) -> anyhow::Result<Self> {
        if config.homeserver_url.is_empty() {
            anyhow::bail!("homeserver_url is required for Matrix gateway");
        }
        if config.username.is_empty() {
            anyhow::bail!("username is required for Matrix gateway");
        }
        Ok(Self { config, secrets })
    }
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
            info!("Error syncing: {e}");
        }

        info!("The client is ready! Listening to new messages…");

        // === Approval infrastructure ===
        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (approval_relay_tx, mut approval_relay_rx) =
            mpsc::channel::<RoomApprovalRequest>(64);

        // Spawn relay task: receives approval requests, sends notices to rooms,
        // stores pending decisions for reaction/command handling
        {
            let pending = pending_approvals.clone();
            let client = bot.client().clone();
            tokio::spawn(async move {
                while let Some(req) = approval_relay_rx.recv().await {
                    let room_id_parsed =
                        match matrix_sdk::ruma::RoomId::parse(&req.room_id) {
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

        // Wrap registry in Arc for sharing across command closures
        let registry = server.registry_arc();

        {
            let config = config.clone();
            let secrets = self.secrets.clone();
            let registry = registry.clone();
            bot.register_text_command(
                "print",
                None,
                Some("Print the conversation".to_string()),
                move |_, _, room| {
                    let config = config.clone();
                    let secrets = secrets.clone();
                    let registry = registry.clone();
                    async move {
                        let context =
                            get_context(&room, &config, &secrets, &registry).await.unwrap();
                        let content =
                            RoomMessageEventContent::notice_plain(context.string_prompt());
                        room.send(content).await.unwrap();
                        Ok(())
                    }
                },
            )
            .await;
        }

        let message_counts: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));

        {
            let config = config.clone();
            let counts = message_counts.clone();
            let secrets = self.secrets.clone();
            let registry = registry.clone();
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

        {
            let secrets = self.secrets.clone();
            let registry = registry.clone();
            bot.register_text_command(
                "model",
                "<model>".to_string(),
                "Select the model to use".to_string(),
                move |sender, text, room| {
                    let secrets = secrets.clone();
                    let registry = registry.clone();
                    async move {
                        commands::set_model(sender, text, room, &secrets, &registry).await
                    }
                },
            )
            .await;
        }

        {
            let secrets = self.secrets.clone();
            let registry = registry.clone();
            bot.register_text_command(
                "backend",
                "<name> <api_base> <api_key>".to_string(),
                "Manually enter an OpenAI Compatible Backend".to_string(),
                move |sender, text, room| {
                    let secrets = secrets.clone();
                    let registry = registry.clone();
                    async move {
                        commands::set_backend(sender, text, room, &secrets, &registry).await
                    }
                },
            )
            .await;
        }

        {
            let config = config.clone();
            let secrets = self.secrets.clone();
            let registry = registry.clone();
            bot.register_text_command(
                "role",
                "[<role>] [<prompt>]".to_string(),
                "Get the role info, set the role, or define a new role".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let secrets = secrets.clone();
                    let registry = registry.clone();
                    async move {
                        commands::set_role(sender, text, room, &config, &secrets, &registry).await
                    }
                },
            )
            .await;
        }

        {
            let config = config.clone();
            let secrets = self.secrets.clone();
            let registry = registry.clone();
            bot.register_text_command(
                "list",
                "".to_string(),
                "List available models".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let secrets = secrets.clone();
                    let registry = registry.clone();
                    async move {
                        commands::list_models(sender, text, room, &config, &secrets, &registry)
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
            let registry = registry.clone();
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

                    let backend =
                        get_backend(&room, &config, &secrets, server.registry()).await;

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
                    let approval_tx = make_room_approval_tx(
                        room_id.clone(),
                        approval_relay_tx.clone(),
                    );

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
