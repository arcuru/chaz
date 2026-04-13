pub mod matrix;
pub mod tui;

use crate::backends::BackendManager;
use crate::role::RoleDetails;
use crate::session::SessionMessage;
use tokio::sync::{mpsc, oneshot};

/// Trait for transport gateways (Matrix, TUI, etc.)
///
/// A gateway owns a transport connection and converts transport-specific
/// events into `ChatRequest`s sent to the router via `event_tx`.
pub trait Gateway {
    fn run(
        self,
        event_tx: mpsc::Sender<ChatRequest>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

/// A request to process a chat message through the agent runtime.
///
/// Gateways send their native transport ID (e.g., Matrix room ID, "tui").
/// The router resolves this to a `ConversationId` via SessionManager.
pub struct ChatRequest {
    /// Transport-native identifier (e.g., Matrix room ID, "tui")
    pub transport_id: String,
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
