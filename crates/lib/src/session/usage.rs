//! Cross-session LLM usage aggregation.
//!
//! Walks the user-central session catalog, opens each session, and sums
//! deduplicated per-call observations plus compatible legacy summaries. Used by:
//! - `/costs` slash command (TUI/Matrix) for an interactive rollup
//! - `chaz usage` CLI subcommand for headless/scripted rollups
//!
//! Both consumers reuse the same `collect_usage()` and rendering helpers;
//! they differ only in the surrounding I/O.

mod calls;
use crate::runtime::ResponseMetadata;
use crate::session::{
    BridgeKind, EntryType, Session, SessionEntry, SessionRegistry, SessionStatus,
};
use calls::{CallEvidence, estimated_cost, reported_fields};
pub use calls::{CallObservation, EstimatedUsage, UsageCoverage};
pub(crate) use calls::{SUMMARY_ID, mark_usage_fields, persist as persist_call};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use tracing::warn;

/// Filters applied while collecting. All `None` = everything.
#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    /// Only count entries whose timestamp is `>= since`.
    pub since: Option<DateTime<Utc>>,
    /// Only count sessions from this bridge origin.
    pub bridge: Option<BridgeKind>,
    /// Skip sessions marked `Closed`.
    pub active_only: bool,
}

/// Aggregate usage across some slice of sessions. The per-session and
/// per-model breakdowns share the same numbers as the totals — pick whichever
/// view the caller wants to render.
#[derive(Debug, Clone, Serialize)]
pub struct UsageRollup {
    pub sessions_scanned: u32,
    pub sessions_with_usage: u32,
    pub total: UsageTotals,
    pub per_session: Vec<SessionUsage>,
    pub per_model: BTreeMap<String, ModelUsage>,
    pub filter: UsageFilterSummary,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd: f64,
    /// True if at least one entry reported a cost — distinguishes "$0.00"
    /// (no data) from "$0.0001 (one cheap call)".
    pub cost_reported: bool,
    pub estimated: EstimatedUsage,
    pub coverage: UsageCoverage,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionUsage {
    pub session_db_id: String,
    pub name: Option<String>,
    /// Lowercase bridge tag (`"cli"`, `"tui"`, …) — matches
    /// `BridgeKind::as_str()`. Stringified here so JSON consumers don't
    /// depend on the catalog enum's Rust variant naming. JSON key stays
    /// `gateway` to preserve the `chaz usage --json` output contract.
    #[serde(rename = "gateway")]
    pub bridge: String,
    pub created_at: Option<DateTime<Utc>>,
    pub status: SessionStatus,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelUsage {
    pub calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_usd: f64,
    pub cost_reported: bool,
    pub estimated: EstimatedUsage,
    pub coverage: UsageCoverage,
}

/// Echo of the applied filter for output rendering. Strings rather than the
/// raw filter types so JSON consumers don't need to know our enum encoding.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageFilterSummary {
    pub since: Option<DateTime<Utc>>,
    /// JSON key stays `gateway` to preserve the `chaz usage --json` contract.
    #[serde(rename = "gateway")]
    pub bridge: Option<String>,
    pub active_only: bool,
}

impl UsageTotals {
    fn record(&mut self, m: &ResponseMetadata) {
        self.calls += 1;
        let (prompt, completion) = reported_fields(Some(m));
        self.coverage.cached_reported += u32::from(m.usage.cached_tokens.is_some());
        self.coverage.prompt_reported += u32::from(prompt);
        self.coverage.completion_reported += u32::from(completion);
        self.prompt_tokens += m.usage.prompt_tokens as u64;
        self.completion_tokens += m.usage.completion_tokens as u64;
        self.cached_tokens += m.usage.cached_tokens.unwrap_or(0) as u64;
        self.cache_creation_tokens += m.usage.cache_creation_tokens.unwrap_or(0) as u64;
        self.reasoning_tokens += m.usage.reasoning_tokens.unwrap_or(0) as u64;
        if let Some(c) = m.usage.cost_usd {
            self.cost_usd += c;
            self.cost_reported = true;
            self.coverage.cost_reported += 1;
        }
    }

