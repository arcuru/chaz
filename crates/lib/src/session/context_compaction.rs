//! Attempt-scoped context edits. Strategy code sees selected members and
//! committed replacements, never a database, a transaction or a codec.
use super::{RepresentationId, worker_links::WorkerCallSite};
use crate::{extension::caps::CapFuture, runtime::RuntimeMessage};

#[derive(Clone, Debug)]
pub struct ContextReplacement {
    pub(crate) targets: Vec<RepresentationId>,
    pub(crate) summary: RepresentationId,
    pub(crate) text: String,
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

    fn historical_members(&self) -> CapFuture<'_, Vec<RepresentationId>>;
    fn replacements(&self) -> CapFuture<'_, Vec<ContextReplacement>>;

    /// Pin selected immutable members before dispatching an auxiliary model
    /// call. Already attempted coverage is not authorization to dispatch again.
    fn reserve(&self, members: Vec<(RuntimeMessage, RepresentationId)>) -> CapFuture<'_, String>;

    /// `None` records an unapplied result, including a locally abandoned call.
    /// A failed/uncertain write must not be retried as another model call.
    fn settle(&self, intent: String, summary: Option<String>) -> CapFuture<'_, ()>;
}
