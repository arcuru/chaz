use crate::agent::Agent;
use crate::backends::{ChatContext, Message};
use crate::config::Config;
use crate::role::RoleDetails;
use crate::types::ConversationId;

use chrono::{DateTime, Utc};
use eidetica::store::Table;
use eidetica::{Database, Instance};
use openai_api_rs::v1::chat_completion::MessageRole;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info};

/// Maximum messages to include in context sent to the LLM.
/// Older messages are dropped to stay within token limits.
const MAX_CONTEXT_MESSAGES: usize = 50;

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

    /// Load messages from eidetica
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
                Err(e) => error!("Failed to load session messages from eidetica: {e}"),
            }
        }
    }

    /// Add a message to the session with persistence
    pub async fn add_message(&mut self, msg: SessionMessage) {
        // Persist to eidetica (SQLite-backed)
        let store_name = self.store_name();
        match self.database.new_transaction().await {
            Ok(txn) => match txn.get_store::<Table<SessionMessage>>(&store_name).await {
                Ok(store) => {
                    if let Err(e) = store.insert(msg.clone()).await {
                        error!("Failed to persist message to eidetica: {e}");
                    } else if let Err(e) = txn.commit().await {
                        error!("Failed to commit to eidetica: {e}");
                    }
                }
                Err(e) => error!("Failed to open eidetica store: {e}"),
            },
            Err(e) => error!("Failed to create eidetica transaction: {e}"),
        }

        self.messages.push(msg);
    }

    /// Merge backfill history from a gateway (e.g., Matrix room history).
    /// Only inserts messages that are older than our earliest message or fill gaps.
    /// Deduplicates by timestamp+content.
    pub async fn backfill(&mut self, history: Vec<SessionMessage>) {
        if history.is_empty() {
            return;
        }

        let mut new_count = 0;
        for msg in history {
            // Skip if we already have a message with the same timestamp and content
            let already_exists = self.messages.iter().any(|existing| {
                existing.timestamp == msg.timestamp && existing.content == msg.content
            });
            if !already_exists {
                // Persist to eidetica
                let store_name = self.store_name();
                if let Ok(txn) = self.database.new_transaction().await {
                    if let Ok(store) = txn.get_store::<Table<SessionMessage>>(&store_name).await {
                        if store.insert(msg.clone()).await.is_ok() {
                            let _ = txn.commit().await;
                        }
                    }
                }
                self.messages.push(msg);
                new_count += 1;
            }
        }

        if new_count > 0 {
            // Re-sort by timestamp after merging
            self.messages.sort_by_key(|m| m.timestamp);
            info!(
                "Backfilled {} messages for {}",
                new_count, self.conversation_id
            );
        }
    }

    /// Build a ChatContext from session history with truncation
    pub fn build_context(&self, role: Option<RoleDetails>, model: Option<String>) -> ChatContext {
        // Truncate: keep only the most recent messages
        let start = self.messages.len().saturating_sub(MAX_CONTEXT_MESSAGES);
        let messages = self.messages[start..]
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
        }
    }

    fn store_name(&self) -> String {
        format!("messages:{}", self.conversation_id.0)
    }
}

/// Manages sessions across conversations.
///
/// Holds a binding registry that maps transport-native IDs (e.g., Matrix room IDs)
/// to gateway-agnostic `ConversationId`s. Multiple transport IDs can map to the
/// same conversation, enabling cross-gateway sessions.
pub struct SessionManager {
    _instance: Instance,
    database: Database,
    sessions: HashMap<ConversationId, Session>,
    /// Maps transport_id → ConversationId. Enables multiple gateways to share a conversation.
    bindings: HashMap<String, ConversationId>,
    pub agent: Agent,
}

impl SessionManager {
    pub async fn new(
        instance: Instance,
        mut user: eidetica::user::User,
        config: &Config,
    ) -> anyhow::Result<Self> {
        // Find or create the eidetica sessions database
        let database = match user.find_database("chaz-sessions").await {
            Ok(existing) if !existing.is_empty() => existing.into_iter().next().unwrap(),
            _ => {
                let mut settings = eidetica::crdt::Doc::new();
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
            bindings: HashMap::new(),
            agent,
        })
    }

    /// Get the eidetica database (for sharing with tools)
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Resolve a transport ID to a ConversationId.
    ///
    /// Creates a new binding if none exists. Default: transport_id becomes the ConversationId.
    /// Future: this can be overridden to map multiple transport IDs to the same conversation.
    pub fn resolve_conversation(&mut self, transport_id: &str) -> ConversationId {
        self.bindings
            .entry(transport_id.to_string())
            .or_insert_with(|| ConversationId(transport_id.to_string()))
            .clone()
    }

    /// Get or create a session for a conversation
    pub async fn get_or_create(&mut self, id: &ConversationId) -> &mut Session {
        if !self.sessions.contains_key(id) {
            let session = Session::new(id.clone(), self.database.clone()).await;
            info!(
                "Session for {}: {} messages loaded",
                id,
                session.messages.len()
            );
            self.sessions.insert(id.clone(), session);
        }
        self.sessions.get_mut(id).unwrap()
    }
}
