pub mod cli;
pub mod matrix;
pub mod tui;

use crate::server::Server;
use crate::tool::ToolApprovalInfo;
use std::sync::Arc;
use tokio::sync::oneshot;

/// Trait for transport gateways (Matrix, TUI, etc.)
///
/// A gateway owns a transport connection and bridges platform events
/// into session database entries. The server processes entries via
/// callbacks and delivers responses through the response channel.
pub trait Gateway {
    fn run(
        self,
        server: Arc<Server>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
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
