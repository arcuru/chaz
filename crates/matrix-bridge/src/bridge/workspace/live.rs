//! Matrix SDK and session-registry implementations of the workspace effects.

use std::sync::Arc;
use std::time::Duration;

use chaz_core::server::Server;
use matrix_sdk::config::RequestConfig;
use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
use matrix_sdk::ruma::api::client::discovery::get_capabilities;
use matrix_sdk::ruma::api::client::room::Visibility;
use matrix_sdk::ruma::api::client::room::create_room::v3::{Request as CreateRoom, RoomPreset};
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedRoomId, RoomVersionId, UserId};
use matrix_sdk::{Client, Room, RoomMemberships};
use serde_json::json;

use super::{CreateError, MARKER_TYPE, Marker, MatrixOps, Role, RoomSpec, SessionOps};
use crate::bridge::{AttachedChannels, PendingApprovals, attach_once, owning_agent_entry};

/// How long to wait for a just-created room to arrive through sync before a
/// follow-up step reports it as not visible yet.
const ROOM_ECHO_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the `createRoom` body for a workspace room. Privacy settings are set
/// explicitly rather than trusted to a preset: invite-only, joined-only
/// history, no guests, unlisted, and encryption from creation for every room
/// that carries conversation (Lobby and chats). Nobody is invited here;
/// invitations are separate steps after the room is ready.
pub(crate) fn create_request(spec: &RoomSpec, bot: &str) -> anyhow::Result<CreateRoom> {
    let version = RoomVersionId::try_from(spec.room_version.as_str())?;
    let mut request = CreateRoom::new();
    request.name = Some(spec.name.clone());
    request.topic = spec.topic.clone();
    request.preset = Some(RoomPreset::PrivateChat);
    request.visibility = Visibility::Private;
    request.is_direct = false;
    request.room_version = Some(version);
    // A homeserver may publish the creator's join before applying initial_state
    // or returning createRoom. Mark genesis too, so bootstrap cannot win that
    // window and create an unrelated session. This marker only excludes rooms;
    // it never authorizes enrollment or identifies a session.
    let mut creation = json!({MARKER_TYPE: spec.marker});
    if spec.role == Role::Space {
        creation["type"] = json!("m.space");
    }
    request.creation_content = Some(Raw::from_json_string(creation.to_string())?);
    let mut state = vec![
        json!({"type": "m.room.join_rules", "state_key": "",
               "content": {"join_rule": "invite"}}),
        json!({"type": "m.room.history_visibility", "state_key": "",
               "content": {"history_visibility": "joined"}}),
        json!({"type": "m.room.guest_access", "state_key": "",
               "content": {"guest_access": "forbidden"}}),
        json!({"type": MARKER_TYPE, "state_key": "", "content": spec.marker}),
    ];
    if spec.role != Role::Space {
        state.push(json!({"type": "m.room.encryption", "state_key": "",
                          "content": {"algorithm": "m.megolm.v1.aes-sha2"}}));
    }
    request.initial_state = state
        .into_iter()
        .map(|event| Raw::from_json_string(event.to_string()))
        .collect::<Result<_, _>>()?;
    // The manager gets admin power. Room v12+ creators hold implicit
    // unlimited power and must not appear in `users`; before v12 the override
    // replaces the default map, so the bot's own 100 is restated.
    let mut users = serde_json::Map::new();
    users.insert(spec.manager.clone(), json!(100));
    if spec.room_version.parse::<u32>().is_ok_and(|n| n < 12) {
        users.insert(bot.to_string(), json!(100));
    }
    request.power_level_content_override = Some(Raw::from_json_string(
        json!({ "users": users }).to_string(),
    )?);
    Ok(request)
}

pub(crate) struct LiveMatrix {
    client: Client,
}

impl LiveMatrix {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    fn bot(&self) -> anyhow::Result<&UserId> {
        self.client
            .user_id()
            .ok_or_else(|| anyhow::anyhow!("not logged in"))
    }

    async fn room(&self, room_id: &str) -> anyhow::Result<Room> {
        let id = OwnedRoomId::try_from(room_id)?;
        if let Some(room) = self.client.get_room(&id) {
            return Ok(room);
        }
        tokio::time::timeout(ROOM_ECHO_TIMEOUT, self.client.await_room_remote_echo(&id))
            .await
            .map_err(|_| anyhow::anyhow!("room {room_id} is not visible through sync yet"))
    }

    fn via(&self) -> anyhow::Result<String> {
        Ok(self.bot()?.server_name().to_string())
    }
}

async fn room_marker(room: &Room) -> Option<Marker> {
    let raw = room
        .get_state_event(StateEventType::from(MARKER_TYPE), "")
        .await
        .ok()??;
    let content = match raw {
        RawAnySyncOrStrippedState::Sync(raw) => raw.get_field::<serde_json::Value>("content"),
        RawAnySyncOrStrippedState::Stripped(raw) => raw.get_field::<serde_json::Value>("content"),
    }
    .ok()??;
    serde_json::from_value(content).ok()
}

/// Whether a joined room carries any workspace marker in the local state
/// cache. Used to keep such a room out of chat dispatch even when the record
/// does not list it (for example after an uncertain create).
pub(crate) async fn has_marker(room: &Room) -> bool {
    // A failed state read is not proof that ordinary bootstrap is safe.
    for event_type in [MARKER_TYPE, "m.room.create"] {
        match room
            .get_state_event(StateEventType::from(event_type), "")
            .await
        {
            Err(_) => return true,
            Ok(Some(raw)) => {
                let raw = match raw {
                    RawAnySyncOrStrippedState::Sync(raw) => raw.json().get().to_owned(),
                    RawAnySyncOrStrippedState::Stripped(raw) => raw.json().get().to_owned(),
                };
                match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(event) if is_workspace_state(&event) => return true,
                    Err(_) => return true,
                    _ => {}
                }
            }
            Ok(None) if event_type == "m.room.create" => return true,
            Ok(None) => {}
        }
    }
    false
}

