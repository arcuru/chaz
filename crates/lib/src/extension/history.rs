//! Read-only retained tool evidence scoped to a caller's current session/tools.
//! A reference is an identity, not a capability. Every read revalidates current
//! membership, source-tool visibility, and a completed coherent transcript.
use crate::{
    extension::projection::ContextSource,
    runtime::{ToolCallRequest, ToolResultOutcome},
    session::{Session, TurnTranscriptMessage, TurnTranscriptRecord},
    tool::ScopedTools,
};
use eidetica::Database;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const MAX_HISTORY_PAGE_BYTES: usize = 16 * 1024;

/// Host-created invocation scope; no database or execution handle is exposed.
pub struct ScopedHistory {
    db: Database,
    agent: String,
    tools: ScopedTools,
    active: HashSet<String>,
    selected: Option<Vec<(ContextSource, ToolCallRequest)>>,
}

/// Recorded bytes only (after hooks/scanning/recording bounds), not the output
/// that originally left an external process. Nothing here re-executes a tool.
#[derive(Clone, Debug)]
pub struct RetainedToolResult {
    pub reference: String,
    pub tool_name: String,
    pub output: String,
    pub sha256: String,
    /// Legacy records have a marker, not a separate truncation bit. Report
    /// marker presence conservatively; absence cannot prove pre-recording loss.
    pub stored_truncation_marker: bool,
}
impl RetainedToolResult {
    pub fn frame(&self, payload: &str) -> String {
        crate::runtime::wrap_tool_output(&self.tool_name, payload)
    }
}

#[derive(Debug, Serialize)]
pub struct HistoryPage {
    pub reference: String,
    pub status: &'static str,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub stored_bytes: usize,
    pub sha256: Option<String>,
    pub stored_truncation_marker: bool,
    pub content: String,
}

impl ScopedHistory {
    pub(crate) fn new(
        db: Database,
        agent: String,
        tools: ScopedTools,
        active: HashSet<String>,
    ) -> Self {
        Self {
            db,
            agent,
            tools,
            active,
            selected: None,
        }
    }

    pub(crate) fn for_projection(
        mut self,
        baseline: &super::projection::ProjectedRequest,
        sources: &[ContextSource],
    ) -> Self {
        self.selected =
            Some(
                baseline
                    .messages
                    .iter()
                    .zip(sources)
                    .flat_map(|(message, source)| match message {
                        crate::runtime::RuntimeMessage::AssistantToolCalls {
                            tool_calls, ..
                        } if matches!(source, ContextSource::ReplayedExchange { .. }) => tool_calls
                            .iter()
                            .map(|call| (source.clone(), call.clone()))
                            .collect::<Vec<_>>(),
                        _ => vec![],
                    })
                    .collect(),
            );
        self
    }