    fn observe(&mut self, call: &CallObservation) {
        if let Some(metadata) = &call.metadata {
            self.record(metadata);
        } else {
            self.calls += 1;
        }
        let (prompt_reported, completion_reported) = reported_fields(call.metadata.as_ref());
        if !prompt_reported {
            let known = call
                .metadata
                .as_ref()
                .map_or(0, |m| u64::from(m.usage.prompt_tokens));
            self.estimated.prompt_tokens += call.input_tokens.saturating_sub(known);
            self.coverage.prompt_estimated += 1;
        }
        if !completion_reported && let Some(output) = call.output_tokens {
            self.estimated.completion_tokens += output;
            self.coverage.completion_estimated += 1;
        }
        if call
            .metadata
            .as_ref()
            .and_then(|m| m.usage.cost_usd)
            .is_none()
        {
            let prompt = if prompt_reported {
                call.metadata.as_ref().unwrap().usage.prompt_tokens as u64
            } else {
                call.input_tokens
            };
            let output = if completion_reported {
                Some(call.metadata.as_ref().unwrap().usage.completion_tokens as u64)
            } else {
                call.output_tokens
            };
            let (cost, full) = estimated_cost(call, prompt, output);
            if let Some(cost) = cost {
                self.estimated.cost_usd += cost;
                self.coverage.cost_estimated += 1;
                self.coverage.cost_partial += u32::from(!full);
            }
        }
    }

    fn add(&mut self, other: &Self) {
        self.calls += other.calls;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cost_usd += other.cost_usd;
        self.cost_reported |= other.cost_reported;
        self.estimated.prompt_tokens += other.estimated.prompt_tokens;
        self.estimated.completion_tokens += other.estimated.completion_tokens;
        self.estimated.cost_usd += other.estimated.cost_usd;
        self.coverage.add(&other.coverage);
    }

    pub fn cost_text(&self) -> String {
        cost_text(self.calls, self.cost_usd, &self.estimated, &self.coverage)
    }
    pub fn prompt_text(&self) -> String {
        token_text(
            self.prompt_tokens,
            self.estimated.prompt_tokens,
            self.calls,
            self.coverage.prompt_reported,
            self.coverage.prompt_estimated,
        )
    }
    pub fn completion_text(&self) -> String {
        token_text(
            self.completion_tokens,
            self.estimated.completion_tokens,
            self.calls,
            self.coverage.completion_reported,
            self.coverage.completion_estimated,
        )
    }
}

/// Last observed model call for one agent. The requested identity determines
/// which input budget applies; the returned wire model remains provenance.
#[derive(Debug, Clone)]
pub struct LastCall {
    pub requested_model: String,
    pub wire_model: Option<String>,
    pub context_tokens: Option<u32>,
    pub timestamp: DateTime<Utc>,
    pub context_estimated: bool,
}

/// Presentation telemetry reconstructed from the same snapshot as the entries.
/// No activity lease or in-memory runtime is required, so interrupted calls and
/// reconnects retain the usage that was actually reported.
#[derive(Debug, Clone, Default)]
pub struct SessionTelemetry {
    pub totals: UsageTotals,
    pub last_calls: BTreeMap<String, LastCall>,
    pub per_model: BTreeMap<String, ModelUsage>,
}

impl SessionTelemetry {
    /// Legacy turn summaries still count, but identified summaries are replaced
    /// by their per-call transcript, never added on top of it.
    pub fn from_entries(entries: &[SessionEntry]) -> Self {
        let mut telemetry = Self::default();
        for entry in entries {
            if !matches!(entry.entry_type, EntryType::Message | EntryType::Error)
                || entry
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m.turn_attempt_id.is_some())
            {
                continue;
            }
            telemetry.record_legacy_entry(entry);
        }
        telemetry
    }

    fn record_legacy_entry(&mut self, entry: &SessionEntry) {
        let metadata = entry.metadata.as_ref();
        if let Some(metadata) = metadata {
            self.totals.record(metadata);
            record_model(&mut self.per_model, &metadata.model, |t| t.record(metadata));
        }
        if entry.entry_type == EntryType::Message {
            self.last_calls.insert(
                entry.sender.clone(),
                LastCall {
                    requested_model: metadata.map(|m| m.model.clone()).unwrap_or_default(),
                    wire_model: metadata.map(|m| m.model.clone()),
                    context_tokens: metadata.and_then(|m| m.context_tokens).filter(|&n| n > 0),
                    timestamp: entry.timestamp,
                    context_estimated: false,
                },
            );
        }
    }
}

/// Observe completed provider calls, including tool iterations and a terminal
/// call whose final Message is not yet published. Reads are pinned so a racing
/// finalization cannot expose both a call and an unmarked turn summary.
pub async fn telemetry_at_snapshot(
    database: &eidetica::Database,
    snapshot: &eidetica::Snapshot,
    entries: &[SessionEntry],
) -> anyhow::Result<SessionTelemetry> {
    telemetry_filtered_at_snapshot(database, snapshot, entries, &UsageFilter::default()).await
}

