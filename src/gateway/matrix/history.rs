use crate::session::{EntryType, SessionEntry};

use chrono::Utc;
use matrix_sdk::{
    room::MessagesOptions,
    ruma::events::room::message::{MessageType, RoomMessageEventContent},
    Room,
};

/// Read room message history as SessionEntries for backfilling.
/// Reads backward from most recent, stops at `!chaz clear` or end of history.
pub async fn read_room_history(room: &Room) -> Vec<SessionEntry> {
    let mut entries = Vec::new();
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

                    // Use event origin_server_ts if available, otherwise now
                    let timestamp = message
                        .event
                        .get_field::<u64>("origin_server_ts")
                        .unwrap_or(None)
                        .and_then(|ts| chrono::DateTime::from_timestamp_millis(ts as i64))
                        .unwrap_or_else(Utc::now);

                    entries.push(SessionEntry {
                        sender: sender.clone(),
                        content: body,
                        timestamp,
                        entry_type: EntryType::Message,
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
    entries.reverse();
    entries
}
