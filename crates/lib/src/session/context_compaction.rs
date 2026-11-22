//! Attempt-scoped context edits. Strategy code sees selected members and
//! committed replacements, never a database, a transaction or a codec.
use super::{RepresentationId, worker_links::WorkerCallSite};
use crate::{extension::caps::CapFuture, runtime::RuntimeMessage};

pub(crate) fn fold_citation(key: &str, last_index: usize) -> String {
    format!(
        "\n\nRetained message refs (tool_history_query, if available): cr1:{key}:<index>, indices 0 through {last_index}."
    )
}

#[derive(Clone, Debug)]
pub struct ContextReplacement {
    pub(crate) targets: Vec<RepresentationId>,
    pub(crate) summary: RepresentationId,
    pub(crate) text: String,
}

/// Validated foreground activity for the same agent and model in the source
/// lineage. No timing or cost policy is interpreted by the store.
#[derive(Debug, Default)]
pub struct CompletedTurnActivity {
    pub(crate) latest_completion: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) call_counts: Vec<usize>,
}

/// Local tool authority for one invocation; selecting a range grants no model
/// dispatch or ability to rerun the original tool.
pub trait CompactToolScope: Send + Sync {
    fn targets<'a>(&'a self, call: &'a WorkerCallSite) -> CapFuture<'a, Vec<RepresentationId>>;
    fn replace<'a>(
        &'a self,
        call: &'a WorkerCallSite,
        targets: Vec<RepresentationId>,
        summary: String,
    ) -> CapFuture<'a, ()>;
}

pub trait ContextCompaction: Send + Sync {
    fn tool_scope(&self) -> &dyn CompactToolScope;

    fn scoped_history<'a>(
        &'a self,
        model_id: Option<&'a str>,
    ) -> CapFuture<'a, (Vec<RepresentationId>, CompletedTurnActivity)>;
    fn replacements(&self) -> CapFuture<'_, Vec<ContextReplacement>>;

    /// Pin selected immutable members before dispatching an auxiliary model
    /// call. Already attempted coverage is not authorization to dispatch again.
    fn reserve(&self, members: Vec<(RuntimeMessage, RepresentationId)>) -> CapFuture<'_, String>;

    /// `None` records an unapplied result, including a locally abandoned call.
    /// A failed/uncertain write must not be retried as another model call.
    fn settle(&self, intent: String, summary: Option<String>) -> CapFuture<'_, ()>;
}