pub(crate) async fn telemetry_filtered_at_snapshot(
    database: &eidetica::Database,
    snapshot: &eidetica::Snapshot,
    entries: &[SessionEntry],
    filter: &UsageFilter,
) -> anyhow::Result<SessionTelemetry> {
    use super::{TURN_TRANSCRIPT_STORE, TurnTranscriptMessage, TurnTranscriptRecord};
    use eidetica::store::Table;
    let txn = database.new_transaction_at(snapshot).await?;
    let observations = txn
        .get_store::<Table<CallEvidence>>(calls::STORE)
        .await?
        .search(|_| true)
        .await?;
    let mut calls: BTreeMap<_, _> = observations
        .into_iter()
        .map(|(_, row)| {
            (
                (
                    row.invocation.clone(),
                    row.branch.clone(),
                    row.call.model_sequence,
                ),
                row,
            )
        })
        .collect();
    let observed_keys: std::collections::BTreeSet<_> = calls.keys().cloned().collect();
    let mut records = txn
        .get_store::<Table<TurnTranscriptRecord>>(TURN_TRANSCRIPT_STORE)
        .await?
        .search(|_| true)
        .await?;
    let identities: BTreeMap<_, _> = records
        .iter()
        .filter_map(|(_, r)| {
            r.identity
                .as_ref()
                .map(|i| (r.attempt_id.clone(), i.clone()))
        })
        .collect();
    records.sort_by_key(|(_, r)| (r.timestamp, r.sequence));
    for (_, record) in records {
        let Some(identity) = record
            .identity
            .or_else(|| identities.get(&record.attempt_id).cloned())
        else {
            continue;
        };
        let TurnTranscriptMessage::ModelResponse {
            model_sequence,
            metadata,
            ..
        } = record.message
        else {
            continue;
        };
        let key = (
            record.attempt_id.clone(),
            identity.execution_id.clone(),
            model_sequence,
        );
        // The new snapshot already covers this call; its transcript is not an
        // additional bill. Old identified transcript calls remain readable.
        if observed_keys.contains(&key) {
            continue;
        }
        let key = (
            format!("transcript:{}", record.attempt_id),
            identity.execution_id.clone(),
            record.sequence,
        );
        calls.entry(key).or_insert_with(|| CallEvidence {
            local_estimate: false,
            invocation: record.attempt_id,
            branch: identity.execution_id,
            agent: identity.agent,
            call: CallObservation {
                model_sequence,
                timestamp: record.timestamp,
                requested_model: identity.requested_model,
                request_budget_tokens: None,
                input_tokens: 0,
                output_tokens: None,
                prices: None,
                metadata,
                responded: true,
            },
        });
    }
    let identities: std::collections::BTreeSet<_> =
        calls.values().map(|r| r.invocation.as_str()).collect();
    let mut telemetry = SessionTelemetry::default();
    for entry in entries {
        if !include_entry(entry, filter) {
            continue;
        }
        let metadata = entry.metadata.as_ref();
        if metadata.is_some_and(|m| {
            m.turn_attempt_id
                .as_deref()
                .is_some_and(|id| identities.contains(id))
                || m.extra
                    .get(SUMMARY_ID)
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| identities.contains(id))
        }) {
            continue;
        }
        telemetry.record_legacy_entry(entry);
    }
    let mut rows: Vec<_> = calls.into_values().collect();
    rows.sort_by_key(|r| {
        (
            r.call.timestamp,
            r.call.model_sequence,
            r.invocation.clone(),
            r.branch.clone(),
        )
    });
    for row in rows {
        let call = row.call;
        if filter.since.is_some_and(|since| call.timestamp < since) {
            continue;
        }
        // Legacy transcript calls have no local request estimate. Do not turn
        // their absent evidence into a zero-token estimate.
        let legacy = !row.local_estimate;
        if legacy {
            if let Some(m) = &call.metadata {
                telemetry.totals.record(m);
            } else {
                telemetry.totals.calls += 1;
            }
        } else {
            telemetry.totals.observe(&call);
        }
        let model = call
            .metadata
            .as_ref()
            .map(|m| m.model.as_str())
            .filter(|m| !m.is_empty())
            .unwrap_or(&call.requested_model);
        record_model(&mut telemetry.per_model, model, |t| {
            if legacy {
                if let Some(m) = &call.metadata {
                    t.record(m);
                } else {
                    t.calls += 1;
                }
            } else {
                t.observe(&call);
            }
        });
        if telemetry
            .last_calls
            .get(&row.agent)
            .is_none_or(|last| last.timestamp <= call.timestamp)
        {
            let reported = reported_fields(call.metadata.as_ref()).0;
            let context_tokens = if reported {
                call.metadata
                    .as_ref()
                    .map(|m| m.usage.prompt_tokens)
                    .filter(|&n| !legacy || n > 0)
            } else if legacy {
                None
            } else {
                u32::try_from(call.input_tokens).ok()
            };
            telemetry.last_calls.insert(
                row.agent,
                LastCall {
                    requested_model: call.requested_model,
                    wire_model: call.metadata.as_ref().map(|m| m.model.clone()),
                    context_tokens,
                    timestamp: call.timestamp,
                    context_estimated: !reported && !legacy,
                },
            );
        }
    }
    Ok(telemetry)
}

