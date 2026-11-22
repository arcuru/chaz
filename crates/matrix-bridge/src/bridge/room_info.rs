//! Passive room facts for an already routed Matrix turn. Never fetch state.

use chaz_core::session::MatrixRoomObservation;
use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::{EncryptionState, Room, RoomState};
use serde_json::Value;

pub(super) async fn observe(room: &Room, workspace_chat: bool) -> Option<MatrixRoomObservation> {
    if room.state() != RoomState::Joined {
        return None;
    }
    let name = state_string(room, StateEventType::RoomName, "name").await;
    let join_rule = state_string(room, StateEventType::RoomJoinRules, "join_rule").await;
    let history_visibility = state_string(
        room,
        StateEventType::RoomHistoryVisibility,
        "history_visibility",
    )
    .await;
    let encrypted = match room.encryption_state() {
        EncryptionState::Encrypted => Some(true),
        EncryptionState::NotEncrypted => Some(false),
        _ => None,
    };
    (room.state() == RoomState::Joined).then(|| MatrixRoomObservation {
        name: name.filter(|name| !name.trim().is_empty() && name.len() <= 255),
        workspace_chat,
        join_rule: join_rule.filter(|rule| {
            matches!(
                rule.as_str(),
                "public" | "invite" | "knock" | "restricted" | "knock_restricted" | "private"
            )
        }),
        history_visibility: history_visibility.filter(|visibility| {
            matches!(
                visibility.as_str(),
                "world_readable" | "shared" | "invited" | "joined"
            )
        }),
        encrypted,
    })
}

async fn state_string(room: &Room, event_type: StateEventType, field: &str) -> Option<String> {
    // Display-name helpers can synthesize a name from members. Read the
    // literal state event instead and don't substitute Matrix defaults.
    let RawAnySyncOrStrippedState::Sync(event) =
        room.get_state_event(event_type, "").await.ok()??
    else {
        return None;
    };
    if event
        .get_field::<Value>("unsigned")
        .ok()?
        .is_some_and(|unsigned| unsigned.get("redacted_because").is_some())
    {
        return None;
    }
    let content = event.get_field::<Value>("content").ok()??;
    Some(content.get(field)?.as_str()?.to_owned())
}
