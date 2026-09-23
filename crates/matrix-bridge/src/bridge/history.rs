use chaz_core::bridge::inbound_user_entry;
use chaz_core::session::SessionEntry;

use chrono::Utc;
use matrix_sdk::{
    Room,
    room::MessagesOptions,
    ruma::events::room::message::{MessageType, RoomMessageEventContent},
};
use std::collections::HashSet;

/// Read messages strictly before this login's join event, stopping at the
/// prior leave on a rejoin. Keep event IDs past a clear marker too: the sync
/// loop may redeliver those pre-join events after the room is joined again.
/// If the join/leave boundary cannot be found, fail closed.
pub async fn read_room_history(
    room: &Room,
    join_event_id: &str,
    previous_join_id: Option<&str>,
    bot_user_id: &str,
    login_id: &str,
) -> anyhow::Result<(Vec<SessionEntry>, HashSet<String>)> {
    let mut entries = Vec::new();
    let mut prejoin_ids = HashSet::new();
    let mut importing = true;
    let mut options = MessagesOptions::backward();
    let mut past_join = false;
    let mut found_join = false;
    let mut found_leave = previous_join_id.is_none();

    'outer: loop {
        let batch = room.messages(options).await?;
        for message in batch.chunk {
            let raw = message.raw();
            let event_id = raw.get_field::<String>("event_id")?.unwrap_or_default();
            if !past_join {
                if event_id == join_event_id {
                    past_join = true;
                    found_join = true;
                }
                continue;
            }
            prejoin_ids.insert(event_id.clone());
            if previous_join_id == Some(event_id.as_str()) {
                break 'outer;
            }
            if !found_leave
                && raw.get_field::<String>("type")?.as_deref() == Some("m.room.member")
                && raw.get_field::<String>("state_key")?.as_deref() == Some(bot_user_id)
                && raw
                    .get_field::<serde_json::Value>("content")?
                    .and_then(|content| content.get("membership")?.as_str().map(str::to_owned))
                    .as_deref()
                    == Some("leave")
            {
                found_leave = true;
                break 'outer;
            }
            if !importing {
                continue;
            }
            if let Some((sender, content)) = raw.get_field::<String>("sender")?.zip(
                raw.get_field::<RoomMessageEventContent>("content")
                    .unwrap_or(None),
            ) {
                // TODO(multimodal): backfill skips non-text events (images,
                // files, etc.). See docs/src/user_guide/matrix.md "Limitations".
                if let MessageType::Text(text_content) = &content.msgtype {
                    if text_content.body.starts_with("!chaz clear") {
                        // A clear marker is a safe lower bound even when a
                        // previous leave event lies further back.
                        importing = false;
                        continue;
                    }
                    if text_content.body.starts_with("!chaz") {
                        let command = text_content.body.trim_start_matches("!chaz").trim();
                        if command.is_empty() {
                            continue;
                        }
                        if let Some(cmd) = command.split_whitespace().next()
                            && [
                                "help", "party", "send", "list", "rename", "print", "model",
                                "clear", "backend", "role",
                            ]
                            .contains(&cmd.to_lowercase().as_str())
                        {
                            continue;
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
                    let timestamp = raw
                        .get_field::<u64>("origin_server_ts")
                        .unwrap_or(None)
                        .and_then(|ts| chrono::DateTime::from_timestamp_millis(ts as i64))
                        .unwrap_or_else(Utc::now);

                    let mut entry = inbound_user_entry(
                        "matrix",
                        login_id,
                        room.room_id().as_str(),
                        &sender,
                        None,
                        &body,
                        Some(event_id.clone()),
                    );
                    entry.timestamp = timestamp;
                    entries.push(entry);
                }
            }
        }
        if let Some(token) = batch.end {
            options = MessagesOptions::backward().from(Some(token.as_str()));
        } else {
            break;
        }
    }

    anyhow::ensure!(
        found_join,
        "join event {join_event_id} not found in room history"
    );
    anyhow::ensure!(found_leave, "leave event not found before rejoin");
    entries.reverse();
    Ok((entries, prejoin_ids))
}