fn record_model(
    models: &mut BTreeMap<String, ModelUsage>,
    model: &str,
    record: impl FnOnce(&mut UsageTotals),
) {
    if model.is_empty() {
        return;
    }
    let row = models.entry(model.to_string()).or_default();
    let mut total = UsageTotals {
        calls: row.calls,
        prompt_tokens: row.prompt_tokens,
        completion_tokens: row.completion_tokens,
        cost_usd: row.cost_usd,
        cost_reported: row.cost_reported,
        estimated: row.estimated.clone(),
        coverage: row.coverage.clone(),
        ..Default::default()
    };
    record(&mut total);
    row.calls = total.calls;
    row.prompt_tokens = total.prompt_tokens;
    row.completion_tokens = total.completion_tokens;
    row.cost_usd = total.cost_usd;
    row.cost_reported = total.cost_reported;
    row.estimated = total.estimated;
    row.coverage = total.coverage;
}

impl UsageCoverage {
    fn add(&mut self, other: &Self) {
        self.cached_reported += other.cached_reported;
        self.prompt_reported += other.prompt_reported;
        self.completion_reported += other.completion_reported;
        self.cost_reported += other.cost_reported;
        self.prompt_estimated += other.prompt_estimated;
        self.completion_estimated += other.completion_estimated;
        self.cost_estimated += other.cost_estimated;
        self.cost_partial += other.cost_partial;
    }
}
fn token_text(reported: u64, estimated: u64, calls: u32, reports: u32, estimates: u32) -> String {
    if reports == 0 && estimates == 0 {
        return "unknown".into();
    }
    let value = if estimates > 0 {
        format!(
            "~{} ({reported} reported + ~{estimated})",
            reported + estimated
        )
    } else {
        reported.to_string()
    };
    if reports + estimates < calls {
        format!("{value} partial")
    } else {
        value
    }
}
fn cost_text(
    calls: u32,
    reported: f64,
    estimated: &EstimatedUsage,
    coverage: &UsageCoverage,
) -> String {
    let mut text = if coverage.cost_estimated > 0 {
        format!(
            "~${:.4} (${reported:.4} reported + ~${:.4})",
            reported + estimated.cost_usd,
            estimated.cost_usd
        )
    } else if coverage.cost_reported > 0 {
        format!("${reported:.4}")
    } else {
        "cost unknown".into()
    };
    if coverage.cost_reported + coverage.cost_estimated < calls || coverage.cost_partial > 0 {
        text.push_str(" partial");
    }
    text
}

/// Walk every session in the catalog and aggregate `ResponseMetadata`.
/// Unreadable sessions are logged and skipped rather than failing the whole
/// rollup — the goal is "what we can see right now", not strict correctness.
pub async fn collect_usage(
    registry: &SessionRegistry,
    filter: &UsageFilter,
) -> anyhow::Result<UsageRollup> {
    let indices = registry.list_sessions().await?;

    let mut sessions_scanned = 0u32;
    let mut sessions_with_usage = 0u32;
    let mut total = UsageTotals::default();
    let mut per_session: Vec<SessionUsage> = Vec::new();
    let mut per_model: BTreeMap<String, ModelUsage> = BTreeMap::new();

    for index in indices {
        if let Some(g) = filter.bridge
            && index.bridge != g
        {
            continue;
        }
        if filter.active_only && matches!(index.status, SessionStatus::Closed) {
            continue;
        }
        sessions_scanned += 1;

        let collected: anyhow::Result<_> = async {
            let (conv_id, db) = registry.open_session(&index.session_db_id).await?;
            let session = Session::new(conv_id, db.clone()).await;
            let meta = session.read_meta().await;
            let snapshot = db.snapshot().await?;
            let entries = Session::entries_at_snapshot(&db, &snapshot).await?;
            let telemetry =
                telemetry_filtered_at_snapshot(&db, &snapshot, &entries, filter).await?;
            for (model, row) in telemetry.per_model {
                record_model(&mut per_model, &model, |t| {
                    t.add(&UsageTotals {
                        calls: row.calls,
                        prompt_tokens: row.prompt_tokens,
                        completion_tokens: row.completion_tokens,
                        cost_usd: row.cost_usd,
                        cost_reported: row.cost_reported,
                        estimated: row.estimated,
                        coverage: row.coverage,
                        ..Default::default()
                    })
                });
            }
            let totals = telemetry.totals;
            Ok((meta.name, totals))
        }
        .await;
        let (name, totals) = match collected {
            Ok(result) => result,
            Err(e) => {
                warn!(
                    session_db_id = %index.session_db_id,
                    "usage: session unreadable, skipping: {e}"
                );
                continue;
            }
        };

        if totals.calls > 0 {
            sessions_with_usage += 1;
        }

        total.add(&totals);

        per_session.push(SessionUsage {
            session_db_id: index.session_db_id.clone(),
            name,
            bridge: index.bridge.as_str().to_string(),
            created_at: index.created_at,
            status: index.status,
            totals,
        });
    }

    // Sort by the displayed cost (reported plus labeled estimates), then calls.
    per_session.sort_by(|a, b| {
        let cost_cmp = (b.totals.cost_usd + b.totals.estimated.cost_usd)
            .partial_cmp(&(a.totals.cost_usd + a.totals.estimated.cost_usd))
            .unwrap_or(std::cmp::Ordering::Equal);
        if cost_cmp != std::cmp::Ordering::Equal {
            return cost_cmp;
        }
        b.totals.calls.cmp(&a.totals.calls)
    });

    Ok(UsageRollup {
        sessions_scanned,
        sessions_with_usage,
        total,
        per_session,
        per_model,
        filter: UsageFilterSummary {
            since: filter.since,
            bridge: filter.bridge.map(|g| g.as_str().to_string()),
            active_only: filter.active_only,
        },
    })
}