pub(crate) fn is_workspace_state(event: &serde_json::Value) -> bool {
    event["type"] == MARKER_TYPE
        || (event["type"] == "m.room.create" && event["content"].get(MARKER_TYPE).is_some())
}

impl MatrixOps for LiveMatrix {
    fn bot_id(&self) -> anyhow::Result<String> {
        Ok(self.bot()?.to_string())
    }
    async fn default_room_version(&self) -> anyhow::Result<String> {
        let response = self
            .client
            .send(get_capabilities::v3::Request::new())
            .await?;
        Ok(response.capabilities.room_versions.default.to_string())
    }

    async fn create_room(&self, spec: &RoomSpec) -> Result<String, CreateError> {
        let bot = self
            .bot()
            .map_err(|e| CreateError::Rejected(e.to_string()))?
            .to_string();
        let request =
            create_request(spec, &bot).map_err(|e| CreateError::Rejected(e.to_string()))?;
        // createRoom has no transaction ID: one attempt, no SDK retries.
        match self
            .client
            .send(request)
            .with_request_config(RequestConfig::new().disable_retry())
            .await
        {
            Ok(response) => Ok(response.room_id.to_string()),
            Err(error) => {
                let status = error.as_client_api_error().map(|e| e.status_code.as_u16());
                if status.is_some_and(|s| (400..500).contains(&s)) {
                    Err(CreateError::Rejected(error.to_string()))
                } else {
                    Err(CreateError::Uncertain(error.to_string()))
                }
            }
        }
    }

    async fn find_marked_rooms(&self, marker: &Marker) -> anyhow::Result<Vec<String>> {
        let bot = self.bot()?.to_owned();
        let mut found = Vec::new();
        for room in self.client.joined_rooms() {
            let created_by_bot = room.creators().is_some_and(|c| c.contains(&bot));
            if created_by_bot && room_marker(&room).await.as_ref() == Some(marker) {
                found.push(room.room_id().to_string());
            }
        }
        Ok(found)
    }

    async fn invite(&self, room: &str, user: &str) -> anyhow::Result<()> {
        let room = self.room(room).await?;
        let user = UserId::parse(user)?;
        let current = room.members(RoomMemberships::INVITE | RoomMemberships::JOIN);
        if current.await?.iter().any(|m| m.user_id() == user) {
            return Ok(());
        }
        room.invite_user_by_id(&user).await?;
        Ok(())
    }

    async fn link_child(&self, space: &str, child: &str) -> anyhow::Result<()> {
        let via = self.via()?;
        self.room(space)
            .await?
            .send_state_event_raw("m.space.child", child, json!({"via": [via]}))
            .await?;
        Ok(())
    }

    async fn link_parent(&self, child: &str, space: &str) -> anyhow::Result<()> {
        let via = self.via()?;
        self.room(child)
            .await?
            .send_state_event_raw(
                "m.space.parent",
                space,
                json!({"via": [via], "canonical": true}),
            )
            .await?;
        Ok(())
    }

    async fn set_name(&self, room: &str, name: &str) -> anyhow::Result<()> {
        self.room(room).await?.set_name(name.to_string()).await?;
        Ok(())
    }
}

pub(crate) struct LiveSessions {
    pub server: Arc<Server>,
    pub client: Client,
    pub attached: AttachedChannels,
    pub pending: PendingApprovals,
    pub login_id: String,
    pub owning_agent: String,
}

impl SessionOps for LiveSessions {
    fn agent_id(&self) -> anyhow::Result<String> {
        owning_agent_entry(&self.server, &self.owning_agent)
            .map(|agent| agent.db_id.to_string())
            .ok_or_else(|| anyhow::anyhow!("owning agent is not hosted"))
    }
    async fn provision(
        &self,
        room: &str,
        source: &str,
        allow_create: bool,
    ) -> anyhow::Result<String> {
        let agent = owning_agent_entry(&self.server, &self.owning_agent).ok_or_else(|| {
            anyhow::anyhow!("owning agent {} is not hosted yet", self.owning_agent)
        })?;
        let (_conv, db) = self
            .server
            .registry()
            .provision_marked_conversational_channel_session(
                &agent,
                &self.login_id,
                "matrix",
                &self.login_id,
                room,
                source,
                allow_create,
            )
            .await?;
        Ok(db.root_id().to_string())
    }

    async fn install_delivery(&self, room: &str, session: &str) -> anyhow::Result<()> {
        let agent = owning_agent_entry(&self.server, &self.owning_agent)
            .ok_or_else(|| anyhow::anyhow!("owning agent is not hosted"))?;
        let (_conv, db) = crate::bridge::resolve_workspace_session(
            &self.server,
            &agent,
            &self.login_id,
            room,
            session,
        )
        .await?;
        let id = OwnedRoomId::try_from(room)?;
        let room = match self.client.get_room(&id) {
            Some(room) => room,
            None => {
                tokio::time::timeout(ROOM_ECHO_TIMEOUT, self.client.await_room_remote_echo(&id))
                    .await
                    .map_err(|_| anyhow::anyhow!("room {room} is not visible through sync yet"))?
            }
        };
        attach_once(
            &self.attached,
            &db,
            room,
            &self.server,
            &self.login_id,
            &self.owning_agent,
            self.pending.clone(),
        )
        .await
    }

    async fn set_label(&self, session: &str, label: &str) -> anyhow::Result<()> {
        let (_conv, db) = self.server.registry().open_session(session).await?;
        chaz_core::session::set_session_description(&db, Some(label)).await?;
        Ok(())
    }
}
