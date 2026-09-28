//! The core-owned, transport-neutral session protocol for external events.
//!
//! The payload lives in `SessionEntry::content` to avoid changing every
//! ordinary chat entry. An unsupported version or role is inert, but the row
//! remains in the shared transcript for a newer reader.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{EntryRouting, EntryType, SessionEntry};
use chrono::Utc;

pub const BRIDGE_EVENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeEventRole {
    Observation,
    Outbound,
    Receipt,
}

impl BridgeEventRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Outbound => "outbound",
            Self::Receipt => "receipt",
        }
    }
}

/// Versioned envelope; `role` stays open on deserialization so future roles
/// do not make the entire session's typed `entries` scan fail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BridgeEvent {
    pub version: u32,
    pub role: String,
    pub body: String,
    /// Namespaced, optional transport metadata; not a source of core authority.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub adapter: BTreeMap<String, serde_json::Value>,
}

impl BridgeEvent {
    pub fn new(role: BridgeEventRole, body: impl Into<String>) -> Self {
        Self {
            version: BRIDGE_EVENT_VERSION,
            role: role.as_str().to_string(),
            body: body.into(),
            adapter: BTreeMap::new(),
        }
    }

    pub fn role(&self) -> Option<BridgeEventRole> {
        if self.version != BRIDGE_EVENT_VERSION {
            return None;
        }
        match self.role.as_str() {
            "observation" => Some(BridgeEventRole::Observation),
            "outbound" => Some(BridgeEventRole::Outbound),
            "receipt" => Some(BridgeEventRole::Receipt),
            _ => None,
        }
    }
}

impl SessionEntry {
    pub fn new_bridge_event(
        sender: impl Into<String>,
        role: BridgeEventRole,
        body: impl Into<String>,
        routing: Option<EntryRouting>,
    ) -> Self {
        Self {
            sender: sender.into(),
            content: serde_json::to_string(&BridgeEvent::new(role, body))
                .expect("bridge event envelope is JSON-serializable"),
            timestamp: Utc::now(),
            entry_type: EntryType::BridgeEvent,
            metadata: None,
            routing,
        }
    }

    /// A known core role, including the already-persisted Matrix rows.
    /// Unsupported envelopes and unknown kinds never acquire side effects.
    pub fn bridge_role(&self) -> Option<BridgeEventRole> {
        match &self.entry_type {
            EntryType::MatrixObserved => Some(BridgeEventRole::Observation),
            EntryType::MatrixSend => Some(BridgeEventRole::Outbound),
            EntryType::MatrixSent => Some(BridgeEventRole::Receipt),
            EntryType::BridgeEvent => self.bridge_event()?.role(),
            _ => None,
        }
    }

    pub fn bridge_event(&self) -> Option<BridgeEvent> {
        (self.entry_type == EntryType::BridgeEvent)
            .then(|| serde_json::from_str(&self.content).ok())
            .flatten()
    }

