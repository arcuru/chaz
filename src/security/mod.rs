pub mod leak_detector;
pub mod network;
pub mod sanitizer;
pub mod secrets;

pub use leak_detector::{LeakDetector, LeakPolicy};
pub use network::NetworkPolicy;
pub use sanitizer::Sanitizer;
pub use secrets::SecretStore;

use crate::gateway::{ApprovalDecision, ApprovalExchange};
use crate::tool::{ApprovalRequirement, ToolApprovalInfo};
use std::collections::HashSet;
use tokio::sync::{mpsc, oneshot};

/// Security context threaded through the runtime.
#[derive(Clone)]
pub struct SecurityContext {
    pub leak_detector: LeakDetector,
    pub auto_approved_tools: HashSet<String>,
    /// Callback channel for requesting approval. None = deny all approval-required tools.
    pub approval_callback: Option<mpsc::Sender<ApprovalExchange>>,
}

impl SecurityContext {
    /// Check if a tool call needs approval based on its requirement and auto-approve config.
    pub fn needs_approval(&self, tool_name: &str, requirement: &ApprovalRequirement) -> bool {
        match requirement {
            ApprovalRequirement::Never => false,
            ApprovalRequirement::Always => true,
            ApprovalRequirement::UnlessAutoApproved => {
                !self.auto_approved_tools.contains(tool_name)
            }
        }
    }

    /// Request approval for a tool call. Returns the decision.
    /// If no approval channel is set, defaults to Deny for safety.
    pub async fn request_approval(&self, info: ToolApprovalInfo) -> ApprovalDecision {
        match &self.approval_callback {
            Some(tx) => {
                let (decision_tx, decision_rx) = oneshot::channel();
                let exchange = ApprovalExchange { info, decision_tx };
                if tx.send(exchange).await.is_err() {
                    return ApprovalDecision::Deny;
                }
                decision_rx.await.unwrap_or(ApprovalDecision::Deny)
            }
            None => ApprovalDecision::Deny,
        }
    }
}
