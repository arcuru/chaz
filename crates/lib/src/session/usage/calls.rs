//! Presentation snapshots, not replayable requests, attempts or billing records.
//! One row per logical call is replaced when a response arrives. No prompt or
//! output bytes are persisted here (including private scheduled instructions).
use crate::backends::ModelInfo;
use crate::runtime::{LLMResponse, ResponseMetadata};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub(super) const STORE: &str = "call_telemetry";
pub(crate) const SUMMARY_ID: &str = "chaz_telemetry_id";
const USAGE_FIELDS: &str = "chaz_usage_fields";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallObservation {
    pub model_sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub requested_model: String,
    pub request_budget_tokens: Option<usize>,
    pub input_tokens: u64,
    pub output_tokens: Option<u64>,
    pub prices: Option<ModelInfo>,
    pub metadata: Option<ResponseMetadata>,
    pub responded: bool,
}

impl CallObservation {
    pub(crate) fn response(&mut self, response: &LLMResponse) {
        let (content, calls, metadata) = match response {
            LLMResponse::Text {
                content, metadata, ..
            } => (Some(content.as_str()), &[][..], metadata),
            LLMResponse::ToolCalls {
                content,
                tool_calls,
                metadata,
                ..
            } => (content.as_deref(), tool_calls.as_slice(), metadata),
        };
        // Only visible returned text and tool names/arguments; no speculative
        // future output or hidden reasoning, nor provider transport IDs.
        self.output_tokens = Some(
            (crate::context::estimate_tokens(content.unwrap_or_default())
                + calls
                    .iter()
                    .map(|c| {
                        crate::context::estimate_tokens(&c.name)
                            + crate::context::estimate_tokens(&c.arguments)
                    })
                    .sum::<usize>()) as u64,
        );
        self.metadata = metadata.clone();
        self.responded = true;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct CallEvidence {
    #[serde(default)]
    pub local_estimate: bool,
    pub invocation: String,
    pub branch: Option<String>,
    pub agent: String,
    pub call: CallObservation,
}

pub(crate) async fn persist(
    db: &eidetica::Database,
    invocation: &str,
    branch: Option<&str>,
    agent: &str,
    call: CallObservation,
) -> Result<(), String> {
    let key = serde_json::to_string(&(invocation, branch, call.model_sequence))
        .map_err(|e| e.to_string())?;
    let txn = db.new_transaction().await.map_err(|e| e.to_string())?;
    txn.get_store::<eidetica::store::Table<CallEvidence>>(STORE)
        .await
        .map_err(|e| e.to_string())?
        .set(
            key,
            CallEvidence {
                local_estimate: true,
                invocation: invocation.into(),
                branch: branch.map(str::to_string),
                agent: agent.into(),
                call,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    txn.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// New backends distinguish omitted fields from explicitly reported zero.
/// Old metadata retains its historical reported-count semantics.
pub(crate) fn mark_usage_fields(
    extra: &mut serde_json::Map<String, serde_json::Value>,
    prompt: bool,
    completion: bool,
) {
    extra.insert(
        USAGE_FIELDS.into(),
        serde_json::json!({"prompt": prompt, "completion": completion}),
    );
}

pub(crate) fn reported_fields(metadata: Option<&ResponseMetadata>) -> (bool, bool) {
    let Some(metadata) = metadata else {
        return (false, false);
    };
    match metadata.extra.get(USAGE_FIELDS) {
        Some(fields) => (
            fields["prompt"].as_bool().unwrap_or(false),
            fields["completion"].as_bool().unwrap_or(false),
        ),
        None => (true, true),
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EstimatedUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_usd: f64,
}

/// Counts of call observations covered by each source. An absent field is
/// `calls - reported - estimated`; cost can be estimated only in part.
#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageCoverage {
    pub cached_reported: u32,
    pub prompt_reported: u32,
    pub completion_reported: u32,
    pub cost_reported: u32,
    pub prompt_estimated: u32,
    pub completion_estimated: u32,
    pub cost_estimated: u32,
    pub cost_partial: u32,
}

pub(super) fn estimated_cost(
    call: &CallObservation,
    prompt: u64,
    output: Option<u64>,
) -> (Option<f64>, bool) {
    let Some(prices) = &call.prices else {
        return (None, false);
    };
    let usage = call.metadata.as_ref().map(|m| &m.usage);
    let cached = usage.and_then(|u| u.cached_tokens).unwrap_or(0) as u64;
    let creation = usage.and_then(|u| u.cache_creation_tokens).unwrap_or(0) as u64;
    let rate = |value: Option<f64>| value.filter(|v| v.is_finite() && *v >= 0.0);
    let input = if cached > prompt || creation > 0 {
        None
    } else if cached > 0 {
        rate(prices.price_input)
            .zip(rate(prices.price_cache_read))
            .map(|(input, cache)| (prompt - cached) as f64 * input + cached as f64 * cache)
    } else {
        // Unknown cache data never creates inferred cache hits. This is the
        // explicitly approximate uncached-input estimate, not a bill.
        rate(prices.price_input).map(|price| prompt as f64 * price)
    };
    let output = output
        .zip(rate(prices.price_output))
        .map(|(tokens, price)| tokens as f64 * price);
    let full = input.is_some() && output.is_some();
    let cost = (input.is_some() || output.is_some())
        .then(|| (input.unwrap_or(0.0) + output.unwrap_or(0.0)) / 1_000_000.0);
    (cost.filter(|cost| cost.is_finite()), full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::TokenUsage;
    use crate::session::usage::{UsageFilter, collect_usage, render_text, telemetry_at_snapshot};
    use crate::session::{EntryType, Session, SessionEntry};

    fn call() -> CallObservation {
        CallObservation {
            model_sequence: 0,
            timestamp: Utc::now(),
            requested_model: "alias".into(),
            request_budget_tokens: Some(10000),
            input_tokens: 1000,
            output_tokens: None,
            prices: Some(ModelInfo {
                id: "alias".into(),
                price_input: Some(2.0),
                price_output: Some(4.0),
                ..Default::default()
            }),
            metadata: None,
            responded: false,
        }
    }
    async fn observe(db: &eidetica::Database) -> crate::session::usage::SessionTelemetry {
        let snapshot = db.snapshot().await.unwrap();
        let entries = Session::entries_at_snapshot(db, &snapshot).await.unwrap();
        telemetry_at_snapshot(db, &snapshot, &entries)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn telemetry_estimates_replace_reports_and_agree_with_filtered_rollups() {
        let (_instance, registry) = crate::test_support::fresh_session_registry().await;
        let (id, db) = registry.create_session(Some("cli")).await.unwrap();
        let mut session = Session::new(id, db.clone()).await;
        let mut call = call();
        persist(&db, "schedule-invocation", None, "host", call.clone())
            .await
            .unwrap();
        let held = db.snapshot().await.unwrap();
        let held_entries = Session::entries_at_snapshot(&db, &held).await.unwrap();
        let pending = observe(&db).await;
        assert_eq!(pending.totals.calls, 1);
        assert_eq!(pending.totals.prompt_tokens, 0);
        assert_eq!(pending.totals.estimated.prompt_tokens, 1000);
        assert_eq!(pending.totals.estimated.completion_tokens, 0);
        assert_eq!(
            pending.totals.coverage.completion_estimated, 0,
            "no future output"
        );
        assert_eq!(pending.totals.coverage.cost_partial, 1);
        assert_eq!(pending.totals.estimated.cost_usd, 0.002);
        assert!(pending.last_calls["host"].context_estimated);
        let rollup = collect_usage(&registry, &Default::default()).await.unwrap();
        assert_eq!(
            serde_json::to_value(&rollup.total).unwrap(),
            serde_json::to_value(&pending.totals).unwrap()
        );
        assert!(render_text(&rollup).contains("~1000"));
        assert!(render_text(&rollup).contains("partial"));
        call.metadata = Some(ResponseMetadata {
            model: "wire".into(),
            usage: TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 10,
                cached_tokens: Some(0),
                cost_usd: Some(0.0),
                ..Default::default()
            },
            ..Default::default()
        });
        call.output_tokens = Some(9);
        call.responded = true;
        // Re-observation replaces the same logical call, including reported zero.
        for _ in 0..2 {
            persist(&db, "schedule-invocation", None, "host", call.clone())
                .await
                .unwrap();
        }
        let mut summary = call.metadata.clone().unwrap();
        summary
            .extra
            .insert(SUMMARY_ID.into(), serde_json::json!("schedule-invocation"));
        session
            .add_entry(SessionEntry {
                sender: "host".into(),
                content: "done".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: Some(summary),
                routing: None,
            })
            .await
            .unwrap();
        let exact = observe(&db).await;
        assert_eq!(exact.totals.calls, 1);
        assert_eq!(exact.totals.completion_tokens, 10);
        assert_eq!(exact.totals.estimated.prompt_tokens, 0);
        assert_eq!(exact.totals.estimated.cost_usd, 0.0);
        assert_eq!(exact.totals.coverage.cost_reported, 1);
        assert_eq!(exact.last_calls["host"].context_tokens, Some(0));
        assert!(!exact.last_calls["host"].context_estimated);
        let rollup = collect_usage(&registry, &Default::default()).await.unwrap();
        assert_eq!(
            serde_json::to_value(&rollup.total).unwrap(),
            serde_json::to_value(&exact.totals).unwrap()
        );
        assert!(!rollup.per_model.contains_key("alias"));
        assert_eq!(rollup.per_model["wire"].calls, 1);
        assert!(!render_text(&rollup).contains('~'));
        let old = telemetry_at_snapshot(&db, &held, &held_entries)
            .await
            .unwrap();
        assert_eq!(old.totals.estimated.prompt_tokens, 1000, "pinned read");
        let empty = collect_usage(
            &registry,
            &UsageFilter {
                since: Some(call.timestamp + chrono::Duration::nanoseconds(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            empty.total.calls, 0,
            "summary must stay suppressed across since boundary"
        );
        let filtered = collect_usage(
            &registry,
            &UsageFilter {
                bridge: Some(crate::session::BridgeKind::Tui),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(filtered.sessions_scanned, 0);
        registry
            .mark_session_closed(&db.root_id().to_string())
            .await
            .unwrap();
        assert_eq!(
            collect_usage(
                &registry,
                &UsageFilter {
                    active_only: true,
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .sessions_scanned,
            0
        );
    }

    #[tokio::test]
    async fn telemetry_partial_usage_unknown_prices_branches_and_agents_remain_distinct() {
        let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
        let mut call = call();
        call.metadata = Some(ResponseMetadata {
            model: "wire".into(),
            usage: TokenUsage {
                prompt_tokens: 40,
                ..Default::default()
            },
            ..Default::default()
        });
        mark_usage_fields(&mut call.metadata.as_mut().unwrap().extra, true, false);
        call.output_tokens = Some(3);
        call.responded = true;
        call.prices = None;
        for (branch, agent) in [("a", "host"), ("b", "guest")] {
            for _ in 0..2 {
                persist(&db, "attempt", Some(branch), agent, call.clone())
                    .await
                    .unwrap();
            }
        }
        let totals = observe(&db).await;
        assert_eq!(totals.totals.calls, 2);
        assert_eq!(totals.totals.prompt_tokens, 80);
        assert_eq!(totals.totals.completion_tokens, 0);
        assert_eq!(totals.totals.estimated.completion_tokens, 6);
        assert_eq!(totals.totals.coverage.prompt_reported, 2);
        assert_eq!(totals.totals.coverage.completion_reported, 0);
        assert_eq!(totals.totals.coverage.cost_estimated, 0);
        assert_eq!(totals.last_calls.len(), 2);
        let json = serde_json::to_value(&totals.totals).unwrap();
        assert_eq!(json["completion_tokens"], 0);
        assert_eq!(json["estimated"]["completion_tokens"], 6);
        assert_eq!(totals.per_model["wire"].calls, 2);
        assert!(totals.totals.cost_text().contains("unknown"));
    }

    #[tokio::test]
    async fn telemetry_partial_native_input_estimates_only_the_unknown_remainder() {
        let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
        let mut call = call();
        call.metadata = Some(ResponseMetadata {
            usage: TokenUsage {
                prompt_tokens: 500,
                cached_tokens: Some(500),
                ..Default::default()
            },
            ..Default::default()
        });
        mark_usage_fields(&mut call.metadata.as_mut().unwrap().extra, false, false);
        persist(&db, "partial-native", None, "host", call)
            .await
            .unwrap();
        let telemetry = observe(&db).await;
        assert_eq!(telemetry.totals.prompt_tokens, 500);
        assert_eq!(telemetry.totals.estimated.prompt_tokens, 500);
        assert_eq!(telemetry.last_calls["host"].context_tokens, Some(1000));
        assert!(telemetry.last_calls["host"].context_estimated);
    }

    #[test]
    fn telemetry_price_cache_and_missing_output_never_invent_data() {
        let mut call = call();
        assert_eq!(estimated_cost(&call, 1000, None), (Some(0.002), false));
        assert_eq!(estimated_cost(&call, 1000, Some(10)), (Some(0.00204), true));
        call.metadata = Some(ResponseMetadata {
            usage: TokenUsage {
                cached_tokens: Some(500),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            estimated_cost(&call, 1000, Some(10)),
            (Some(0.00004), false),
            "unknown cache-read rate"
        );
        call.prices.as_mut().unwrap().price_cache_read = Some(0.5);
        assert_eq!(estimated_cost(&call, 1000, Some(10)), (Some(0.00129), true));
        call.metadata.as_mut().unwrap().usage.cache_creation_tokens = Some(5);
        assert_eq!(
            estimated_cost(&call, 1000, Some(10)),
            (Some(0.00004), false),
            "no invented write price"
        );
        call.prices.as_mut().unwrap().price_output = Some(f64::NAN);
        assert_eq!(estimated_cost(&call, 1000, Some(10)), (None, false));
    }
}
