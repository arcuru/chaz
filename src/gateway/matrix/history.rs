use crate::session::SessionMessage;

use chrono::Utc;
use matrix_sdk::{
    Room,
    room::MessagesOptions,
    ruma::events::room::message::{MessageType, RoomMessageEventContent},
};

/// Read room message history as SessionMessages for backfilling.
/// Reads backward from most recent, stops at `!chaz clear` or end of history.
pub async fn read_room_history(room: &Room) -> Vec<SessionMessage> {
    let mut messages = Vec::new();
    let bot_user_id = room
        .client()
        .user_id()
        .map(|uid: &matrix_sdk::ruma::UserId| uid.to_string());
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
                    // Stop at !chaz clear
                    if text_content.body.starts_with("!chaz clear") {
                        break 'outer;
                    }
                    // Skip chaz commands that aren't meaningful conversation
                    if text_content.body.starts_with("!chaz") {
                        let command = text_content.body.trim_start_matches("!chaz").trim();
                        if command.is_empty() {
                            continue;
                        }
                        if let Some(cmd) = command.split_whitespace().next() {
                            if [
                                "help", "party", "send", "list", "rename", "print", "model",
                                "clear", "backend", "role",
                            ]
                            .contains(&cmd.to_lowercase().as_str())
                            {
                                continue;
                            }
                        }
                    }

                    let body = if text_content.body.starts_with("!chaz") {
                        text_content
                            .body
                            .trim_start_matches("!chaz")
                            .trim()
                            .to_string()
                    } else {
                        text_content.body.clone()
                    };

                    let role = if bot_user_id.as_ref().is_some_and(|uid| sender == *uid) {
                        "assistant"
                    } else {
                        "user"
                    };

                    // Use event origin_server_ts if available, otherwise now
                    let timestamp = message
                        .event
                        .get_field::<u64>("origin_server_ts")
                        .unwrap_or(None)
                        .and_then(|ts| chrono::DateTime::from_timestamp_millis(ts as i64))
                        .unwrap_or_else(Utc::now);

                    messages.push(SessionMessage {
                        role: role.to_string(),
                        content: body,
                        sender: sender.clone(),
                        timestamp,
                    });
                }
            }
        }
        if let Some(token) = batch.end {
            options = MessagesOptions::backward().from(Some(token.as_str()));
        } else {
            break;
        }
    }

    // Reverse to chronological order (we read backward)
    messages.reverse();
    messages
}
