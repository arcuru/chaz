pub mod matrix;
pub mod tui;

use crate::backends::BackendManager;
use crate::role::RoleDetails;
use crate::session::SessionMessage;
use crate::tool::ToolApprovalInfo;
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
    /// Optional agent override from gateway (e.g., Matrix room tags `is.chaz.agent`)
    pub agent_override: Option<String>,
    /// Optional model override from gateway (e.g., Matrix room tags)
    pub model_override: Option<String>,
    /// Optional role override from gateway (e.g., Matrix room tags)
    pub role_override: Option<RoleDetails>,
    /// Backend to use (gateway resolves transport-specific backends)
    pub backend: BackendManager,
    pub response_tx: oneshot::Sender<ChatResponse>,
    /// Room history for backfilling sessions (provided on first message per room)
    pub backfill_history: Option<Vec<SessionMessage>>,
    /// Channel for the runtime to request tool approval from the gateway.
    /// The gateway reads approval requests and sends back decisions.
    pub approval_tx: Option<mpsc::Sender<ApprovalExchange>>,
}

/// Response from the agent runtime
pub enum ChatResponse {
    Message {
        body: String,
        is_markdown: bool,
    },
    Error {
        error: String,
    },
    /// Message was added to session but no LLM response generated (batched with later messages)
    Skipped,
}

/// An approval exchange: the runtime sends tool info and a channel to receive the decision.
pub struct ApprovalExchange {
    pub info: ToolApprovalInfo,
    pub decision_tx: oneshot::Sender<ApprovalDecision>,
}

/// User's decision on a tool approval request
#[derive(Clone, Debug, PartialEq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
    /// Approve this and all remaining tool calls this turn
    ApproveAll,
}
