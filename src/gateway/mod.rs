pub mod matrix;
pub mod tui;

use crate::backends::BackendManager;
use crate::role::RoleDetails;
use crate::session::SessionMessage;
use crate::types::ConversationId;
use tokio::sync::oneshot;

/// A request to process a chat message through the agent runtime
pub struct ChatRequest {
    pub conversation_id: ConversationId,
    pub sender: String,
    pub body: String,
    /// Optional model override from gateway (e.g., Matrix room tags)
    pub model_override: Option<String>,
    /// Optional role override from gateway (e.g., Matrix room tags)
    pub role_override: Option<RoleDetails>,
    /// Backend to use (gateway resolves transport-specific backends)
    pub backend: BackendManager,
    pub response_tx: oneshot::Sender<ChatResponse>,
    /// Room history for backfilling sessions (provided on first message per room)
    pub backfill_history: Option<Vec<SessionMessage>>,
}

/// Response from the agent runtime
pub enum ChatResponse {
    Message { body: String, is_markdown: bool },
    Error { error: String },
}
