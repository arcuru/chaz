//! Version-one immutable conversation records. Bodies occur in facts, not views.
use eidetica::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::runtime::{ResponseMetadata, ToolCallRequest};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactId {
    pub database: String,
    pub operation: String,
    pub index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepresentationId {
    pub database: String,
    pub operation: String,
    pub index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedBinding {
    pub database: String,
    /// Main-graph tips, not store tips. Constructed by the session adapter.
    pub frontier: Snapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallId {
    pub request: String,
    pub branch: String,
    pub model_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallId {
    pub response: FactId,
    pub index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecutionId {
    pub call: ToolCallId,
    pub attempt: String,
    pub ordinal: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeResponse {
    pub call: CallId,
    pub attempt: String,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallRequest>,
    /// Includes opaque native output/reasoning and its existing trusted origin.
    /// Keep unknown original observations here, never in compact accounting.
    pub provider_extra: BTreeMap<String, serde_json::Value>,
    pub metadata: Option<ResponseMetadata>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Payload {
    Input {
        sender: String,
        text: String,
    },
    NativeModelResponse {
        response: NativeResponse,
    },
    ToolOutcome {
        execution: ToolExecutionId,
        executed_arguments: String,
        content: String,
        outcome: crate::runtime::ToolResultOutcome,
    },
    TransformationOutput {
        text: String,
    },
    PublishedBody {
        text: String,
    },
    /// Inspection only; recording a control observation conveys no authority.
    ControlObservation {
        value: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub payload_version: u32,
    pub payload: Payload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum View {
    Message(MessageRole),
    Directive,
    Summary,
    NativeExchange,
    ToolResult,
    AuditOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Representation {
    pub fact: FactId,
    pub view: View,
    pub decoder_version: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationKind {
    Append,
    Replace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub version: u32,
    pub op_id: String,
    pub branch_id: String,
    pub observed: ObservedBinding,
    pub causal_seq: u64,
    pub kind: OperationKind,
    pub targets: Vec<RepresentationId>,
    pub facts: Vec<Fact>,
    pub outputs: Vec<Representation>,
}

impl Operation {
    #[cfg(test)]
    pub fn fact_id(&self, index: u32) -> FactId {
        FactId {
            database: self.observed.database.clone(),
            operation: self.op_id.clone(),
            index,
        }
    }

    pub fn representation_id(&self, index: u32) -> RepresentationId {
        RepresentationId {
            database: self.observed.database.clone(),
            operation: self.op_id.clone(),
            index,
        }
    }
}
