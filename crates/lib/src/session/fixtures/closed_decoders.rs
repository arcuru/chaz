// Frozen closed reader boundaries: EntryType from 7d09ffca4965f3f6a3f34ead817a60f6f027dbd7;
// other enums from d54337282658713c0222b83f80e02a377c596e59.
use super::*;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum EntryType {
    Message,
    Directive,
    ToolCall,
    ToolResult,
    Ack,
    Error,
    Summary,
    ApprovalRequest,
    ApprovalDecision,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SessionCommand {
    Compact {
        source_snapshot: Snapshot,
    },
    Retry {
        target_request_id: TurnRequestId,
        expected_interrupted_attempt_id: String,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum SessionCommandOutcome {
    Compact { summary: String },
    RetryAccepted { target_attempt_id: String },
    Rejected { message: String },
    Failed { message: String },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum TurnTranscriptMessage {
    ModelResponse {
        model_sequence: u64,
        content: Option<String>,
        tool_calls: Vec<crate::runtime::ToolCallRequest>,
        provider_extra: serde_json::Map<String, serde_json::Value>,
        metadata: Option<crate::runtime::ResponseMetadata>,
        terminal: bool,
    },
    ToolResult {
        model_sequence: u64,
        call_index: usize,
        call_id: String,
        name: String,
        output: String,
        outcome: ToolResultOutcome,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ToolResultOutcome {
    Success,
    Error,
    Denied,
    ApprovalTimedOut,
    RateLimited,
    Blocked,
    TimedOut,
    Unavailable,
}