    async fn records(&self) -> anyhow::Result<Vec<(TurnTranscriptRecord, ToolCallRequest)>> {
        let snapshot = self.db.snapshot().await?;
        let history = Session::retained_tool_history_at(&self.db, &snapshot).await?;
        // Revalidate authority after the potentially slow retained-store read.
        let current: HashSet<_> = super::read_active(&self.db)
            .await?
            .into_iter()
            .map(|r| r.name().to_string())
            .collect();
        let txn = self.db.new_transaction().await?;
        let meta = txn.get_store::<eidetica::store::DocStore>("meta").await?;
        let agents: Vec<crate::session::AgentRef> = match meta.get_string("agents").await {
            Ok(value) => serde_json::from_str(&value)?,
            Err(error) if error.is_not_found() => vec![],
            Err(error) => return Err(error.into()),
        };
        let legacy = match meta.get_string("agent_name").await {
            Ok(value) => Some(value),
            Err(error) if error.is_not_found() => None,
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            if agents.is_empty() {
                legacy.as_ref().is_none_or(|name| name == &self.agent)
            } else {
                agents.iter().any(|a| a.display_name == self.agent)
            },
            "caller is no longer a session participant"
        );
        let visible = self
            .tools
            .clone()
            .with_active_extensions(Some(self.active.intersection(&current).cloned().collect()));
        // Keep only complete, structurally valid exchanges. Compaction changes
        // the visible context but does not itself revoke retained-source access.
        Ok(history
            .into_iter()
            .filter(|records| crate::context::replay_turn(records, usize::MAX).is_some())
            .flat_map(|records| {
                let calls: Vec<_> = records
                    .iter()
                    .filter_map(|r| match &r.message {
                        TurnTranscriptMessage::ModelResponse {
                            model_sequence,
                            tool_calls,
                            ..
                        } => Some(
                            tool_calls
                                .iter()
                                .cloned()
                                .map(|c| (*model_sequence, c))
                                .collect::<Vec<_>>(),
                        ),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                records
                    .into_iter()
                    .filter_map(|r| {
                        let TurnTranscriptMessage::ToolResult {
                            model_sequence,
                            call_id,
                            name,
                            outcome: ToolResultOutcome::Success,
                            ..
                        } = &r.message
                        else {
                            return None;
                        };
                        visible.get(name)?;
                        let (_, call) = calls
                            .iter()
                            .find(|(seq, c)| seq == model_sequence && &c.id == call_id)?;
                        if self.selected.as_ref().is_some_and(|selected| {
                            !selected
                                .iter()
                                .any(|(source, made)| made == call && matches_source(source, &r))
                        }) {
                            return None;
                        }
                        Some((r, call.clone()))
                    })
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    fn result(&self, record: &TurnTranscriptRecord) -> RetainedToolResult {
        let TurnTranscriptMessage::ToolResult { name, output, .. } = &record.message else {
            unreachable!("records filters tool results")
        };
        let sha256 = format!("{:x}", Sha256::digest(output.as_bytes()));
        // Include immutable source coordinates and bytes, never visible indices.
        let identity = serde_json::to_vec(&(1, self.db.root_id().to_string(), record))
            .expect("record serializes");
        RetainedToolResult {
            reference: format!("tr1:{:x}", Sha256::digest(identity)),
            tool_name: name.clone(),
            output: output.clone(),
            sha256,
            stored_truncation_marker: output.ends_with("\n[tool output truncated at 100 KB]"),
        }
    }

    /// Resolve only the baseline's selected historical source. Unattributed or
    /// current-attempt inputs never gain a retained identity by guessing an id.
    pub async fn selected_result(
        &self,
        source: &ContextSource,
        call: &ToolCallRequest,
    ) -> anyhow::Result<Option<RetainedToolResult>> {
        let ContextSource::ReplayedExchange {
            request_id,
            attempt_id,
            model_sequence,
        } = source
        else {
            return Ok(None);
        };
        Ok(self
            .selected_results(&[(
                ContextSource::ReplayedExchange {
                    request_id: request_id.clone(),
                    attempt_id: attempt_id.clone(),
                    model_sequence: *model_sequence,
                },
                call.clone(),
            )])
            .await?
            .pop()
            .flatten())
    }

    /// Resolve a selected batch from one coherent retained read, without an
    /// invocation cache or persisted derived state.
    pub async fn selected_results(
        &self,
        selected: &[(ContextSource, ToolCallRequest)],
    ) -> anyhow::Result<Vec<Option<RetainedToolResult>>> {
        let records = self.records().await?;
        Ok(selected.iter().map(|(source, call)| {
            let ContextSource::ReplayedExchange { request_id, attempt_id, model_sequence } = source else { return None; };
            records.iter().find(|(record, made)| made == call
                && record.request_id.as_str() == request_id && &record.attempt_id == attempt_id
                && matches!(&record.message, TurnTranscriptMessage::ToolResult { model_sequence: seq, .. } if seq == model_sequence))
                .map(|(record, _)| self.result(record))
        }).collect())
    }

    /// Exact UTF-8 pages, bounded in stored bytes. Unknown, forged, foreign,
    /// removed and currently invisible identities share a non-disclosing result.
    pub async fn page(
        &self,
        reference: &str,
        offset: usize,
        max_bytes: usize,
    ) -> anyhow::Result<HistoryPage> {
        anyhow::ensure!(
            max_bytes > 0 && max_bytes <= MAX_HISTORY_PAGE_BYTES,
            "page size must be 1..=16384 stored bytes"
        );
        anyhow::ensure!(
            reference.len() == 68
                && reference.starts_with("tr1:")
                && reference[4..].bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid retained-source reference"
        );
        for (record, _) in self.records().await? {
            let result = self.result(&record);
            if result.reference != reference {
                continue;
            }
            let output = &result.output;
            anyhow::ensure!(
                offset <= output.len() && output.is_char_boundary(offset),
                "offset is not a stored UTF-8 byte boundary"
            );
            let mut end = offset.saturating_add(max_bytes).min(output.len());
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            anyhow::ensure!(
                end > offset || offset == output.len(),
                "page size cannot fit the next UTF-8 character"
            );
            return Ok(HistoryPage {
                reference: reference.into(),
                status: "retained",
                offset,
                next_offset: (end < output.len()).then_some(end),
                stored_bytes: output.len(),
                sha256: Some(result.sha256),
                stored_truncation_marker: result.stored_truncation_marker,
                content: output[offset..end].into(),
            });
        }
        Ok(HistoryPage {
            reference: reference.into(),
            status: "missing_or_not_authorized",
            offset,
            next_offset: None,
            stored_bytes: 0,
            sha256: None,
            stored_truncation_marker: false,
            content: String::new(),
        })
    }
}

fn matches_source(source: &ContextSource, record: &TurnTranscriptRecord) -> bool {
    matches!((source, &record.message),
        (ContextSource::ReplayedExchange { request_id, attempt_id, model_sequence },
         TurnTranscriptMessage::ToolResult { model_sequence: seq, .. })
        if record.request_id.as_str() == request_id && &record.attempt_id == attempt_id && seq == model_sequence)
}
