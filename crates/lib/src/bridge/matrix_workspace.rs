//! Action requests between a daemon and its Matrix workspace bridge. These
//! are inert history rows unless the bridge can match one to its own live
//! ingress. Neither a session writer nor a claimed sender supplies consent.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::session::{BridgeEventRole, SessionEntry, TurnRequestId};

pub const ACTION_REQUEST_LIFETIME: Duration = Duration::from_secs(30);
pub const ACTION_RESULT_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Rename,
    NewChat,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub kind: ActionKind,
    pub title: String,
    pub turn: TurnRequestId,
    pub attempt: String,
    pub login: String,
    pub room: String,
    pub event: String,
}

impl ActionRequest {
    pub fn into_entry(self, agent: &str) -> serde_json::Result<SessionEntry> {
        encode(agent, BridgeEventRole::MatrixWorkspaceRequest, &self)
    }

    pub fn from_entry(entry: &SessionEntry) -> Option<Self> {
        decode(entry, BridgeEventRole::MatrixWorkspaceRequest)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResult {
    /// Row ID of the exact action invocation, not the waking turn ID.
    pub invocation: TurnRequestId,
    pub outcome: Result<String, String>,
}

impl ActionResult {
    pub fn into_entry(self, login: &str) -> serde_json::Result<SessionEntry> {
        encode(login, BridgeEventRole::MatrixWorkspaceResult, &self)
    }

    pub fn from_entry(entry: &SessionEntry) -> Option<Self> {
        decode(entry, BridgeEventRole::MatrixWorkspaceResult)
    }
}

fn encode(
    sender: &str,
    role: BridgeEventRole,
    value: &impl Serialize,
) -> serde_json::Result<SessionEntry> {
    Ok(SessionEntry::new_bridge_event(
        sender,
        role,
        serde_json::to_string(value)?,
        None,
    ))
}

fn decode<T: serde::de::DeserializeOwned>(
    entry: &SessionEntry,
    role: BridgeEventRole,
) -> Option<T> {
    let event = entry.bridge_event()?;
    if entry.routing.is_some() || event.role()? != role {
        return None;
    }
    serde_json::from_str(&event.body).ok()
}
