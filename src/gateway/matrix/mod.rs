mod commands;
mod history;

use crate::config::Config;
use crate::gateway::Gateway;
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};

use headjack::Tags;
use headjack::*;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};

use commands::{get_backend, get_context, rate_limit};
use history::read_room_history;

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

        {
            let config = config.clone();
            let secrets = self.secrets.clone();
            bot.register_text_command(
                "print",
                None,
                Some("Print the conversation".to_string()),
                move |_, _, room| {
                    let config = config.clone();
                    let secrets = secrets.clone();
                    async move {
                        let context = get_context(&room, &config, &secrets).await.unwrap();
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
            bot.register_text_command(
                "send",
                "<message>".to_string(),
                "Send a message without context".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let counts = counts.clone();
                    let secrets = secrets.clone();
                    async move { commands::send(sender, text, room, &config, &counts, &secrets).await }
                },
            )
            .await;
        }

        {
            let secrets = self.secrets.clone();
            bot.register_text_command(
                "model",
                "<model>".to_string(),
                "Select the model to use".to_string(),
                move |sender, text, room| {
                    let secrets = secrets.clone();
                    async move { commands::set_model(sender, text, room, &secrets).await }
                },
            )
            .await;
        }

        bot.register_text_command(
            "backend",
            "<name> <api_base> <api_key>".to_string(),
            "Manually enter an OpenAI Compatible Backend".to_string(),
            commands::set_backend,
        )
        .await;

        {
            let config = config.clone();
            let secrets = self.secrets.clone();
            bot.register_text_command(
                "role",
                "[<role>] [<prompt>]".to_string(),
                "Get the role info, set the role, or define a new role".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let secrets = secrets.clone();
                    async move { commands::set_role(sender, text, room, &config, &secrets).await }
                },
            )
            .await;
        }

        {
            let config = config.clone();
            let secrets = self.secrets.clone();
            bot.register_text_command(
                "list",
                "".to_string(),
                "List available models".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let secrets = secrets.clone();
                    async move { commands::list_models(sender, text, room, &config, &secrets).await }
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
            bot.register_text_command(
                "rename",
                "".to_string(),
                "Rename the room and set the topic based on the chat content".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let counts = counts.clone();
                    let secrets = secrets.clone();
                    async move { commands::rename(sender, text, room, &config, &counts, &secrets).await }
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
            let backfilled_rooms: Arc<Mutex<HashSet<String>>> =
                Arc::new(Mutex::new(HashSet::new()));
            let seen_events: Arc<Mutex<HashSet<String>>> =
                Arc::new(Mutex::new(HashSet::new()));
            bot.register_text_handler(move |sender, body: String, room, event| {
                let config = config.clone();
                let backfilled_rooms = backfilled_rooms.clone();
                let seen_events = seen_events.clone();
                let counts = counts.clone();
                let secrets = secrets.clone();
                let server = server.clone();
                let gateway_watched = gateway_watched.clone();
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

                    let agent_override = {
                        let tags = Tags::new(&room, "is.chaz.agent").await;
                        tags.get_value("default")
                    };

                    let body = if body.starts_with("!chaz") {
                        body.trim_start_matches("!chaz").trim().to_string()
                    } else {
                        body
                    };

                    let backend = get_backend(&room, &config, &secrets).await;
                    let room_id = room.room_id().to_string();

                    // Get or create session DB
                    let (_conv_id, session_db) = match server
                        .registry()
                        .get_or_create_session_db(&room_id)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            error!("Failed to get session DB: {e}");
                            return Ok(());
                        }
                    };

                    // Register server callback (agent processing)
                    if let Err(e) = server
                        .register_session(
                            &room_id,
                            &session_db,
                            backend,
                            agent_override,
                            None, // Matrix approval UX deferred
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
                            if let Err(e) = session_db.on_local_write(
                                move |_entry, db, _instance| {
                                    let matrix_room = matrix_room.clone();
                                    let agents = agents.clone();
                                    let db = db.clone();
                                    let rid = rid.clone();
                                    Box::pin(async move {
                                        // Read latest entry
                                        let session = Session::new(
                                            crate::types::ConversationId(rid),
                                            db,
                                        )
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
                                },
                            ) {
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
                    let mut session = Session::new(
                        crate::types::ConversationId(room_id),
                        session_db,
                    )
                    .await;
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
