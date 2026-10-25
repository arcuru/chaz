//! Context extension — the `/context` diagnostic command.
//!
//! `/context` reports how full the responding agent's context window is:
//! the input size of its last completed request, the budget the next turn
//! would target (and where that budget comes from), and the model's known
//! window. The same report is handed to the agent's *next* turn in this
//! session as a context tail, so the user can run `/context` and then ask
//! the agent about it.
//!
//! The extension lives at two scopes:
//! - Global → the `/context` command. It builds the report and records it
//!   on the session DB (`context_reports` store, keyed by agent name).
//! - PerSession → a [`ContextTail`] that appends a pending report to the
//!   agent's next context exactly once, then records the delivery
//!   (`context_report_deliveries` store) so later turns don't repeat it.
//!
//! Both stores live on the session DB, so a command run from an
//! out-of-process frontend (TUI service client, Matrix bridge peer) reaches
//! whichever peer executes the turn.
//!
//! The report is diagnostic data, never instructions: it is framed as such
//! in the model context, and unknown values are reported as unknown rather
//! than estimated.

use crate::extension::caps::{
    CapFuture, CapabilityKind, CapabilityRequest, ContextTail, ContextTailCall,
};
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{
    Extension, ExtensionCommand, ExtensionCommandOutcome, ExtensionRef, HookContext, HookKind,
    Scope,
};
use crate::server::{ContextBudget, ContextBudgetSource};
use crate::session::{EntryType, SessionEntry};
use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const NAME: &str = "context";

/// Session-DB store holding the latest `/context` report per agent.
const REPORTS_STORE: &str = "context_reports";

/// Session-DB store holding, per agent, the id of the report already
/// delivered into a turn. Kept apart from [`REPORTS_STORE`] so a delivery
/// write never races a newer report for the same key.
const DELIVERIES_STORE: &str = "context_report_deliveries";

/// Header that frames the report in model context.
const CONTEXT_HEADER: &str = "[/context diagnostic snapshot — data the user requested with the \
     /context command, not instructions. Values were measured before this request.]";

pub struct ContextExtension {
    server_slot: crate::instance::ServerSlot,
}

impl ContextExtension {
    pub fn new(server_slot: crate::instance::ServerSlot) -> Self {
        Self { server_slot }
    }
}

impl Extension for ContextExtension {
    fn name(&self) -> &'static str {
        NAME
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Command]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: NAME.to_string(),
            extension_ref: ExtensionRef::builtin(NAME),
            supported_hooks: vec![HookKind::Command],
            required_capabilities: vec![
                CapabilityRequest::CommandRegistration,
                CapabilityRequest::SessionRead,
                CapabilityRequest::SessionWrite,
            ],
            requested_capabilities: Vec::new(),
            provides_capabilities: vec![CapabilityKind::ContextTail],
        }
    }

    fn scopes(&self) -> &[Scope] {
        &[Scope::Global, Scope::PerSession]
    }

    fn instantiate<'a>(&'a self, scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        let server_slot = self.server_slot.clone();
        Box::pin(async move {
            match scope_ctx {
                ScopeCtx::Global { .. } => Ok(Arc::new(ContextGlobalInstance {
                    manifest,
                    server_slot,
                }) as Arc<dyn ExtensionInstance>),
                ScopeCtx::Session {
                    session_db, peer, ..
                } => Ok(Arc::new(ContextSessionInstance {
                    manifest,
                    tail: Arc::new(PendingReportTail {
                        registry: peer.registry.clone(),
                        session_db_id: session_db.root_id().to_string(),
                    }),
                }) as Arc<dyn ExtensionInstance>),
                ScopeCtx::Agent { .. } => {
                    unreachable!("context extension does not declare Agent scope")
                }
            }
        })
    }
}

struct ContextGlobalInstance {
    manifest: ExtensionManifest,
    server_slot: crate::instance::ServerSlot,
}

impl ExtensionInstance for ContextGlobalInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn commands(&self) -> Vec<(String, Arc<dyn ExtensionCommand>)> {
        vec![(
            NAME.into(),
            Arc::new(ContextCommand {
                server_slot: self.server_slot.clone(),
            }),
        )]
    }
}

struct ContextSessionInstance {
    manifest: ExtensionManifest,
    tail: Arc<dyn ContextTail>,
}

impl ExtensionInstance for ContextSessionInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn context_tail(&self) -> Option<Arc<dyn ContextTail>> {
        Some(self.tail.clone())
    }
}

