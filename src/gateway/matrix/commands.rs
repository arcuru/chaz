use crate::backends::{BackendManager, ChatContext, Message};
use crate::config::*;
use crate::defaults::DEFAULT_CONFIG;
use crate::role::{RoleDetails, get_role, get_role_names};
use crate::security::SecretStore;

use headjack::Tags;
use headjack::*;
use matrix_sdk::{
    Room, RoomMemberships,
    room::MessagesOptions,
    ruma::{
        OwnedUserId,
        events::room::message::{MessageType, RoomMessageEventContent},
    },
};
use openai_api_rs::v1::chat_completion::MessageRole;
use regex::Regex;
use std::collections::HashMap;
use tokio::sync::Mutex;
use tracing::{error, info};

/// Rate limit the user to a set number of messages.
/// Returns true if the user is being rate limited.
pub async fn rate_limit(
    room: &Room,
    sender: &OwnedUserId,
    config: &Config,
    message_counts: &Mutex<HashMap<String, u64>>,
) -> bool {
    let room_size = room
        .members(RoomMemberships::ACTIVE)
        .await
        .unwrap_or(Vec::new())
        .len();
    let message_limit = config.message_limit.unwrap_or(u64::MAX);
    let room_size_limit = config.room_size_limit.unwrap_or(usize::MAX);
    let count = {
        let mut messages = message_counts.lock().await;
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
pub async fn list_models(
    _: OwnedUserId,
    _: String,
    room: Room,
    config: &Config,
    secrets: &SecretStore,
) -> Result<(), ()> {
    let context = get_context(&room, config, secrets).await.unwrap();
    let backends = get_backend(&room, config, secrets).await;
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
pub async fn set_role(
    _: OwnedUserId,
    text: String,
    room: Room,
    config: &Config,
    secrets: &SecretStore,
) -> Result<(), ()> {
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
        let context = get_context(&room, config, secrets);
        let mut room_roles = Vec::new();
        for tag in tags.tags() {
            let role = tag.split('=').next().unwrap();
            if role != "chazdefault" {
                room_roles.push(role);
            }
        }
        let config_roles = get_role_names(config.roles.clone());
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

/// Send a message without context
pub async fn send(
    sender: matrix_sdk::ruma::OwnedUserId,
    text: String,
    room: matrix_sdk::Room,
    config: &Config,
    message_counts: &Mutex<HashMap<String, u64>>,
    secrets: &SecretStore,
) -> Result<(), ()> {
    if rate_limit(&room, &sender, config, message_counts).await {
        return Ok(());
    }
    let input = text
        .split_whitespace()
        .skip(2)
        .collect::<Vec<&str>>()
        .join(" ");

    let context = get_context(&room, config, secrets).await.unwrap();
    let no_context = ChatContext {
        messages: vec![Message::new(MessageRole::user, input.to_string())],
        model: context.model,
        role: context.role,
    };

    info!(
        "Request: {} - {}",
        sender.as_str(),
        input.replace('\n', " ")
    );
    if let Ok(result) = get_backend(&room, config, secrets)
        .await
        .execute(&no_context)
        .await
    {
        info!(
            "Response: {} - {}",
            sender.as_str(),
            result.replace('\n', " ")
        );
        let content = RoomMessageEventContent::notice_plain(result.clone());
        room.send(content).await.unwrap();
    }
    Ok(())
}

/// Add a backend provider into the room tags
pub async fn set_backend(_: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
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
pub async fn set_model(
    sender: OwnedUserId,
    text: String,
    room: Room,
    secrets: &SecretStore,
) -> Result<(), ()> {
    let model = text.split_whitespace().nth(2);
    if let Some(model) = model {
        let backend = get_backend_default(&room, secrets).await;
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
        list_models_default(sender, text, room, secrets).await?;
    }
    Ok(())
}

/// List models fallback for set_model (no config available in old-style command handler)
async fn list_models_default(
    _: OwnedUserId,
    _: String,
    room: Room,
    secrets: &SecretStore,
) -> Result<(), ()> {
    let backends = get_backend_default(&room, secrets).await;
    let response = format!(
        "!chaz Known Backends:\n{}\n\nKnown Models:\n{}",
        backends.list_known_backends().join("\n"),
        backends.list_known_models().join("\n")
    );
    room.send(RoomMessageEventContent::notice_plain(response))
        .await
        .unwrap();
    Ok(())
}

pub async fn rename(
    sender: OwnedUserId,
    _: String,
    room: Room,
    config: &Config,
    message_counts: &Mutex<HashMap<String, u64>>,
    secrets: &SecretStore,
) -> Result<(), ()> {
    if rate_limit(&room, &sender, config, message_counts).await {
        return Ok(());
    }
    if let Ok(context) = get_context(&room, config, secrets).await {
        let mut context = context;
        context.model = config.chat_summary_model.clone();
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

        let response = get_backend(&room, config, secrets)
            .await
            .execute(&context)
            .await;
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

        context.model = config.chat_summary_model.clone();
        context.messages.push(Message::new(
            MessageRole::user,
            [
                "Summarize this conversation in less than 50 characters.",
                "Do not output anything except for the summary text.",
                "Do not include any commentary or context, only the summary.",
            ]
            .join(" "),
        ));

        let response = get_backend(&room, config, secrets)
            .await
            .execute(&context)
            .await;
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
/// Extracts API keys from tags into the SecretStore, keeping them out of Backend structs.
async fn get_tag_backend(room: &Room, secrets: &SecretStore) -> Option<Vec<Backend>> {
    let mut backends = Vec::new();
    let tags = Tags::new(room, "is.chaz.backend").await;
    let default_backend = tags.get_value("chazdefault");
    let room_id = room.room_id().as_str();
    for tag in tags.tags() {
        if tag.split('.').nth(1).is_some_and(|x| x.starts_with("url")) {
            let name = tag.split('.').next().unwrap_or_default();
            let mut backend = Backend::new(BackendType::OpenAICompatible);
            backend.name = Some(name.to_string());
            backend.api_base = tags.get_value(&format!("{}.url", name));
            // Extract API key into SecretStore instead of keeping on Backend struct
            if let Some(key) = tags.get_value(&format!("{}.token", name)) {
                let ref_id = format!("room:{room_id}:{name}");
                secrets.insert(ref_id.clone(), key).await;
                backend.api_key_ref = Some(ref_id);
            }
            if backend.api_base.is_some() && backend.api_key_ref.is_some() {
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

/// Returns the backend based on the config
pub async fn get_backend(room: &Room, config: &Config, secrets: &SecretStore) -> BackendManager {
    let mut backends = Vec::new();
    if let Some(tag_backends) = get_tag_backend(room, secrets).await {
        backends = tag_backends;
    }
    if let Some(config_backends) = &config.backends {
        backends.extend(config_backends.clone());
    }
    if backends.is_empty() {
        BackendManager::new(&None, secrets.clone())
    } else {
        BackendManager::new(&Some(backends), secrets.clone())
    }
}

/// Returns the backend from room tags only (for commands without config access)
async fn get_backend_default(room: &Room, secrets: &SecretStore) -> BackendManager {
    let mut backends = Vec::new();
    if let Some(tag_backends) = get_tag_backend(room, secrets).await {
        backends = tag_backends;
    }
    if backends.is_empty() {
        BackendManager::new(&None, secrets.clone())
    } else {
        BackendManager::new(&Some(backends), secrets.clone())
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

/// Gets the context of the current conversation from Matrix room history.
/// Used by legacy commands (print, send, rename, role, list) that bypass the router.
pub async fn get_context(
    room: &Room,
    config: &Config,
    secrets: &SecretStore,
) -> Result<ChatContext, ()> {
    let mut context = ChatContext {
        messages: Vec::new(),
        model: None,
        role: None,
    };
    context.role = get_role(
        config.role.clone(),
        config.roles.clone(),
        DEFAULT_CONFIG.roles.clone(),
    );

    let mut options = MessagesOptions::backward();

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
                if let MessageType::Text(text_content) = &content.msgtype {
                    if is_command("!", &text_content.body) {
                        if text_content.body.starts_with("!chaz model") && context.model.is_none() {
                            let model = text_content.body.split_whitespace().nth(2);
                            if let Some(model) = model {
                                if get_backend(room, config, secrets)
                                    .await
                                    .validate_model(model)
                                    .is_ok()
                                {
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
                                    "help", "party", "send", "list", "rename", "print", "model",
                                    "clear",
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
            context.role = get_role(
                Some(role),
                config.roles.clone(),
                DEFAULT_CONFIG.roles.clone(),
            );
        }
    }

    context.messages.reverse();
    Ok(context)
}
