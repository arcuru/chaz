//! Matrix-specific gateway commands.
//!
//! Commands that have a cross-transport analogue (model/role/compact/share/…)
//! are handled by `crate::commands::dispatch`. This module keeps only the
//! Matrix-specific glue: rate limiting, backend selection for a room, legacy
//! history reconstruction for `send`/`rename`, and the `rename`/`send` bodies.

use crate::backends::{BackendManager, ChatContext, Message};
use crate::config::*;
use crate::defaults::DEFAULT_CONFIG;
use crate::role::{RoleDetails, get_role};
use crate::security::SecretStore;
use crate::session::SessionRegistry;

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

/// Send a message without context (Matrix-only legacy command).
pub async fn send(
    sender: matrix_sdk::ruma::OwnedUserId,
    text: String,
    room: matrix_sdk::Room,
    config: &Config,
    message_counts: &Mutex<HashMap<String, u64>>,
    secrets: &SecretStore,
    registry: &SessionRegistry,
) -> Result<(), ()> {
    if rate_limit(&room, &sender, config, message_counts).await {
        return Ok(());
    }
    let input = text
        .split_whitespace()
        .skip(2)
        .collect::<Vec<&str>>()
        .join(" ");

    let context = get_context(&room, config, secrets, registry).await.unwrap();
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
    if let Ok(result) = get_backend(&room, config, secrets, registry)
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

/// Rename the Matrix room and set its topic based on the conversation
/// (Matrix-only — operates on the room, not the session).
pub async fn rename(
    sender: OwnedUserId,
    _: String,
    room: Room,
    config: &Config,
    message_counts: &Mutex<HashMap<String, u64>>,
    secrets: &SecretStore,
    registry: &SessionRegistry,
) -> Result<(), ()> {
    if rate_limit(&room, &sender, config, message_counts).await {
        return Ok(());
    }
    if let Ok(context) = get_context(&room, config, secrets, registry).await {
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

        let response = get_backend(&room, config, secrets, registry)
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

        let response = get_backend(&room, config, secrets, registry)
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

/// Get the backend defined in the session binding.
fn get_binding_backend(binding: &crate::session::SessionBinding) -> Option<Backend> {
    let name = binding.backend_name.as_ref()?;
    let url = binding.backend_url.as_ref()?;
    let key_ref = binding.backend_key_ref.as_ref()?;
    let mut backend = Backend::new(BackendType::OpenAICompatible);
    backend.name = Some(name.clone());
    backend.api_base = Some(url.clone());
    backend.api_key_ref = Some(key_ref.clone());
    Some(backend)
}

/// Returns the backend based on the config and session binding
pub async fn get_backend(
    room: &Room,
    config: &Config,
    secrets: &SecretStore,
    registry: &SessionRegistry,
) -> BackendManager {
    let room_id = room.room_id().to_string();
    let mut backends = Vec::new();
    if let Some(binding) = registry.get_binding(&room_id).await {
        if let Some(backend) = get_binding_backend(&binding) {
            backends.push(backend);
        }
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
/// Used by legacy commands (send, rename) that bypass the session DB.
pub async fn get_context(
    room: &Room,
    config: &Config,
    secrets: &SecretStore,
    registry: &SessionRegistry,
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
                                if get_backend(room, config, secrets, registry)
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
    // Apply session config from registry
    let room_id = room.room_id().to_string();
    if let Some(binding) = registry.get_binding(&room_id).await {
        if let Some(model) = &binding.model {
            context.model = Some(model.clone());
        }
        if let Some(role_name) = &binding.role_name {
            if let Some(prompt) = &binding.role_prompt {
                context.role = Some(RoleDetails::new(
                    role_name,
                    None,
                    Some(prompt.clone()),
                    None,
                ));
            } else {
                context.role = get_role(
                    Some(role_name.clone()),
                    config.roles.clone(),
                    DEFAULT_CONFIG.roles.clone(),
                );
            }
        }
    }

    context.messages.reverse();
    Ok(context)
}