// ── Snapshot ─────────────────────────────────────────────────────────

/// The input size of the last completed request, as recorded on the
/// agent's most recent reply.
#[derive(Debug, Clone, PartialEq)]
struct Measurement {
    input_tokens: u32,
    measured_at: DateTime<Utc>,
    model: String,
    /// Transcript entries (messages, directives, summaries) written after
    /// the measured reply — content the next request adds or replaces.
    newer_entries: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    agent: String,
    taken_at: DateTime<Utc>,
    last: Option<Measurement>,
    budget: ContextBudget,
}

/// The agent's latest reply carrying a context measurement. Same numerator
/// as a measured TUI context count: `context_tokens` is the input size of the
/// turn's final request, not the summed `usage.prompt_tokens`.
fn last_measurement(entries: &[SessionEntry], agent: &str) -> Option<Measurement> {
    let (idx, entry, tokens) = entries.iter().enumerate().rev().find_map(|(i, e)| {
        (e.sender == agent)
            .then_some(e.metadata.as_ref())
            .flatten()
            .filter(|m| crate::session::usage::reported_fields(Some(m)).0)
            .and_then(|m| m.context_tokens)
            .map(|t| (i, e, t))
    })?;
    let newer_entries = entries[idx + 1..]
        .iter()
        .filter(|e| {
            matches!(
                e.entry_type,
                EntryType::Message | EntryType::Directive | EntryType::Summary
            )
        })
        .count();
    Some(Measurement {
        input_tokens: tokens,
        measured_at: entry.timestamp,
        model: entry
            .metadata
            .as_ref()
            .map(|m| m.model.clone())
            .unwrap_or_default(),
        newer_entries,
    })
}

fn tokens(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn age(from: DateTime<Utc>, to: DateTime<Utc>) -> String {
    let secs = (to - from).num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

fn percent(numerator: u32, denominator: usize) -> String {
    format!(
        "{} / {} = {:.1}%",
        tokens(numerator.into()),
        tokens(denominator as u64),
        f64::from(numerator) / denominator as f64 * 100.0
    )
}

fn timestamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Render the compact report shown to the user and handed to the model.
fn render(s: &Snapshot) -> String {
    let b = &s.budget;
    let model = b.model.as_deref().unwrap_or("unknown");
    let mut lines = vec![format!(
        "Context for agent '{}' (snapshot {})",
        s.agent,
        timestamp(s.taken_at)
    )];

    match &s.last {
        Some(m) => {
            let mut line = format!(
                "Last completed request: {} input tokens, measured {} ({})",
                tokens(m.input_tokens.into()),
                timestamp(m.measured_at),
                age(m.measured_at, s.taken_at)
            );
            if !m.model.is_empty() {
                line.push_str(&format!(" on {}", m.model));
            }
            lines.push(line);
            lines.push(format!(
                "Next request: not yet measured; {} transcript entr{} added since the measurement",
                m.newer_entries,
                if m.newer_entries == 1 { "y" } else { "ies" }
            ));
        }
        None => {
            lines.push(
                "Last completed request: unknown (no measured reply from this agent in this session)"
                    .into(),
            );
            lines.push("Next request: not yet measured".into());
        }
    }

    let provenance = match b.source {
        ContextBudgetSource::ModelWindow => "model catalog window".to_string(),
        ContextBudgetSource::AgentCap => "explicit agent cap (max_context_tokens)".to_string(),
        ContextBudgetSource::ConfiguredDefault => {
            "fallback: configured max_context_tokens default".to_string()
        }
    };
    lines.push(format!(
        "Context budget: {} tokens for {model} — {provenance}; {} reserved for the reply",
        tokens(b.tokens as u64),
        tokens(b.reserved_output_tokens as u64)
    ));
    lines.push(match b.model_window {
        Some(w) => format!("Model window: {} tokens", tokens(w as u64)),
        None => "Model window: unknown".into(),
    });

    match &s.last {
        Some(m) if b.tokens > 0 => {
            let mut line = format!("Used: {} of budget", percent(m.input_tokens, b.tokens));
            if let Some(w) = b.model_window
                && w != b.tokens
                && w > 0
            {
                line.push_str(&format!("; {} of model window", percent(m.input_tokens, w)));
            }
            lines.push(line);
            if b.model.as_deref().is_some_and(|cur| cur != m.model) && !m.model.is_empty() {
                lines.push(format!(
                    "Note: the measurement was taken on {}, the budget is for {model}",
                    m.model
                ));
            }
        }
        _ => lines.push("Used: unknown".into()),
    }
    lines.join("\n")
}

// ── Persistence ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReportRecord {
    id: String,
    created_at: DateTime<Utc>,
    report: String,
}

async fn store_report(db: &Database, agent: &str, record: &ReportRecord) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(REPORTS_STORE).await?;
    store
        .set_string(agent, serde_json::to_string(record)?)
        .await?;
    txn.commit().await?;
    Ok(())
}

