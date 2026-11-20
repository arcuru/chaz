//! Required, replaceable context selection over a host-owned coherent view.
//!
//! Endpoints return references, not provenance or native messages. Rendering,
//! exchange reconstruction, source identities and final budgets belong to the host.
#[cfg(test)]
mod tests;

use super::{ExtensionHub, caps::CapFuture, projection::PROJECTOR_TIMEOUT, settings};
use crate::{
    cache::CacheOptions,
    context::ContextBuilder,
    session::{EntryType, HistoryGroup},
};
use anyhow::Context;
use eidetica::crdt::{Doc, doc::Value as NativeValue};
use eidetica::store::DocStore;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, panic::AssertUnwindSafe, sync::Arc};

pub(crate) const CONTEXT_SELECTIONS_STORE: &str = "context_selections";

fn selection_settings(record: &NativeValue, selected: &str) -> anyhow::Result<Value> {
    let record = record
        .as_doc()
        .context("expected a context selection record")?;
    anyhow::ensure!(
        record.is_atomic() && record.len() == 3 && record.get_as::<i64>("version") == Some(1),
        "invalid or unsupported context selection record"
    );
    anyhow::ensure!(
        record.get("extension").and_then(NativeValue::as_text) == Some(selected),
        "context selection record does not match its extension key"
    );
    settings::decode(
        record
            .get("settings")
            .context("missing context selection settings")?,
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextStrategyGrant {
    pub extension: String,
}
impl Default for ContextStrategyGrant {
    fn default() -> Self {
        Self {
            extension: "baseline_context".into(),
        }
    }
}

/// A reference into this invocation's scoped inputs. The host rejects foreign,
/// duplicate or reordered history references; replay is always a complete group.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ContextItem {
    Instructions,
    /// A rendered UTC date boundary derived from the following entry.
    Date(usize),
    Entry(usize),
    Exchange {
        entry: usize,
        budget: usize,
    },
    Tail,
    Durable {
        row: usize,
        message: usize,
    },
}
#[derive(Clone, Debug, Default)]
pub struct ContextPlan {
    pub items: Vec<ContextItem>,
    pub cache: CacheOptions,
    pub truncated: bool,
}

/// Read-only, invocation-local selection/hydration facility. It does not load an
/// extra history or expose a database handle. Only host-selected rows are visible.
pub struct ScopedContext<'a> {
    pub(crate) builder: &'a ContextBuilder<'a>,
    pub(crate) system: &'a str,
    pub(crate) tail: Option<&'a str>,
    pub(crate) sent: HashSet<&'a str>,
}
/// One eligible row and its date. Its index names the whole group for
/// entry, date and exchange references in this invocation.
pub struct ScopedGroup<'a> {
    pub index: usize,
    pub kind: &'a EntryType,
    pub date: Option<chrono::NaiveDate>,
    /// A legacy positional cutoff, never an explicit-target summary.
    pub positional_summary: bool,
}

impl ScopedContext<'_> {
    pub fn groups(&self) -> impl DoubleEndedIterator<Item = ScopedGroup<'_>> + '_ {
        self.builder
            .groups
            .iter()
            .enumerate()
            .filter_map(|(index, group)| {
                Some(ScopedGroup {
                    index,
                    kind: group.kind()?,
                    date: group.date(),
                    positional_summary: group.positional_summary(),
                })
            })
    }
    /// Compatibility projection for strategies using entry indices.
    pub fn entries(&self) -> impl DoubleEndedIterator<Item = (usize, EntryType)> + '_ {
        self.groups().map(|group| (group.index, group.kind.clone()))
    }
    pub(crate) fn group(&self, index: usize) -> anyhow::Result<&HistoryGroup> {
        self.builder
            .groups
            .get(index)
            .filter(|group| group.kind().is_some())
            .ok_or_else(|| anyhow::anyhow!("foreign context entry"))
    }
    /// Date metadata of an eligible retained row, never the executor's clock.
    pub fn entry_date(&self, index: usize) -> anyhow::Result<chrono::NaiveDate> {
        self.group(index)?
            .date()
            .ok_or_else(|| anyhow::anyhow!("context group has no recorded date"))
    }
    pub fn has_instructions(&self) -> bool {
        !self.system.is_empty()
    }
    pub fn has_tail(&self) -> bool {
        self.tail.is_some()
    }
    pub fn durable_items(&self) -> impl Iterator<Item = ContextItem> + '_ {
        self.builder
            .contributions
            .iter()
            .enumerate()
            .flat_map(|(row, batch)| {
                (0..batch.contribution.messages.len())
                    .map(move |message| ContextItem::Durable { row, message })
            })
    }
    /// Hydrate one scoped reference, with host-assigned provenance. These are
    /// read-only derived messages, not a database handle or a write/execute grant.
    pub fn hydrate(
        &self,
        item: &ContextItem,
    ) -> anyhow::Result<
        Vec<(
            crate::runtime::RuntimeMessage,
            super::projection::ContextSource,
        )>,
    > {
        self.builder.hydrate(self, item)
    }
    /// Render only the referenced item. Native exchange reconstruction is bounded
    /// by its caller-supplied token budget and never performs a new store read.
    pub fn cost(&self, item: &ContextItem) -> anyhow::Result<usize> {
        Ok(self
            .builder
            .hydrate(self, item)?
            .iter()
            .map(|(m, _)| crate::context::estimate_request_tokens(std::slice::from_ref(m), &[]))
            .sum())
    }
    pub fn tools_cost(&self) -> usize {
        crate::context::estimate_request_tokens(&[], self.builder.tool_defs)
    }
}

