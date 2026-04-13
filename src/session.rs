use crate::agent::Agent;
use crate::backends::{ChatContext, Message};
use crate::config::Config;
use crate::role::RoleDetails;
use crate::types::ConversationId;

use chrono::{DateTime, Utc};
use eidetica::crdt::Doc;
use eidetica::store::Table;
use eidetica::{Database, Instance};
use openai_api_rs::v1::chat_completion::MessageRole;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info};

/// A message stored in a session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    pub role: String,
    pub content: String,
    pub sender: String,
    pub timestamp: DateTime<Utc>,
}

/// Per-conversation state with persistent message history
pub struct Session {
    pub conversation_id: ConversationId,
    database: Database,
    messages: Vec<SessionMessage>,
}

impl Session {
    async fn new(conversation_id: ConversationId, database: Database) -> Self {
        let mut session = Session {
            conversation_id,
            database,
            messages: Vec::new(),
        };
        session.load_from_db().await;
        session
    }

    /// Load existing messages from eidetica
    async fn load_from_db(&mut self) {
        let store_name = self.store_name();
        let Ok(txn) = self.database.new_transaction().await else {
            return;
        };
        if let Ok(store) = txn.get_store::<Table<SessionMessage>>(&store_name).await {
            match store.search(|_| true).await {
                Ok(records) => {
                    let mut msgs: Vec<SessionMessage> =
                        records.into_iter().map(|(_, msg)| msg).collect();
                    msgs.sort_by_key(|m| m.timestamp);
                    self.messages = msgs;
                }
                Err(e) => error!("Failed to load session messages: {e}"),
            }
        }
    }

    /// Add a message and persist to eidetica
    pub async fn add_message(&mut self, msg: SessionMessage) {
        self.messages.push(msg.clone());

        let store_name = self.store_name();
        match self.database.new_transaction().await {
            Ok(txn) => match txn.get_store::<Table<SessionMessage>>(&store_name).await {
                Ok(store) => {
                    if let Err(e) = store.insert(msg).await {
                        error!("Failed to persist message: {e}");
                    } else if let Err(e) = txn.commit().await {
                        error!("Failed to commit message: {e}");
                    }
                }
                Err(e) => error!("Failed to open message store: {e}"),
            },
            Err(e) => error!("Failed to create transaction: {e}"),
        }
    }

    /// Build a ChatContext from session history
    pub fn build_context(&self, role: Option<RoleDetails>, model: Option<String>) -> ChatContext {
        let messages = self
            .messages
            .iter()
            .map(|m| {
                let msg_role = match m.role.as_str() {
                    "assistant" => MessageRole::assistant,
                    "system" => MessageRole::system,
                    _ => MessageRole::user,
                };
                Message::new(msg_role, m.content.clone())
            })
            .collect();

        ChatContext {
            messages,
            model,
            role,
            media: Vec::new(),
        }
    }

    fn store_name(&self) -> String {
        format!("messages:{}", self.conversation_id.0)
    }
}

/// Manages sessions across conversations with eidetica persistence
pub struct SessionManager {
    /// Kept alive so Database's WeakInstance stays valid
    _instance: Instance,
    database: Database,
    sessions: HashMap<ConversationId, Session>,
    pub agent: Agent,
}

impl SessionManager {
    pub async fn new(
        instance: Instance,
        mut user: eidetica::user::User,
        config: &Config,
    ) -> anyhow::Result<Self> {
        // Find or create the sessions database
        let database = match user.find_database("chaz-sessions").await {
            Ok(existing) if !existing.is_empty() => existing.into_iter().next().unwrap(),
            _ => {
                let mut settings = Doc::new();
                settings.set("name", "chaz-sessions");
                let key_id = user.get_default_key()?;
                user.create_database(settings, &key_id).await?
            }
        };

        let agent = Agent::from_config(config);

        Ok(Self {
            _instance: instance,
            database,
            sessions: HashMap::new(),
            agent,
        })
    }

    /// Get or create a session for a conversation
    pub async fn get_or_create(&mut self, id: &ConversationId) -> &mut Session {
        if !self.sessions.contains_key(id) {
            let session = Session::new(id.clone(), self.database.clone()).await;
            info!(
                "Session for {}: {} messages loaded from storage",
                id,
                session.messages.len()
            );
            self.sessions.insert(id.clone(), session);
        }
        self.sessions.get_mut(id).unwrap()
    }
}