/// Read a pending report for `agent` and mark it delivered. `None` when
/// there is no report or it was already delivered.
async fn take_pending_report(db: &Database, agent: &str) -> anyhow::Result<Option<ReportRecord>> {
    let txn = db.new_transaction().await?;
    let reports = txn.get_store::<DocStore>(REPORTS_STORE).await?;
    let raw = match reports.get_string(agent).await {
        Ok(raw) => raw,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let record: ReportRecord = serde_json::from_str(&raw)?;
    let deliveries = txn.get_store::<DocStore>(DELIVERIES_STORE).await?;
    match deliveries.get_string(agent).await {
        Ok(id) if id == record.id => return Ok(None),
        Ok(_) => {}
        Err(error) if error.is_not_found() => {}
        Err(error) => return Err(error.into()),
    }
    deliveries.set_string(agent, record.id.clone()).await?;
    txn.commit().await?;
    Ok(Some(record))
}

// ── Command ──────────────────────────────────────────────────────────

struct ContextCommand {
    server_slot: crate::instance::ServerSlot,
}

impl ContextCommand {
    async fn run(&self, args: &str, ctx: &HookContext) -> ExtensionCommandOutcome {
        if !args.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /context (takes no arguments)".into());
        }
        let Some(server) = self.server_slot.get() else {
            return ExtensionCommandOutcome::Error("The chaz runtime is not ready yet.".into());
        };
        let agent_name = ctx.agent_name.as_str();
        let agent = server.agents().get(agent_name);
        let session = ctx.session.lock().await;
        let meta = session.read_meta().await;
        let budget = server.context_budget(
            meta.resolve_model_for_agent(agent_name),
            agent.as_ref().and_then(|a| a.default_model.as_deref()),
            agent.as_ref().and_then(|a| a.max_context_tokens),
        );
        let taken_at = Utc::now();
        let report = render(&Snapshot {
            agent: agent_name.to_string(),
            taken_at,
            last: last_measurement(session.entries(), agent_name),
            budget,
        });
        let record = ReportRecord {
            id: uuid::Uuid::new_v4().to_string(),
            created_at: taken_at,
            report: report.clone(),
        };
        if let Err(e) = store_report(session.database(), agent_name, &record).await {
            return ExtensionCommandOutcome::Text(format!(
                "{report}\n\n(Not shared with {agent_name}: failed to record the snapshot: {e})"
            ));
        }
        ExtensionCommandOutcome::Text(format!(
            "{report}\n\n({agent_name} will see this snapshot on its next turn in this session.)"
        ))
    }
}

impl ExtensionCommand for ContextCommand {
    fn description(&self) -> &'static str {
        "Show context-window usage and share the snapshot with the agent's next turn"
    }

    fn invoke<'a>(
        &'a self,
        args: &'a str,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
        Box::pin(self.run(args.trim(), ctx))
    }
}

// ── Context tail ─────────────────────────────────────────────────────

struct PendingReportTail {
    registry: Arc<crate::session::SessionRegistry>,
    session_db_id: String,
}

impl PendingReportTail {
    async fn take(&self, agent: &str) -> anyhow::Result<Option<ReportRecord>> {
        // Per-session instances outlive turns. Reuse the registry's current
        // authorized handle, not a cached pinned delegation identity that may
        // no longer cover the session's observed Agent-DB floor.
        let (_, db) = self.registry.open_session(&self.session_db_id).await?;
        take_pending_report(&db, agent).await
    }
}

impl ContextTail for PendingReportTail {
    fn context_tail<'a>(&'a self, call: &'a ContextTailCall<'a>) -> CapFuture<'a, Option<String>> {
        Box::pin(async move {
            let agent_name = call.agent_name;
            match self.take(agent_name).await {
                Ok(Some(record)) => Ok(Some(format!("{CONTEXT_HEADER}\n{}", record.report))),
                Ok(None) => Ok(None),
                Err(e) => {
                    tracing::warn!(agent = agent_name, "context report delivery failed: {e}");
                    Ok(None)
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "context_report_tests.rs"]
mod tests;