pub struct ContextStrategyCall<'a> {
    pub agent_name: &'a str,
    pub session_db_id: Option<&'a str>,
    pub request_id: Option<&'a str>,
    pub context: &'a ScopedContext<'a>,
    pub budget_tokens: usize,
    /// The selected extension's own persisted session settings, read this call.
    pub settings: &'a Value,
}
pub trait ContextStrategy: Send + Sync {
    fn select<'a>(&'a self, call: &'a ContextStrategyCall<'a>) -> CapFuture<'a, ContextPlan>;
}

impl ExtensionHub {
    pub fn set_context_strategy_grant(
        &mut self,
        grant: &ContextStrategyGrant,
    ) -> Result<(), String> {
        if grant.extension.trim().is_empty() {
            return Err("context_strategy requires an extension name".into());
        }
        self.context_strategy = Some(grant.clone());
        Ok(())
    }
    pub(crate) async fn resolve_context_strategy(
        &self,
        agent: &str,
        db: Option<&eidetica::Database>,
        turn_active: &HashSet<String>,
    ) -> anyhow::Result<(String, Arc<dyn ContextStrategy>)> {
        let grant = self
            .context_strategy
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no required context strategy configured"))?;
        let active = self
            .active_extensions_for_call(agent, db, turn_active)
            .await;
        let instances = self.scoped_instances(agent, db).await;
        let endpoint = instances
            .get(&grant.extension)
            .filter(|_| active.contains(&grant.extension))
            .filter(|i| {
                i.manifest()
                    .provides_capabilities
                    .contains(&super::caps::CapabilityKind::ContextStrategy)
            })
            .and_then(|i| i.context_strategy());
        endpoint
            .map(|e| (grant.extension.clone(), e))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "required context strategy '{}' is unavailable or inactive",
                    grant.extension
                )
            })
    }
    /// Persist the explicit granted selection/settings in a session/agent key.
    /// This is reconstruction state, not an execution or projection grant.
    pub(crate) async fn record_context_selection(
        &self,
        db: &eidetica::Database,
        agent: &str,
        selected: &str,
        settings: &Value,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.context_strategy
                .as_ref()
                .is_some_and(|g| g.extension == selected),
            "context selection not granted"
        );
        let key = serde_json::to_string(&(agent, selected))?;
        let txn = db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(CONTEXT_SELECTIONS_STORE).await?;
        let unchanged = match settings::stored(&store, &key).await? {
            // Keep the predecessor's exact no-op check on retained Text.
            Some(NativeValue::Text(old)) => {
                let expected = serde_json::to_string(
                    &serde_json::json!({"extension": selected, "settings": settings}),
                )?;
                old == expected
            }
            // Compare serialized settings, as before (e.g. -0.0 vs 0.0), not
            // the native List's generated position IDs.
            Some(old) => {
                serde_json::to_vec(&selection_settings(&old, selected)?)?
                    == serde_json::to_vec(settings)?
            }
            None => false,
        };
        if unchanged {
            return Ok(());
        }
        let mut record = Doc::atomic();
        record.set("version", 1_i64);
        record.set("extension", selected);
        record.set("settings", settings::encode(settings));
        store.set_node(key, record).await?;
        txn.commit().await?;
        Ok(())
    }
    pub(crate) async fn check_context_strategy(
        &self,
        selected: Option<&str>,
        agent: &str,
        db: Option<&eidetica::Database>,
        active: &HashSet<String>,
    ) -> anyhow::Result<()> {
        if let Some(selected) = selected {
            let (current, _) = self.resolve_context_strategy(agent, db, active).await?;
            anyhow::ensure!(
                current == selected,
                "required context strategy changed during the attempt"
            );
        }
        Ok(())
    }
}

pub(crate) async fn invoke(
    endpoint: &dyn ContextStrategy,
    call: &ContextStrategyCall<'_>,
) -> anyhow::Result<ContextPlan> {
    match tokio::time::timeout(
        PROJECTOR_TIMEOUT,
        AssertUnwindSafe(async { endpoint.select(call).await }).catch_unwind(),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => anyhow::bail!("required context strategy panicked"),
        Err(_) => anyhow::bail!("required context strategy timed out"),
    }
}
