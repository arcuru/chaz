mod commands;
mod history;

use crate::config::Config;
use crate::defaults::DEFAULT_CONFIG;
use crate::gateway::{ChatRequest, ChatResponse, Gateway};
use crate::role::{RoleDetails, get_role};

use headjack::Tags;
use headjack::*;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{error, info};

use commands::{get_backend, get_context, rate_limit};
use history::read_room_history;

pub struct MatrixGateway {
    config: Config,
}

impl MatrixGateway {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        if config.homeserver_url.is_empty() {
            anyhow::bail!("homeserver_url is required for Matrix gateway");
        }
        if config.username.is_empty() {
            anyhow::bail!("username is required for Matrix gateway");
        }
        Ok(Self { config })
    }
}

impl Gateway for MatrixGateway {
    async fn run(self, event_tx: mpsc::Sender<ChatRequest>) -> anyhow::Result<()> {
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

        // React to invites before initial sync so we join rooms
        // even if they were invited before the bot was started.
        bot.join_rooms();

        if let Err(e) = bot.sync().await {
            info!("Error syncing: {e}");
        }

        info!("The client is ready! Listening to new messages…");

        // === Register commands (handled directly, not routed through router) ===

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
            bot.register_text_command(
                "print",
                None,
                Some("Print the conversation".to_string()),
                move |_, _, room| {
                    let config = config.clone();
                    async move {
                        let context = get_context(&room, &config).await.unwrap();
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
            bot.register_text_command(
                "send",
                "<message>".to_string(),
                "Send a message without context".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let counts = counts.clone();
                    async move { commands::send(sender, text, room, &config, &counts).await }
                },
            )
            .await;
        }

        bot.register_text_command(
            "model",
            "<model>".to_string(),
            "Select the model to use".to_string(),
            commands::set_model,
        )
        .await;

        bot.register_text_command(
            "backend",
            "<name> <api_base> <api_key>".to_string(),
            "Manually enter an OpenAI Compatible Backend".to_string(),
            commands::set_backend,
        )
        .await;

        {
            let config = config.clone();
            bot.register_text_command(
                "role",
                "[<role>] [<prompt>]".to_string(),
                "Get the role info, set the role, or define a new role".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    async move { commands::set_role(sender, text, room, &config).await }
                },
            )
            .await;
        }

        {
            let config = config.clone();
            bot.register_text_command(
                "list",
                "".to_string(),
                "List available models".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    async move { commands::list_models(sender, text, room, &config).await }
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
            bot.register_text_command(
                "rename",
                "".to_string(),
                "Rename the room and set the topic based on the chat content".to_string(),
                move |sender, text, room| {
                    let config = config.clone();
                    let counts = counts.clone();
                    async move { commands::rename(sender, text, room, &config, &counts).await }
                },
            )
            .await;
        }

        // === Text handler — routes messages through the router ===

        {
            let tx = event_tx;
            let config = config.clone();
            let counts = message_counts;
            let backfilled_rooms: Arc<Mutex<HashSet<String>>> =
                Arc::new(Mutex::new(HashSet::new()));
            bot.register_text_handler(move |sender, body: String, room, event| {
                let tx = tx.clone();
                let config = config.clone();
                let backfilled_rooms = backfilled_rooms.clone();
                let counts = counts.clone();
                async move {
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

                    {
                        // Read model/role overrides from room tags
                        let model_override = {
                            let tags = Tags::new(&room, "is.chaz.model").await;
                            tags.get_value("default")
                        };
                        let role_override = {
                            let tags = Tags::new(&room, "is.chaz.role").await;
                            if let Some(role_name) = tags.get_value("chazdefault") {
                                if let Some(prompt) = tags.get_value(&role_name) {
                                    Some(RoleDetails::new(&role_name, None, Some(prompt), None))
                                } else {
                                    get_role(
                                        Some(role_name),
                                        config.roles.clone(),
                                        DEFAULT_CONFIG.roles.clone(),
                                    )
                                }
                            } else {
                                None
                            }
                        };

                        // Strip !chaz prefix if present (it's just a trigger, not part of the message)
                        let body = if body.starts_with("!chaz") {
                            body.trim_start_matches("!chaz").trim().to_string()
                        } else {
                            body
                        };

                        let backend = get_backend(&room, &config).await;
                        let (response_tx, response_rx) = oneshot::channel();

                        // Backfill room history on first message per room
                        let room_id = room.room_id().to_string();
                        let backfill_history = {
                            let mut rooms = backfilled_rooms.lock().await;
                            if rooms.insert(room_id.clone()) {
                                // First time seeing this room — read history
                                info!("Backfilling history for room {}", room_id);
                                Some(read_room_history(&room).await)
                            } else {
                                None
                            }
                        };

                        if tx
                            .send(ChatRequest {
                                transport_id: room_id,
                                sender: sender.to_string(),
                                body,
                                model_override,
                                role_override,
                                backend,
                                response_tx,
                                backfill_history,
                            })
                            .await
                            .is_err()
                        {
                            error!("Router channel closed");
                            return Ok(());
                        }

                        match response_rx.await {
                            Ok(ChatResponse::Message { body, is_markdown }) => {
                                info!("Response: {}", body.replace('\n', " "));
                                if is_markdown {
                                    room.send(RoomMessageEventContent::text_markdown(body))
                                        .await
                                        .unwrap();
                                } else {
                                    room.send(RoomMessageEventContent::notice_plain(body))
                                        .await
                                        .unwrap();
                                }
                            }
                            Ok(ChatResponse::Error { error }) => {
                                let err = format!("!chaz Error: {}", error.replace('\n', " "));
                                tracing::error!("{}", err);
                                room.send(RoomMessageEventContent::notice_plain(err))
                                    .await
                                    .unwrap();
                            }
                            Err(_) => error!("Router dropped response channel"),
                        }
                    }
                    Ok(())
                }
            });
        }

        // Headjack's run() doesn't retry on transient sync errors (timeouts,
        // network blips, server errors). Wrap in a retry loop so the bot stays alive.
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