/// Tally recorded message and stopped-turn metadata in entry order. Session selection stays
/// with the caller; sharing the model accumulator preserves cost arithmetic.
#[cfg(test)]
pub(crate) fn collect_session_usage(
    entries: &[SessionEntry],
    filter: &UsageFilter,
    per_model: &mut BTreeMap<String, ModelUsage>,
) -> UsageTotals {
    let mut totals = UsageTotals::default();
    for entry in entries {
        if !include_entry(entry, filter) {
            continue;
        }
        let Some(m) = &entry.metadata else { continue };
        totals.record(m);
        record_model(per_model, &m.model, |t| t.record(m));
    }
    totals
}

fn include_entry(entry: &SessionEntry, filter: &UsageFilter) -> bool {
    if !matches!(entry.entry_type, EntryType::Message | EntryType::Error) {
        return false;
    }
    if let Some(since) = filter.since
        && entry.timestamp < since
    {
        return false;
    }
    true
}

/// Render a `UsageRollup` as human-readable plain text. Used by `/costs` and
/// the default `chaz usage` output.
pub fn render_text(r: &UsageRollup) -> String {
    let mut out = String::new();
    let scope = describe_filter(&r.filter);
    out.push_str(&format!(
        "LLM usage{scope}\n  Sessions: {} scanned, {} with recorded usage\n",
        r.sessions_scanned, r.sessions_with_usage,
    ));
    if r.total.calls == 0 {
        out.push_str("  (no entries with metadata)\n");
        return out;
    }
    let cost = format!(" | {}", r.total.cost_text());
    let cached = if r.total.cached_tokens > 0 {
        format!(" ({} cached)", r.total.cached_tokens)
    } else {
        String::new()
    };
    out.push_str(&format!(
        "  Total: {} call{} | {} prompt + {} completion{cached}{cost}\n",
        r.total.calls,
        if r.total.calls == 1 { "" } else { "s" },
        r.total.prompt_text(),
        r.total.completion_text(),
    ));

    if !r.per_model.is_empty() {
        let mut rows: Vec<(&String, &ModelUsage)> = r.per_model.iter().collect();
        rows.sort_by(|a, b| {
            let cost_cmp = (b.1.cost_usd + b.1.estimated.cost_usd)
                .partial_cmp(&(a.1.cost_usd + a.1.estimated.cost_usd))
                .unwrap_or(std::cmp::Ordering::Equal);
            if cost_cmp != std::cmp::Ordering::Equal {
                return cost_cmp;
            }
            b.1.calls.cmp(&a.1.calls)
        });
        out.push_str("\nBy model:\n");
        for (name, u) in rows {
            let cost = format!(
                "  {}",
                cost_text(u.calls, u.cost_usd, &u.estimated, &u.coverage)
            );
            out.push_str(&format!(
                "  {name:<40} {:>4} call{}{cost}\n",
                u.calls,
                if u.calls == 1 { "" } else { "s" },
            ));
        }
    }

    let top: Vec<&SessionUsage> = r
        .per_session
        .iter()
        .filter(|s| s.totals.calls > 0)
        .take(10)
        .collect();
    if !top.is_empty() {
        out.push_str("\nTop sessions:\n");
        for s in top {
            let label = s.name.clone().unwrap_or_else(|| short_id(&s.session_db_id));
            let cost = format!("  {}", s.totals.cost_text());
            out.push_str(&format!(
                "  {label:<32} [{:<6}] {:>4} call{}{cost}\n",
                s.bridge,
                s.totals.calls,
                if s.totals.calls == 1 { "" } else { "s" },
            ));
        }
    }

    out
}

