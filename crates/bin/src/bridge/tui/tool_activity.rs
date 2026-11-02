//! Display-only tool identities and full payloads from one pinned session read.
//! Audit text and adjacency are never association evidence.
use std::collections::{HashMap, HashSet};

use chaz_core::runtime::ToolResultOutcome;
use chaz_core::session::{
    EntryType, SessionEntry, TurnActivity, TurnAttempt, TurnAttemptStatus, TurnTranscriptMessage,
    TurnTranscriptRecord,
};
use chrono::{DateTime, Utc};
use eidetica::store::Table;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RunKey {
    pub request: String,
    pub attempt: String,
    pub execution: Option<String>,
    pub agent: String,
}

impl RunKey {
    fn from_record(record: &TurnTranscriptRecord) -> Option<Self> {
        let identity = record.identity.as_ref()?;
        Some(Self {
            request: record.request_id.as_str().to_owned(),
            attempt: record.attempt_id.clone(),
            execution: identity.execution_id.clone(),
            agent: identity.agent.clone(),
        })
    }

    fn row_id(&self, sequence: u64, index: usize) -> String {
        format!(
            "{}:{}:{sequence:020}:{index:010}",
            self.attempt,
            self.execution.as_deref().unwrap_or("")
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct CallKey {
    pub run: RunKey,
    pub model_sequence: u64,
    pub index: usize,
    pub id: String,
}

#[derive(Debug)]
pub(super) struct Position {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug)]
pub(super) struct ToolResult {
    pub position: Position,
    pub output: String,
    pub outcome: ToolResultOutcome,
}

#[derive(Debug)]
pub(super) struct ToolItem {
    pub key: CallKey,
    pub run: usize,
    pub ordinal: usize,
    pub position: Position,
    pub name: String,
    pub arguments: String,
    pub result: Option<ToolResult>,
}

#[derive(Debug)]
pub(super) struct ToolRun {
    pub key: RunKey,
    pub calls: Vec<usize>,
    pub completed: bool,
    pub cancelled: bool,
    pub active: bool,
}

impl ToolRun {
    pub(super) fn lifecycle(&self) -> &'static str {
        if self.cancelled {
            "cancelled turn"
        } else if self.completed && self.key.execution.is_none() {
            "attempt completed"
        } else if self.completed {
            "attempt completed elsewhere; branch state unknown"
        } else if self.active {
            "attempt active"
        } else {
            "activity unknown"
        }
    }
}

impl ToolItem {
    pub(super) fn status(&self, run: &ToolRun) -> &'static str {
        match self.result.as_ref().map(|result| &result.outcome) {
            Some(ToolResultOutcome::Success) => "done",
            Some(ToolResultOutcome::Cancelled) => "cancelled",
            Some(ToolResultOutcome::Unknown { .. }) => "outcome unknown",
            Some(_) => "failed",
            None if run.cancelled => "cancelled turn; result missing",
            None if run.completed && run.key.execution.is_none() => "missing result",
            None if run.completed => "result not recorded; branch state unknown",
            None if run.active => "awaiting result",
            None => "result not recorded; activity unknown",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ToolRow {
    Call(usize),
    Result,
    Prose { run: usize, sequence: u64 },
}

#[derive(Debug, Default)]
pub(super) struct ToolActivity {
    pub runs: Vec<ToolRun>,
    pub items: Vec<ToolItem>,
    pub rows: HashMap<String, ToolRow>,
}

struct Invocation {
    row: String,
    key: CallKey,
    position: Position,
    name: String,
    arguments: String,
}

struct Response {
    row: String,
    name: String,
    result: ToolResult,
}

/// Duplicate identities are not resolved by ordering or last-writer choice.
fn unique<K: std::hash::Hash + Eq, V>(map: &mut HashMap<K, Option<V>>, key: K, value: V) {
    map.entry(key)
        .and_modify(|slot| *slot = None)
        .or_insert(Some(value));
}

impl ToolActivity {
    pub(super) async fn load<'a>(
        db: &eidetica::Database,
        tips: &eidetica::Snapshot,
        entries: impl Iterator<Item = (&'a str, &'a SessionEntry)>,
    ) -> anyhow::Result<Self> {
        let txn = db.new_transaction_at(tips).await?;
        let records = txn
            .get_store::<Table<TurnTranscriptRecord>>("turn_transcript")
            .await?
            .search(|_| true)
            .await?;
        let attempts = txn
            .get_store::<Table<TurnAttempt>>("turn_attempts")
            .await?
            .search(|_| true)
            .await?;
        Ok(Self::project(
            entries,
            records.into_iter().map(|(_, record)| record),
            attempts.into_iter().map(|(_, attempt)| attempt),
        ))
    }

    fn project<'a>(
        entries: impl Iterator<Item = (&'a str, &'a SessionEntry)>,
        records: impl Iterator<Item = TurnTranscriptRecord>,
        attempts: impl Iterator<Item = TurnAttempt>,
    ) -> Self {
        let entries: Vec<_> = entries.collect();
        let by_id: HashMap<_, _> = entries.iter().copied().collect();
        let completed: HashSet<_> = attempts
            .filter(|a| a.status == TurnAttemptStatus::Completed)
            .map(|a| (a.request_id.as_str().to_owned(), a.attempt_id))
            .collect();
        let mut cancelled = HashSet::new();
        let mut calls = HashMap::new();
        let mut results = HashMap::new();
        let mut prose = HashMap::new();
        let mut anchors = HashMap::<String, usize>::new();
        for record in records {
            let Some(run) = RunKey::from_record(&record) else {
                continue;
            };
            let position = Position {
                sequence: record.sequence,
                timestamp: record.timestamp,
            };
            match record.message {
                TurnTranscriptMessage::ModelResponse {
                    model_sequence,
                    content,
                    tool_calls,
                    terminal: false,
                    ..
                } => {
                    let offset = usize::from(content.as_ref().is_some_and(|s| !s.is_empty()));
                    if offset > 0 {
                        let row = run.row_id(record.sequence, 0);
                        *anchors.entry(row.clone()).or_default() += 1;
                        prose.insert(row, (run.clone(), record.sequence));
                    }
                    for (index, call) in tool_calls.into_iter().enumerate() {
                        let row = run.row_id(record.sequence, index + offset);
                        *anchors.entry(row.clone()).or_default() += 1;
                        let key = CallKey {
                            run: run.clone(),
                            model_sequence,
                            index,
                            id: call.id,
                        };
                        unique(
                            &mut calls,
                            key.clone(),
                            Invocation {
                                row,
                                key,
                                position: Position { ..position },
                                name: call.name,
                                arguments: call.arguments,
                            },
                        );
                    }
                }
                TurnTranscriptMessage::ToolResult {
                    model_sequence,
                    call_index,
                    call_id,
                    name,
                    output,
                    outcome,
                } => {
                    let row = run.row_id(record.sequence, 0);
                    *anchors.entry(row.clone()).or_default() += 1;
                    unique(
                        &mut results,
                        CallKey {
                            run,
                            model_sequence,
                            index: call_index,
                            id: call_id,
                        },
                        Response {
                            row,
                            name,
                            result: ToolResult {
                                position,
                                output,
                                outcome,
                            },
                        },
                    );
                }
                TurnTranscriptMessage::Cancelled => {
                    cancelled.insert(run);
                }
                _ => {}
            }
        }
        let valid = |row: &str, kind: EntryType, agent: &str| {
            anchors.get(row) == Some(&1)
                && by_id
                    .get(row)
                    .is_some_and(|entry| entry.entry_type == kind && entry.sender == agent)
        };
        let mut invocations: HashMap<_, _> = calls
            .into_values()
            .flatten()
            .filter(|call| valid(&call.row, EntryType::ToolCall, &call.key.run.agent))
            .map(|call| (call.row.clone(), call))
            .collect();
        let mut projection = Self::default();
        let mut runs = HashMap::new();
        // Entry order, not hash iteration or result arrival, sets presentation order.
        for (row, _) in entries {
            let Some(call) = invocations.remove(row) else {
                continue;
            };
            let response = match results.remove(&call.key) {
                Some(None) => continue,
                Some(Some(response)) if response.name != call.name => continue,
                Some(Some(response)) => {
                    // Future outcomes deliberately have no display audit row in
                    // the native writer. Retain that typed payload as unknown;
                    // never consume a row of another kind or sender.
                    let unknown_without_row =
                        matches!(response.result.outcome, ToolResultOutcome::Unknown { .. })
                            && !by_id.contains_key(response.row.as_str())
                            && anchors.get(&response.row) == Some(&1);
                    (valid(&response.row, EntryType::ToolResult, &call.key.run.agent)
                        || unknown_without_row)
                        .then_some(response)
                }
                None => None,
            };
            let run = *runs.entry(call.key.run.clone()).or_insert_with(|| {
                let run = projection.runs.len();
                projection.runs.push(ToolRun {
                    completed: completed
                        .contains(&(call.key.run.request.clone(), call.key.run.attempt.clone())),
                    cancelled: cancelled.contains(&call.key.run),
                    active: false,
                    key: call.key.run.clone(),
                    calls: Vec::new(),
                });
                run
            });
            let item = projection.items.len();
            projection.runs[run].calls.push(item);
            projection.rows.insert(row.to_owned(), ToolRow::Call(item));
            projection.items.push(ToolItem {
                key: call.key,
                run,
                ordinal: projection.runs[run].calls.len(),
                position: call.position,
                name: call.name,
                arguments: call.arguments,
                result: response.map(|response| {
                    if by_id.contains_key(response.row.as_str()) {
                        projection.rows.insert(response.row, ToolRow::Result);
                    }
                    response.result
                }),
            });
        }
        for (row, (key, sequence)) in prose {
            if valid(&row, EntryType::TurnContext, &key.agent)
                && let Some(&run) = runs.get(&key)
            {
                projection
                    .rows
                    .insert(row, ToolRow::Prose { run, sequence });
            }
        }
        projection
    }

    /// A heartbeat describes the attempt, not execution of a particular call.
    /// Cache invalidation must also happen when this disposable evidence expires.
    pub(super) fn age(&mut self, activity: &TurnActivity, now: DateTime<Utc>) -> bool {
        let active: HashSet<_> = activity.active_at(now).collect();
        let mut changed = false;
        for run in &mut self.runs {
            let fresh = active.contains(run.key.attempt.as_str());
            changed |= run.active != fresh;
            run.active = fresh;
        }
        changed
    }
}

#[cfg(test)]
#[path = "tool_activity_tests.rs"]
mod tests;
