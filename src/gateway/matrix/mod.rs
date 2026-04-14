mod commands;
mod history;

use crate::config::Config;
use crate::gateway::Gateway;
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, SessionEntry};

use headjack::Tags;
use headjack::*;
use matrix_sdk::Room as MatrixRoom;
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

        // Shared rate limiting state across all handlers
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

        // === Text handler — writes entries to session DB, server processes via callbacks ===

        // Room handle cache for response delivery
        let rooms: Arc<Mutex<HashMap<String, MatrixRoom>>> =
            Arc::new(Mutex::new(HashMap::new()));

        {
            let config = config.clone();
            let counts = message_counts;
            let secrets = self.secrets.clone();
            let server = server.clone();
            let rooms = rooms.clone();
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
                let rooms = rooms.clone();
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

                    // Read agent/model/role overrides from room tags
                    let agent_override = {
                        let tags = Tags::new(&room, "is.chaz.agent").await;
                        tags.get_value("default")
                    };

                    // Strip !chaz prefix if present
                    let body = if body.starts_with("!chaz") {
                        body.trim_start_matches("!chaz").trim().to_string()
                    } else {
                        body
                    };

                    let backend = get_backend(&room, &config, &secrets).await;
                    let room_id = room.room_id().to_string();

                    // Cache room handle for response delivery
                    rooms.lock().await.insert(room_id.clone(), room.clone());

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

                    // Register with server (ensures callback is set up)
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

                    // Backfill room history on first message per room
                    {
                        let mut backfilled = backfilled_rooms.lock().await;
                        if backfilled.insert(room_id.clone()) {
                            info!("Backfilling history for room {}", room_id);
                            let history = read_room_history(&room).await;
                            let mut session = crate::session::Session::new(
                                crate::types::ConversationId(room_id.clone()),
                                session_db.clone(),
                            )
                            .await;
                            session.backfill(history).await;
                        }
                    }

                    // Write user entry to session DB — this triggers the callback
                    // which causes the server to run the agent
                    let mut session = crate::session::Session::new(
                        crate::types::ConversationId(room_id.clone()),
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

                    // Response delivery happens asynchronously via the response channel
                    Ok(())
                }
            });
        }

        // === Response delivery task — reads agent responses and sends to Matrix rooms ===

        let response_rooms = rooms;
        tokio::spawn(async move {
            let mut response_rx = server.take_response_rx().await;
            while let Some(delivery) = response_rx.recv().await {
                let rooms = response_rooms.lock().await;
                if let Some(room) = rooms.get(&delivery.transport_id) {
                    info!("Response: {}", delivery.body.replace('\n', " "));
                    if delivery.is_markdown {
                        room.send(RoomMessageEventContent::text_markdown(&delivery.body))
                            .await
                            .unwrap();
                    } else {
                        room.send(RoomMessageEventContent::notice_plain(&delivery.body))
                            .await
                            .unwrap();
                    }
                } else {
                    error!(
                        "No room handle for transport_id {}",
                        delivery.transport_id
                    );
                }
            }
        });

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
