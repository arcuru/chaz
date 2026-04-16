use crate::backends::{BackendManager, ChatContext, Message};
use crate::config::*;
use crate::defaults::DEFAULT_CONFIG;
use crate::role::{get_role, get_role_names, RoleDetails};
use crate::security::SecretStore;
use crate::session::SessionRegistry;

use headjack::*;
use matrix_sdk::{
    room::MessagesOptions,
    ruma::{
        events::room::message::{MessageType, RoomMessageEventContent},
        OwnedUserId,
    },
    Room, RoomMemberships,
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
    registry: &SessionRegistry,
) -> Result<(), ()> {
    let context = get_context(&room, config, secrets, registry).await.unwrap();
    let backends = get_backend(&room, config, secrets, registry).await;
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
    registry: &SessionRegistry,
) -> Result<(), ()> {
    let room_id = room.room_id().to_string();
    let mut words = text.split_whitespace().skip(2);
    if let Some(name) = words.next() {
        let custom_prompt = words
            .next()
            .map(|prompt| words.fold(prompt.to_string(), |acc, x| format!("{} {}", acc, x)));
        if let Err(e) = registry
            .update_binding(&room_id, |b| {
                b.role_name = Some(name.to_string());
                if let Some(ref prompt) = custom_prompt {
                    b.role_prompt = Some(prompt.clone());
                }
            })
            .await
        {
            error!("Failed to set role: {e}");
        }
        room.send(RoomMessageEventContent::notice_plain(format!(
            "!chaz Role set to \"{}\"",
            name
        )))
        .await
        .unwrap();
    } else {
        let context = get_context(&room, config, secrets, registry).await?;
        let config_roles = get_role_names(config.roles.clone());
        let default_roles = get_role_names(DEFAULT_CONFIG.roles.clone());
        let current_role = {
            if let Some(role) = context.role {
                role.name
            } else {
                "unknown".to_string()
            }
        };
        let mut response_parts = vec![format!("!chaz Current Role: {}", current_role)];

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

/// Add a backend provider for this session
pub async fn set_backend(
    _: OwnedUserId,
    text: String,
    room: Room,
    secrets: &SecretStore,
    registry: &SessionRegistry,
) -> Result<(), ()> {
    let room_id = room.room_id().to_string();
    let mut split = text.split_whitespace();
    split.next();
    split.next();
    if let (Some(name), Some(url), Some(token)) = (split.next(), split.next(), split.next()) {
        let ref_id = format!("session:{room_id}:{name}");
        secrets.insert(ref_id.clone(), token.to_string()).await;
        if let Err(e) = registry
            .update_binding(&room_id, |b| {
                b.backend_name = Some(name.to_string());
                b.backend_url = Some(url.to_string());
                b.backend_key_ref = Some(ref_id.clone());
            })
            .await
        {
            error!("Failed to set backend: {e}");
        }
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
    }
    Ok(())
}

/// Set the model to use for this chat
pub async fn set_model(
    sender: OwnedUserId,
    text: String,
    room: Room,
    secrets: &SecretStore,
    registry: &SessionRegistry,
) -> Result<(), ()> {
    let room_id = room.room_id().to_string();
    let model = text.split_whitespace().nth(2);
    if let Some(model) = model {
        let backend = get_backend_from_binding(&room, secrets, registry).await;
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
        if let Err(e) = registry
            .update_binding(&room_id, |b| {
                b.model = Some(model.to_string());
            })
            .await
        {
            error!("Failed to set model: {e}");
        }
    } else {
        list_models_default(sender, text, room, secrets, registry).await?;
    }
    Ok(())
}

/// List models fallback for set_model (no config available in old-style command handler)
async fn list_models_default(
    _: OwnedUserId,
    _: String,
    room: Room,
    secrets: &SecretStore,
    registry: &SessionRegistry,
) -> Result<(), ()> {
    let backends = get_backend_from_binding(&room, secrets, registry).await;
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

/// Returns the backend from session binding only (for commands without config access)
async fn get_backend_from_binding(
    room: &Room,
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
                context.role = Some(RoleDetails::new(role_name, None, Some(prompt.clone()), None));
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