    pub fn bridge_body(&self) -> Option<String> {
        match &self.entry_type {
            EntryType::BridgeEvent => {
                let event = self.bridge_event()?;
                event.role().map(|_| event.body)
            }
            EntryType::MatrixObserved | EntryType::MatrixSend | EntryType::MatrixSent => {
                Some(self.content.clone())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContextConfig;
    use crate::context::ContextBuilder;
    use crate::runtime::RuntimeMessage;
    use crate::session::{Session, TransportRef};
    use crate::types::ConversationId;

    #[tokio::test]
    async fn unknown_kinds_and_roles_survive_table_scan_without_waking_or_entering_context() {
        let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
        let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        let make = |kind: EntryType, body: &str| SessionEntry {
            sender: "visitor".into(),
            content: body.into(),
            timestamp: Utc::now(),
            entry_type: kind,
            metadata: None,
            routing: None,
        };
        let known_id = session
            .add_entry(make(EntryType::Message, "known question"))
            .await
            .unwrap();
        let future: EntryType = serde_json::from_str(r#""FutureOutbound""#).unwrap();
        assert_eq!(
            serde_json::to_string(&future).unwrap(),
            r#""FutureOutbound""#
        );
        assert_eq!(
            serde_json::from_str::<EntryType>(r#""MatrixSent""#).unwrap(),
            EntryType::MatrixSent
        );
        session
            .add_entry(make(future.clone(), "future private body"))
            .await
            .unwrap();
        let future_with_payload: EntryType =
            serde_json::from_str(r#"{"FutureControl":{"secret":42}}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&future_with_payload).unwrap(),
            r#"{"FutureControl":{"secret":42}}"#
        );
        assert!(serde_json::from_str::<EntryType>(r#"{"Message":{"unsafe":true}}"#).is_err());
        session
            .add_entry(make(future_with_payload.clone(), "opaque future row"))
            .await
            .unwrap();
        let unknown = r#"{"version":12,"role":"outbound","body":"future effect"}"#;
        session
            .add_entry(make(EntryType::BridgeEvent, unknown))
            .await
            .unwrap();
        let reloaded = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        assert_eq!(reloaded.entries().len(), 4);
        assert_eq!(reloaded.entries()[1].entry_type, future);
        assert_eq!(reloaded.entries()[2].entry_type, future_with_payload);
        assert_eq!(reloaded.entries()[3].bridge_role(), None);
        let request = reloaded
            .next_turn_request(|_| false, &Default::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.0.id, known_id);
        let context =
            ContextBuilder::new(reloaded.entries(), "agent", "", &ContextConfig::default())
                .build()
                .await;
        assert!(
            context
                .messages
                .iter()
                .any(|m| matches!(m, RuntimeMessage::User(text) if text == "known question"))
        );
        assert!(
            !context
                .messages
                .iter()
                .any(|m| matches!(m, RuntimeMessage::User(text) if text.contains("future")))
        );
        // Legacy Matrix rows still decode into core roles, without rewriting.
        let legacy = make(EntryType::MatrixObserved, "legacy observation");
        assert_eq!(legacy.bridge_role(), Some(BridgeEventRole::Observation));
    }

    #[test]
    fn reads_legacy_matrix_routing_and_policy_fields_without_rewriting_rows() {
        let legacy = serde_json::json!({
            "sender":"agent", "content":"answer", "timestamp":"2026-09-27T00:00:00Z",
            "entry_type":"Message", "routing":{"matrix_send_id":"old-outbound",
                "matrix_participation":true}
        });
        let entry: SessionEntry = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            entry.routing.as_ref().unwrap().outbound_id.as_deref(),
            Some("old-outbound")
        );
        assert!(entry.routing.as_ref().unwrap().ambient_candidate);
        let old_policy: crate::session::AmbientParticipation = serde_json::from_value(
            serde_json::json!({"login_id":"@bot:s","room_id":"!room:s","generation":"old"}),
        )
        .unwrap();
        assert!(old_policy.matches_source("matrix", "@bot:s", "!room:s"));
        assert!(!old_policy.matches_source("test", "@bot:s", "!room:s"));
    }

    #[tokio::test]
    async fn generic_ingress_checks_binding_and_ambient_policy_before_wake() {
        use crate::session::{
            AmbientParticipation, bind_conversational_transport, update_meta_on_db,
        };
        let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
        bind_conversational_transport(&db, "test", "login", "room")
            .await
            .unwrap();
        let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        let inbound = |body: &str, ambient: bool| SessionEntry {
            sender: "visitor".into(),
            content: body.into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: Some(EntryRouting {
                source: Some(TransportRef {
                    transport: "test".into(),
                    login_id: "login".into(),
                    channel: "room".into(),
                    sender: Some("visitor".into()),
                    sender_display: None,
                    message_id: None,
                }),
                ambient_candidate: ambient,
                ..Default::default()
            }),
        };
        session
            .add_bridge_input(inbound("context-only", true), true)
            .await
            .unwrap();
        assert_eq!(
            session.entries()[0].bridge_role(),
            Some(BridgeEventRole::Observation)
        );
        assert!(
            session
                .next_turn_request(|_| false, &Default::default())
                .await
                .unwrap()
                .is_none()
        );
        let policy = AmbientParticipation::new_for("test", "login", "room");
        update_meta_on_db(&db, |meta| meta.ambient_participation = Some(policy))
            .await
            .unwrap();
        session
            .add_bridge_input(inbound("wake", true), true)
            .await
            .unwrap();
        assert_eq!(session.entries()[1].entry_type, EntryType::Message);
        assert!(
            session
                .add_bridge_input(
                    SessionEntry {
                        routing: Some(EntryRouting {
                            source: Some(TransportRef {
                                channel: "another".into(),
                                ..inbound("wrong", false).routing.unwrap().source.unwrap()
                            }),
                            ..Default::default()
                        }),
                        ..inbound("wrong", false)
                    },
                    true
                )
                .await
                .is_err()
        );
    }
}
