//! Cross-session LLM usage aggregation.
//!
//! Walks the user-central session catalog, opens each session, and sums
//! `ResponseMetadata` across every assistant entry. Used by:
//! - `/costs` slash command (TUI/Matrix) for an interactive rollup
//! - `chaz usage` CLI subcommand for headless/scripted rollups
//!
//! Both consumers reuse the same `collect_usage()` and rendering helpers;
//! they differ only in the surrounding I/O.

use crate::runtime::ResponseMetadata;
use crate::session::{
    BridgeKind, EntryType, Session, SessionEntry, SessionRegistry, SessionStatus,
};
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
        self.prompt_tokens += m.usage.prompt_tokens as u64;
        self.completion_tokens += m.usage.completion_tokens as u64;
        self.cached_tokens += m.usage.cached_tokens.unwrap_or(0) as u64;
        self.cache_creation_tokens += m.usage.cache_creation_tokens.unwrap_or(0) as u64;
        self.reasoning_tokens += m.usage.reasoning_tokens.unwrap_or(0) as u64;
        if let Some(c) = m.usage.cost_usd {
            self.cost_usd += c;
            self.cost_reported = true;
        }
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
}

/// Presentation telemetry reconstructed from the same snapshot as the entries.
/// No activity lease or in-memory runtime is required, so interrupted calls and
/// reconnects retain the usage that was actually reported.
#[derive(Debug, Clone, Default)]
pub struct SessionTelemetry {
    pub totals: UsageTotals,
    pub last_calls: BTreeMap<String, LastCall>,
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
            if let Some(metadata) = &entry.metadata {
                telemetry.totals.record(metadata);
            }
            if entry.entry_type == EntryType::Message {
                let metadata = entry.metadata.as_ref();
                telemetry.last_calls.insert(
                    entry.sender.clone(),
                    LastCall {
                        requested_model: metadata.map(|m| m.model.clone()).unwrap_or_default(),
                        wire_model: metadata.map(|m| m.model.clone()),
                        context_tokens: metadata.and_then(|m| m.context_tokens).filter(|&n| n > 0),
                        timestamp: entry.timestamp,
                    },
                );
            }
        }
        telemetry
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
    use super::{TURN_TRANSCRIPT_STORE, TurnTranscriptMessage, TurnTranscriptRecord};
    use eidetica::store::Table;

    let mut telemetry = SessionTelemetry::from_entries(entries);
    let txn = database.new_transaction_at(snapshot).await?;
    let mut records = txn
        .get_store::<Table<TurnTranscriptRecord>>(TURN_TRANSCRIPT_STORE)
        .await?
        .search(|_| true)
        .await?;
    // A checkpoint may span an upgrade: once the attempt is identified, its
    // earlier raw calls also replace the finalized aggregate's usage.
    let identities: BTreeMap<_, _> = records
        .iter()
        .filter_map(|(_, record)| {
            record
                .identity
                .as_ref()
                .map(|identity| (record.attempt_id.clone(), identity.clone()))
        })
        .collect();
    records.sort_by_key(|(_, record)| (record.timestamp, record.sequence));
    for (_, record) in records {
        let Some(identity) = record
            .identity
            .or_else(|| identities.get(&record.attempt_id).cloned())
        else {
            continue;
        };
        let TurnTranscriptMessage::ModelResponse { metadata, .. } = record.message else {
            continue;
        };
        if let Some(metadata) = &metadata {
            telemetry.totals.record(metadata);
        }
        // A missing usage response supersedes the old numerator, too.
        if telemetry
            .last_calls
            .get(&identity.agent)
            .is_none_or(|last| last.timestamp <= record.timestamp)
        {
            telemetry.last_calls.insert(
                identity.agent,
                LastCall {
                    requested_model: identity.requested_model,
                    wire_model: metadata.as_ref().map(|m| m.model.clone()),
                    context_tokens: metadata
                        .as_ref()
                        .map(|m| m.usage.prompt_tokens)
                        .filter(|&n| n > 0),
                    timestamp: record.timestamp,
                },
            );
        }
    }
    Ok(telemetry)
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

        let (name, totals) = match registry.open_session(&index.session_db_id).await {
            Ok((conv_id, db)) => {
                let session = Session::new(conv_id, db).await;
                let meta = session.read_meta().await;
                let totals = collect_session_usage(session.entries(), filter, &mut per_model);
                (meta.name, totals)
            }
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

        total.calls += totals.calls;
        total.prompt_tokens += totals.prompt_tokens;
        total.completion_tokens += totals.completion_tokens;
        total.cached_tokens += totals.cached_tokens;
        total.cache_creation_tokens += totals.cache_creation_tokens;
        total.reasoning_tokens += totals.reasoning_tokens;
        total.cost_usd += totals.cost_usd;
        total.cost_reported = total.cost_reported || totals.cost_reported;

        per_session.push(SessionUsage {
            session_db_id: index.session_db_id.clone(),
            name,
            bridge: index.bridge.as_str().to_string(),
            created_at: index.created_at,
            status: index.status,
            totals,
        });
    }

    // Sort per_session by cost (when reported) else by call count, descending.
    per_session.sort_by(|a, b| {
        let cost_cmp = b
            .totals
            .cost_usd
            .partial_cmp(&a.totals.cost_usd)
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
        if !m.model.is_empty() {
            let row = per_model.entry(m.model.clone()).or_default();
            row.calls += 1;
            row.prompt_tokens += m.usage.prompt_tokens as u64;
            row.completion_tokens += m.usage.completion_tokens as u64;
            if let Some(c) = m.usage.cost_usd {
                row.cost_usd += c;
                row.cost_reported = true;
            }
        }
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
    let cost = if r.total.cost_reported {
        format!(" | ${:.4}", r.total.cost_usd)
    } else {
        String::new()
    };
    let cached = if r.total.cached_tokens > 0 {
        format!(" ({} cached)", r.total.cached_tokens)
    } else {
        String::new()
    };
    out.push_str(&format!(
        "  Total: {} call{} | {} prompt + {} completion{cached}{cost}\n",
        r.total.calls,
        if r.total.calls == 1 { "" } else { "s" },
        r.total.prompt_tokens,
        r.total.completion_tokens,
    ));

    if !r.per_model.is_empty() {
        let mut rows: Vec<(&String, &ModelUsage)> = r.per_model.iter().collect();
        rows.sort_by(|a, b| {
            let cost_cmp =
                b.1.cost_usd
                    .partial_cmp(&a.1.cost_usd)
                    .unwrap_or(std::cmp::Ordering::Equal);
            if cost_cmp != std::cmp::Ordering::Equal {
                return cost_cmp;
            }
            b.1.calls.cmp(&a.1.calls)
        });
        out.push_str("\nBy model:\n");
        for (name, u) in rows {
            let cost = if u.cost_reported {
                format!("  ${:.4}", u.cost_usd)
            } else {
                String::new()
            };
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
            let cost = if s.totals.cost_reported {
                format!("  ${:.4}", s.totals.cost_usd)
            } else {
                String::new()
            };
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
        assert_eq!(retained.totals.calls, 6);
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
            serde_json::to_value(total).unwrap(),
            serde_json::json!({
                "calls": 4, "prompt_tokens": 400, "completion_tokens": 200,
                "cached_tokens": 40, "cache_creation_tokens": 7, "reasoning_tokens": 3,
                "cost_usd": 0.5, "cost_reported": true
            })
        );
        assert_eq!(
            serde_json::to_value(models).unwrap(),
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
            serde_json::to_value(&rollup.per_model["model"]).unwrap(),
            serde_json::json!({
                "calls": 1, "prompt_tokens": 100, "completion_tokens": 50,
                "cost_usd": 0.25, "cost_reported": true
            })
        );
        let mut wire = serde_json::to_value(&rollup).unwrap();
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
