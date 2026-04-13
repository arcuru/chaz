use crate::backends::{BackendManager, ChatContext, Message};
use crate::config::*;
use crate::defaults::DEFAULT_CONFIG;
use crate::gateway::{ChatRequest, ChatResponse};
use crate::role::{RoleDetails, get_role, get_role_names};
use crate::types::ConversationId;

use headjack::Tags;
use headjack::*;
use matrix_sdk::{
    Room, RoomMemberships,
    media::{MediaFormat, MediaRequest},
    room::MessagesOptions,
    ruma::{
        OwnedUserId,
        events::room::message::{MessageType, RoomMessageEventContent},
    },
};
use openai_api_rs::v1::chat_completion::MessageRole;
use regex::Regex;
use std::format;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

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

    pub async fn run(self, event_tx: mpsc::Sender<ChatRequest>) -> anyhow::Result<()> {
        let config = self.config;

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

        bot.register_text_command(
            "print",
            None,
            Some("Print the conversation".to_string()),
            |_, _, room| async move {
                let context = get_context(&room).await.unwrap();
                let content = RoomMessageEventContent::notice_plain(context.string_prompt());
                room.send(content).await.unwrap();
                Ok(())
            },
        )
        .await;

        bot.register_text_command(
            "send",
            "<message>".to_string(),
            "Send a message without context".to_string(),
            |sender, text, room| async move {
                if rate_limit(&room, &sender).await {
                    return Ok(());
                }
                let input = text
                    .split_whitespace()
                    .skip(2)
                    .collect::<Vec<&str>>()
                    .join(" ");

                let context = get_context(&room).await.unwrap();
                let no_context = ChatContext {
                    messages: vec![Message::new(MessageRole::user, input.to_string())],
                    model: context.model,
                    role: context.role,
                    media: Vec::new(),
                };

                info!(
                    "Request: {} - {}",
                    sender.as_str(),
                    input.replace('\n', " ")
                );
                if let Ok(result) = get_backend(&room).await.execute(&no_context).await {
                    info!(
                        "Response: {} - {}",
                        sender.as_str(),
                        result.replace('\n', " ")
                    );
                    let content = RoomMessageEventContent::notice_plain(result.clone());
                    room.send(content).await.unwrap();
                }
                Ok(())
            },
        )
        .await;

        bot.register_text_command(
            "model",
            "<model>".to_string(),
            "Select the model to use".to_string(),
            set_model,
        )
        .await;

        bot.register_text_command(
            "backend",
            "<name> <api_base> <api_key>".to_string(),
            "Manually enter an OpenAI Compatible Backend".to_string(),
            set_backend,
        )
        .await;

        bot.register_text_command(
            "role",
            "[<role>] [<prompt>]".to_string(),
            "Get the role info, set the role, or define a new role".to_string(),
            set_role,
        )
        .await;

        bot.register_text_command(
            "list",
            "".to_string(),
            "List available models".to_string(),
            list_models,
        )
        .await;

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

        bot.register_text_command(
            "rename",
            "".to_string(),
            "Rename the room and set the topic based on the chat content".to_string(),
            rename,
        )
        .await;

        // === Text handler — routes messages through the router ===

        let tx = event_tx;
        bot.register_text_handler(move |sender, body: String, room, event| {
            let tx = tx.clone();
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

                if rate_limit(&room, &sender).await {
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
                                let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
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

                    let backend = get_backend(&room).await;
                    let (response_tx, response_rx) = oneshot::channel();

                    if tx
                        .send(ChatRequest {
                            conversation_id: ConversationId(room.room_id().to_string()),
                            sender: sender.to_string(),
                            body,
                            model_override,
                            role_override,
                            backend,
                            response_tx,
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

// === Helper functions (moved from main.rs) ===

/// Rate limit the user to a set number of messages
/// Returns true if the user is being rate limited
async fn rate_limit(room: &Room, sender: &OwnedUserId) -> bool {
    let room_size = room
        .members(RoomMemberships::ACTIVE)
        .await
        .unwrap_or(Vec::new())
        .len();
    let message_limit = GLOBAL_CONFIG
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .message_limit
        .unwrap_or(u64::MAX);
    let room_size_limit = GLOBAL_CONFIG
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .room_size_limit
        .unwrap_or(usize::MAX);
    let count = {
        let mut messages = GLOBAL_MESSAGES.lock().unwrap();
        let count = match messages.get_mut(sender.as_str()) {
            Some(count) => count,
            None => {
                messages.insert(sender.as_str().to_string(), 0);
                messages.get_mut(sender.as_str()).unwrap()
            }
        };
        if room_size > room_size_limit {
            return true;
        }
        if *count < message_limit {
            *count += 1;
            return false;
        }
        *count
    };
    error!("User {} has sent {} messages", sender, count);
    room.send(RoomMessageEventContent::notice_plain(format!(
        "!chaz Error: you have used up your message limit of {} messages.",
        message_limit
    )))
    .await
    .unwrap();
    true
}

/// List the available models
async fn list_models(_: OwnedUserId, _: String, room: Room) -> Result<(), ()> {
    let context = get_context(&room).await.unwrap();
    let backends = get_backend(&room).await;
    let response = format!(
        "!chaz Current Model: {}\n\nKnown Backends:\n{}\n\nKnown Models:\n{}",
        context
            .model
            .unwrap_or(backends.default_model().unwrap_or("unknown".to_string())),
        backends.list_known_backends().join("\n"),
        backends.list_known_models().join("\n")
    );
    room.send(RoomMessageEventContent::notice_plain(response))
        .await
        .unwrap();
    Ok(())
}

/// Control the roles
async fn set_role(_: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
    let mut words = text.split_whitespace().skip(2);
    let mut tags = Tags::new(&room, "is.chaz.role").await;
    if let Some(name) = words.next() {
        tags.replace_kv("chazdefault", name);
        if let Some(prompt) = words.next() {
            let prompt = words.fold(prompt.to_string(), |acc, x| format!("{} {}", acc, x));
            tags.replace_kv(name, &prompt);
        }
        tags.sync().await;
        room.send(RoomMessageEventContent::notice_plain(format!(
            "!chaz Role set to \"{}\"",
            name
        )))
        .await
        .unwrap();
    } else {
        let context = get_context(&room);
        let mut room_roles = Vec::new();
        for tag in tags.tags() {
            let role = tag.split('=').next().unwrap();
            if role != "chazdefault" {
                room_roles.push(role);
            }
        }
        let config_roles = {
            let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
            get_role_names(config.roles)
        };
        let default_roles = get_role_names(DEFAULT_CONFIG.roles.clone());
        let context = context.await?;
        let current_role = {
            if let Some(role) = context.role {
                role.name
            } else {
                "unknown".to_string()
            }
        };
        let mut response_parts = vec![format!("!chaz Current Role: {}", current_role)];

        if !room_roles.is_empty() {
            response_parts.push(format!(
                "\n\nRoom Defined Roles:\n{}",
                room_roles.join("\n")
            ));
        }

        if !config_roles.is_empty() {
            response_parts.push(format!(
                "\n\nConfigured Roles:\n{}",
                config_roles.join("\n")
            ));
        }

        if !default_roles.is_empty() {
            response_parts.push(format!("\n\nBuiltin Roles:\n{}", default_roles.join("\n")));
        }
        let response = response_parts.join("");
        room.send(RoomMessageEventContent::notice_plain(response))
            .await
            .unwrap();
    }
    Ok(())
}

/// Add a backend provider into the room tags
async fn set_backend(_: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
    let mut split = text.split_whitespace();
    split.next();
    split.next();
    if let (Some(name), Some(url), Some(token)) = (split.next(), split.next(), split.next()) {
        let mut tags = Tags::new(&room, "is.chaz.backend").await;
        tags.replace_kv("chazdefault", name);
        tags.replace_kv(&format!("{}.url", name), url);
        tags.replace_kv(&format!("{}.token", name), token);
        tags.sync().await;
        room.send(RoomMessageEventContent::notice_plain(format!(
            "!chaz Successfully added backend {}",
            name
        )))
        .await
        .unwrap();
    } else {
        room.send(RoomMessageEventContent::notice_plain(
            "!chaz Error: invalid arguments. Usage: !chaz backend <name> <api_base> <api_key>",
        ))
        .await
        .unwrap();
        return Ok(());
    }
    Ok(())
}

/// Set the model to use for this chat
async fn set_model(sender: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
    let model = text.split_whitespace().nth(2);
    if let Some(model) = model {
        let backend = get_backend(&room).await;
        if backend.is_known_model(model) {
            let response = format!("!chaz Model set to \"{}\"", model);
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        } else if let Err(e) = backend.validate_model(model) {
            let response = format!("!chaz Error: {}", e);
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        } else {
            let response = format!(
                "!chaz Model {} is unknown, but may be valid. Please manually verify that it is supported by your desired backend.",
                model
            );
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        }
        let mut tags = Tags::new(&room, "is.chaz.model").await;
        tags.replace_kv("default", model);
        tags.sync().await;
    } else {
        list_models(sender, text, room).await?;
    }
    Ok(())
}

async fn rename(sender: OwnedUserId, _: String, room: Room) -> Result<(), ()> {
    if rate_limit(&room, &sender).await {
        return Ok(());
    }
    if let Ok(context) = get_context(&room).await {
        let mut context = context;
        context.model = get_chat_summary_model();
        context.messages.push(Message::new(
            MessageRole::user,
            [
                "Summarize this conversation in less than 20 characters to use as the title of this conversation.",
                "The output should be a single line of text describing the conversation.",
                "Do not output anything except for the summary text.",
                "Only the first 20 characters will be used.",
            ]
            .join(" "),
        ));

        let response = get_backend(&room).await.execute(&context).await;
        if let Ok(result) = response {
            info!(
                "Response: {} - {}",
                sender.as_str(),
                result.replace('\n', " ")
            );
            let result = clean_summary_response(&result, None);
            if room.set_name(result).await.is_err() {
                room.send(RoomMessageEventContent::notice_plain(
                    "!chaz Error: I don't have permission to rename the room",
                ))
                .await
                .unwrap();

                return Ok(());
            }
        }
        context.messages.pop();

        context.model = get_chat_summary_model();
        context.messages.push(Message::new(
            MessageRole::user,
            [
                "Summarize this conversation in less than 50 characters.",
                "Do not output anything except for the summary text.",
                "Do not include any commentary or context, only the summary.",
            ]
            .join(" "),
        ));

        let response = get_backend(&room).await.execute(&context).await;
        if let Ok(result) = response {
            info!(
                "Response: {} - {}",
                sender.as_str(),
                result.replace('\n', " ")
            );
            let result = clean_summary_response(&result, None);
            if room.set_room_topic(&result).await.is_err() {
                room.send(RoomMessageEventContent::notice_plain(
                    "!chaz Error: I don't have permission to set the topic",
                ))
                .await
                .unwrap();
            }
        }
    }
    Ok(())
}

/// Get the backend defined in the room tags.
async fn get_tag_backend(room: &Room) -> Option<Vec<Backend>> {
    let mut backends = Vec::new();
    let tags = Tags::new(room, "is.chaz.backend").await;
    let default_backend = tags.get_value("chazdefault");
    for tag in tags.tags() {
        if tag.split('.').nth(1).is_some_and(|x| x.starts_with("url")) {
            let name = tag.split('.').next().unwrap_or_default();
            let mut backend = Backend::new(BackendType::OpenAICompatible);
            backend.name = Some(name.to_string());
            backend.api_base = tags.get_value(&format!("{}.url", name));
            backend.api_key = tags.get_value(&format!("{}.token", name));
            if backend.api_base.is_some() && backend.api_key.is_some() {
                backends.push(backend);
            }
        }
    }
    if let Some(default_backend) = default_backend {
        if let Some(index) = backends
            .iter()
            .position(|x| x.name == Some(default_backend.to_string()))
        {
            backends.swap(0, index);
        }
    }
    Some(backends)
}

/// Returns the backend based on the global config
async fn get_backend(room: &Room) -> BackendManager {
    let mut backends = Vec::new();
    if let Some(tag_backends) = get_tag_backend(room).await {
        backends = tag_backends;
    }
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    if let Some(config_backends) = config.backends {
        backends.extend(config_backends);
    }
    if backends.is_empty() {
        BackendManager::new(&None)
    } else {
        BackendManager::new(&Some(backends))
    }
}

/// Try to clean up the response from the model containing a summary
fn clean_summary_response(response: &str, max_length: Option<usize>) -> String {
    let response = {
        let re = Regex::new(r#""([^"]*)""#).unwrap();
        if let Some(caps) = re.captures(response) {
            caps.get(1).map_or("", |m| m.as_str())
        } else {
            response
        }
    };
    if let Some(max_length) = max_length {
        return response.chars().take(max_length).collect::<String>();
    }
    response.to_string()
}

/// Get the chat summary model from the global config
fn get_chat_summary_model() -> Option<String> {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    config.chat_summary_model
}

/// Gets the context of the current conversation
async fn get_context(room: &Room) -> Result<ChatContext, ()> {
    let mut context = ChatContext {
        messages: Vec::new(),
        model: None,
        media: Vec::new(),
        role: None,
    };
    context.role = {
        let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
        get_role(
            config.role.clone(),
            config.roles.clone(),
            DEFAULT_CONFIG.roles.clone(),
        )
    };

    let mut options = MessagesOptions::backward();

    let enable_media_context = true;

    'outer: while let Ok(batch) = room.messages(options).await {
        for message in batch.chunk {
            if let Some((sender, content)) = message
                .event
                .get_field::<String>("sender")
                .unwrap_or(None)
                .zip(
                    message
                        .event
                        .get_field::<RoomMessageEventContent>("content")
                        .unwrap_or(None),
                )
            {
                match &content.msgtype {
                    MessageType::Image(image_content) => {
                        if enable_media_context {
                            let request = MediaRequest {
                                source: image_content.source.clone(),
                                format: MediaFormat::File,
                            };
                            let mime = image_content
                                .info
                                .as_ref()
                                .unwrap()
                                .mimetype
                                .clone()
                                .unwrap()
                                .parse()
                                .unwrap();
                            let x = room
                                .client()
                                .media()
                                .get_media_file(&request, None, &mime, true, None)
                                .await
                                .unwrap();
                            context.media.push(x);
                        }
                    }
                    MessageType::Text(text_content) => {
                        if is_command("!", &text_content.body) {
                            if text_content.body.starts_with("!chaz model")
                                && context.model.is_none()
                            {
                                let model = text_content.body.split_whitespace().nth(2);
                                if let Some(model) = model {
                                    if get_backend(room).await.validate_model(model).is_ok() {
                                        context.model = Some(model.to_string());
                                    }
                                }
                            }
                            if text_content.body.starts_with("!chaz clear") {
                                break 'outer;
                            }
                            if text_content.body.starts_with("!chaz") {
                                let command = text_content.body.trim_start_matches("!chaz").trim();
                                if command.is_empty() {
                                    continue;
                                }
                                if let Some(command) = command.split_whitespace().next() {
                                    if [
                                        "help", "party", "send", "list", "rename", "print",
                                        "model", "clear",
                                    ]
                                    .contains(&command.to_lowercase().as_str())
                                    {
                                        continue;
                                    }
                                }
                                if room
                                    .client()
                                    .user_id()
                                    .is_some_and(|uid| sender == uid.as_str())
                                {
                                    context.messages.push(Message::new(
                                        MessageRole::assistant,
                                        command.to_string(),
                                    ));
                                } else {
                                    context
                                        .messages
                                        .push(Message::new(MessageRole::user, command.to_string()));
                                }
                            }
                        } else if room
                            .client()
                            .user_id()
                            .is_some_and(|uid| sender == uid.as_str())
                        {
                            context.messages.push(Message::new(
                                MessageRole::assistant,
                                text_content.body.clone(),
                            ));
                        } else {
                            context
                                .messages
                                .push(Message::new(MessageRole::user, text_content.body.clone()));
                        }
                    }
                    _ => {}
                };
            }
        }
        if let Some(token) = batch.end {
            options = MessagesOptions::backward().from(Some(token.as_str()));
        } else {
            break;
        }
    }
    let tags = Tags::new(room, "is.chaz.model").await;
    if let Some(model) = tags.get_value("default") {
        context.model = Some(model);
    }
    let tags = Tags::new(room, "is.chaz.role").await;
    if let Some(role) = tags.get_value("chazdefault") {
        if let Some(prompt) = tags.get_value(&role) {
            context.role = Some(RoleDetails::new(&role, None, Some(prompt), None));
        } else {
            context.role = {
                let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
                get_role(
                    Some(role),
                    config.roles.clone(),
                    DEFAULT_CONFIG.roles.clone(),
                )
            };
        }
    }

    context.messages.reverse();
    context.media.reverse();
    Ok(context)
}