fn describe_filter(f: &UsageFilterSummary) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(since) = f.since {
        parts.push(format!("since {}", since.format("%Y-%m-%d %H:%M UTC")));
    }
    if let Some(g) = &f.bridge {
        parts.push(format!("bridge={g}"));
    }
    if f.active_only {
        parts.push("active sessions only".to_string());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn short_id(s: &str) -> String {
    let tail = s.rsplit(':').next().unwrap_or(s);
    tail.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::TokenUsage;
    use chrono::TimeZone;

    fn reported_json(mut value: serde_json::Value) -> serde_json::Value {
        if let Some(object) = value.as_object_mut() {
            object.remove("estimated");
            object.remove("coverage");
            for child in object.values_mut() {
                *child = reported_json(child.take());
            }
        } else if let Some(array) = value.as_array_mut() {
            for child in array {
                *child = reported_json(child.take());
            }
        }
        value
    }

    fn mk_entry(ts: DateTime<Utc>, model: &str, cost: Option<f64>) -> SessionEntry {
        SessionEntry {
            sender: "agent".to_string(),
            content: String::new(),
            timestamp: ts,
            entry_type: EntryType::Message,
            metadata: Some(ResponseMetadata {
                model: model.to_string(),
                provider: None,
                response_id: None,
                usage: TokenUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cached_tokens: Some(10),
                    cache_creation_tokens: None,
                    reasoning_tokens: None,
                    cost_usd: cost,
                },
                turn_attempt_id: None,
                context_tokens: None,
                extra: Default::default(),
            }),
            routing: None,
        }
    }

    #[test]
    fn collect_session_usage_sums_metadata_and_filters_by_since() {
        let early = Utc.with_ymd_and_hms(2026, 5, 10, 0, 0, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2026, 5, 13, 0, 0, 0).unwrap();
        let entries = vec![
            mk_entry(early, "haiku", Some(0.001)),
            mk_entry(late, "haiku", Some(0.002)),
            mk_entry(late, "opus", None),
        ];

        let all = collect_session_usage(&entries, &UsageFilter::default(), &mut BTreeMap::new());
        assert_eq!(all.calls, 3);
        assert_eq!(all.prompt_tokens, 300);
        assert!((all.cost_usd - 0.003).abs() < 1e-9);
        assert!(all.cost_reported);

        let recent = collect_session_usage(
            &entries,
            &UsageFilter {
                since: Some(Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap()),
                ..Default::default()
            },
            &mut BTreeMap::new(),
        );
        assert_eq!(recent.calls, 2, "since-filter drops earlier entries");
    }

    #[test]
    fn collect_session_usage_counts_stopped_turn_metadata() {
        let mut stopped = mk_entry(Utc::now(), "haiku", Some(0.05));
        stopped.entry_type = EntryType::Error;
        let t = collect_session_usage(&[stopped], &UsageFilter::default(), &mut BTreeMap::new());
        assert_eq!(t.calls, 1);
        assert_eq!(t.prompt_tokens, 100);
        assert_eq!(t.cost_usd, 0.05);
    }

    #[tokio::test]
    async fn telemetry_reconciles_legacy_resumed_and_interrupted_calls_at_pinned_snapshot() {
        use crate::session::{
            TurnCallIdentity, TurnRequestId, TurnTranscriptMessage, TurnTranscriptRecord,
        };
        let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
        let mut session =
            Session::new(crate::types::ConversationId("telemetry".into()), db.clone()).await;
        let mut legacy = mk_entry(Utc::now(), "legacy", Some(0.01));
        legacy.sender = "host".into();
        session.add_entry(legacy).await.unwrap();
        let request = session
            .add_entry(crate::session::SessionEntry {
                sender: "user".into(),
                content: "fixture".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        let attempt = session.start_turn_attempt(request).await.unwrap();
        let call = |sequence, identity, prompt, terminal| TurnTranscriptRecord {
            request_id: attempt.request_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            sequence,
            timestamp: Utc::now(),
            identity,
            message: TurnTranscriptMessage::ModelResponse {
                model_sequence: sequence,
                content: None,
                tool_calls: Vec::new(),
                provider_extra: Default::default(),
                metadata: Some(ResponseMetadata {
                    model: "wire-model".into(),
                    usage: TokenUsage {
                        prompt_tokens: prompt,
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                terminal,
            },
        };
        // First call predates identification; resumed call knows the request.
        session
            .append_turn_transcript(call(0, None, 10, false), Vec::new())
            .await
            .unwrap();
        let identity = TurnCallIdentity {
            agent: "host".into(),
            requested_model: "alias".into(),
            execution_id: None,
        };
        let terminal = call(1, Some(identity.clone()), 20, true);
        session
            .append_turn_transcript(terminal.clone(), Vec::new())
            .await
            .unwrap();
        let before = db.snapshot().await.unwrap();
        let before_entries = Session::entries_at_snapshot(&db, &before).await.unwrap();
        let live = telemetry_at_snapshot(&db, &before, &before_entries)
            .await
            .unwrap();
        assert_eq!(live.totals.prompt_tokens, 130);
        let mut finalized = mk_entry(Utc::now(), "wire-model", Some(0.02));
        finalized.metadata.as_mut().unwrap().usage.prompt_tokens = 30;
        session
            .complete_turn_attempt_with_transcript(&attempt, Some(finalized), Some(terminal))
            .await
            .unwrap();
        let now = db.snapshot().await.unwrap();
        let entries = Session::entries_at_snapshot(&db, &now).await.unwrap();
        assert_eq!(
            telemetry_at_snapshot(&db, &now, &entries)
                .await
                .unwrap()
                .totals
                .prompt_tokens,
            130
        );
        assert_eq!(
            telemetry_at_snapshot(&db, &before, &before_entries)
                .await
                .unwrap()
                .totals
                .prompt_tokens,
            130
        );
        // An interrupted or failed attempt's reported usage is not lost just
        // because it never acquired a final Message or still-live heartbeat.
        let mut interrupted = call(0, Some(identity.clone()), 7, false);
        interrupted.attempt_id = "interrupted".into();
        interrupted.request_id = TurnRequestId::parse("interrupted-request");
        session
            .append_turn_transcript(interrupted.clone(), Vec::new())
            .await
            .unwrap();
        let now = db.snapshot().await.unwrap();
        let entries = Session::entries_at_snapshot(&db, &now).await.unwrap();
        let retained = telemetry_at_snapshot(&db, &now, &entries).await.unwrap();
        assert_eq!(retained.totals.prompt_tokens, 137);
        assert_eq!(retained.last_calls["host"].context_tokens, Some(7));
        interrupted.sequence = 1;
        interrupted.timestamp = Utc::now();
        if let TurnTranscriptMessage::ModelResponse { metadata, .. } = &mut interrupted.message {
            *metadata = None;
        }
        session
            .append_turn_transcript(interrupted, Vec::new())
            .await
            .unwrap();
        let now = db.snapshot().await.unwrap();
        let missing = telemetry_at_snapshot(&db, &now, &entries).await.unwrap();
        assert_eq!(missing.totals.prompt_tokens, 137);
        assert_eq!(missing.last_calls["host"].context_tokens, None);

        // Competitors share an attempt and sequence, but each billed call has
        // its own branch key. Re-observing one record must not add another call.
        for (branch, prompt) in [("a", 11), ("b", 13)] {
            let mut parallel = call(
                0,
                Some(TurnCallIdentity {
                    execution_id: Some(branch.into()),
                    ..identity.clone()
                }),
                prompt,
                true,
            );
            parallel.attempt_id = "parallel-attempt".into();
            for _ in 0..2 {
                session
                    .append_turn_transcript(parallel.clone(), Vec::new())
                    .await
                    .unwrap();
            }
        }
        let now = db.snapshot().await.unwrap();
        let retained = telemetry_at_snapshot(&db, &now, &entries).await.unwrap();
        assert_eq!(retained.totals.prompt_tokens, 161);
        assert_eq!(
            retained.totals.calls, 7,
            "missing legacy call counts toward absent coverage"
        );
        assert_eq!(retained.last_calls["host"].context_tokens, Some(13));
    }

    #[test]
    fn collect_session_usage_skips_non_message_entries() {
        let ts = Utc.with_ymd_and_hms(2026, 5, 13, 0, 0, 0).unwrap();
        let mut tool_entry = mk_entry(ts, "haiku", Some(0.05));
        tool_entry.entry_type = EntryType::ToolCall;
        let entries = vec![tool_entry];

        let t = collect_session_usage(&entries, &UsageFilter::default(), &mut BTreeMap::new());
        assert_eq!(t.calls, 0, "only Message entries count");
    }

    #[test]
    fn tally_keeps_optional_tokens_cost_coverage_and_inclusive_since() {
        let since = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        let mut entries = vec![
            mk_entry(since - chrono::Duration::nanoseconds(1), "old", Some(99.0)),
            mk_entry(since, "zero", Some(0.0)),
            mk_entry(since, "missing", None),
            mk_entry(since, "zero", None),
            mk_entry(since, "", Some(0.5)),
        ];
        let usage = &mut entries[1].metadata.as_mut().unwrap().usage;
        usage.cache_creation_tokens = Some(7);
        usage.reasoning_tokens = Some(3);
        let mut no_metadata = mk_entry(since, "ignored", Some(99.0));
        no_metadata.metadata = None;
        entries.push(no_metadata);
        let mut audit = mk_entry(since, "ignored", Some(99.0));
        audit.entry_type = EntryType::ToolResult;
        entries.push(audit);
        let mut models = BTreeMap::new();
        let total = collect_session_usage(
            &entries,
            &UsageFilter {
                since: Some(since),
                ..Default::default()
            },
            &mut models,
        );
        assert_eq!(
            reported_json(serde_json::to_value(total).unwrap()),
            serde_json::json!({
                "calls": 4, "prompt_tokens": 400, "completion_tokens": 200,
                "cached_tokens": 40, "cache_creation_tokens": 7, "reasoning_tokens": 3,
                "cost_usd": 0.5, "cost_reported": true
            })
        );
        assert_eq!(
            reported_json(serde_json::to_value(models).unwrap()),
            serde_json::json!({
                "zero": {"calls": 2, "prompt_tokens": 200, "completion_tokens": 100, "cost_usd": 0.0, "cost_reported": true},
                "missing": {"calls": 1, "prompt_tokens": 100, "completion_tokens": 50, "cost_usd": 0.0, "cost_reported": false}
            })
        );
        let empty = collect_session_usage(&[], &Default::default(), &mut BTreeMap::new());
        assert_eq!(empty.calls, 0);
        assert!(!empty.cost_reported);
    }

    #[test]
    fn per_model_cost_additions_stay_in_entry_order_across_sessions() {
        let ts = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        let mut models = BTreeMap::new();
        collect_session_usage(
            &[mk_entry(ts, "model", Some(1e16))],
            &Default::default(),
            &mut models,
        );
        collect_session_usage(
            &[
                mk_entry(ts, "model", Some(1.0)),
                mk_entry(ts, "model", Some(1.0)),
            ],
            &Default::default(),
            &mut models,
        );
        // Regrouping the second session into a subtotal would produce 1e16 + 2.
        assert_eq!(models["model"].cost_usd, (1e16_f64 + 1.0) + 1.0);
    }

    #[tokio::test]
    async fn operator_rollup_json_and_text_contract() {
        let (_instance, registry) = crate::test_support::fresh_session_registry().await;
        let (id, db) = registry.create_session(Some("cli")).await.unwrap();
        let mut session = Session::new(id, db).await;
        session
            .update_meta(|m| m.name = Some("fixture".into()))
            .await
            .unwrap();
        let ts = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        session
            .add_entry(mk_entry(ts, "model", Some(0.25)))
            .await
            .unwrap();
        let rollup = collect_usage(&registry, &Default::default()).await.unwrap();
        assert_eq!(
            reported_json(serde_json::to_value(&rollup.per_model["model"]).unwrap()),
            serde_json::json!({
                "calls": 1, "prompt_tokens": 100, "completion_tokens": 50,
                "cost_usd": 0.25, "cost_reported": true
            })
        );
        let mut wire = reported_json(serde_json::to_value(&rollup).unwrap());
        // Only the generated DB ID and catalog timestamp are nondeterministic.
        wire["per_session"][0]["session_db_id"] = serde_json::json!("<session>");
        wire["per_session"][0]["created_at"] = serde_json::json!("<created>");
        assert_eq!(
            wire,
            serde_json::json!({
                "sessions_scanned": 1, "sessions_with_usage": 1,
                "total": {"calls": 1, "prompt_tokens": 100, "completion_tokens": 50, "cached_tokens": 10,
                    "cache_creation_tokens": 0, "reasoning_tokens": 0, "cost_usd": 0.25, "cost_reported": true},
                "per_session": [{"session_db_id": "<session>", "name": "fixture", "gateway": "cli", "created_at": "<created>",
                    "status": "Active", "totals": {"calls": 1, "prompt_tokens": 100, "completion_tokens": 50, "cached_tokens": 10,
                        "cache_creation_tokens": 0, "reasoning_tokens": 0, "cost_usd": 0.25, "cost_reported": true}}],
                "per_model": {"model": {"calls": 1, "prompt_tokens": 100, "completion_tokens": 50, "cost_usd": 0.25, "cost_reported": true}},
                "filter": {"since": null, "gateway": null, "active_only": false}
            })
        );
        assert_eq!(
            render_text(&rollup),
            concat!(
                "LLM usage\n  Sessions: 1 scanned, 1 with recorded usage\n",
                "  Total: 1 call | 100 prompt + 50 completion (10 cached) | $0.2500\n",
                "\nBy model:\n  model                                       1 call  $0.2500\n",
                "\nTop sessions:\n  fixture                          [cli   ]    1 call  $0.2500\n"
            )
        );
    }
}
