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
use std::path::PathBuf;
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
    /// Path to the JSONL file for this session (None = no file persistence)
    file_path: Option<PathBuf>,
}

impl Session {
    async fn new(
        conversation_id: ConversationId,
        database: Database,
        sessions_dir: Option<&PathBuf>,
    ) -> Self {
        let file_path = sessions_dir.map(|dir| {
            // Use a filesystem-safe hash of the conversation ID
            let hash = Self::hash_id(&conversation_id);
            dir.join(format!("{hash}.jsonl"))
        });

        let mut session = Session {
            conversation_id,
            database,
            messages: Vec::new(),
            file_path,
        };

        // Load from file first (persistent across restarts)
        session.load_from_file();

        // Also try eidetica (may have messages from this run not yet on disk — shouldn't happen, but defensive)
        if session.messages.is_empty() {
            session.load_from_db().await;
        }

        session
    }

    /// Load messages from JSONL file
    fn load_from_file(&mut self) {
        let Some(path) = &self.file_path else {
            return;
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for line in content.lines() {
            if let Ok(msg) = serde_json::from_str::<SessionMessage>(line) {
                self.messages.push(msg);
            }
        }
    }

    /// Load messages from eidetica (fallback)
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

    /// Append a message to the JSONL file
    fn persist_to_file(&self, msg: &SessionMessage) {
        let Some(path) = &self.file_path else {
            return;
        };
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut file) => {
                if let Ok(json) = serde_json::to_string(msg) {
                    let _ = writeln!(file, "{json}");
                }
            }
            Err(e) => error!("Failed to persist message to file: {e}"),
        }
    }

    /// Add a message to the session with persistence
    pub async fn add_message(&mut self, msg: SessionMessage) {
        // Persist to file (survives restarts)
        self.persist_to_file(&msg);

        // Persist to eidetica (in-memory, for CRDT operations)
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
            media: Vec::new(),
        }
    }

    fn store_name(&self) -> String {
        format!("messages:{}", self.conversation_id.0)
    }

    /// Produce a filesystem-safe hash of the conversation ID
    fn hash_id(id: &ConversationId) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        id.0.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

/// Manages sessions across conversations
pub struct SessionManager {
    _instance: Instance,
    database: Database,
    sessions: HashMap<ConversationId, Session>,
    sessions_dir: Option<PathBuf>,
    pub agent: Agent,
}

impl SessionManager {
    pub async fn new(
        instance: Instance,
        mut user: eidetica::user::User,
        config: &Config,
        state_dir: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        // Set up file persistence directory
        let sessions_dir = state_dir.map(|dir| {
            let sessions = dir.join("sessions");
            if let Err(e) = std::fs::create_dir_all(&sessions) {
                error!("Failed to create sessions directory: {e}");
            }
            sessions
        });

        // Find or create the eidetica sessions database
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
            sessions_dir,
            agent,
        })
    }

    /// Get or create a session for a conversation
    pub async fn get_or_create(&mut self, id: &ConversationId) -> &mut Session {
        if !self.sessions.contains_key(id) {
            let session = Session::new(
                id.clone(),
                self.database.clone(),
                self.sessions_dir.as_ref(),
            )
            .await;
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
