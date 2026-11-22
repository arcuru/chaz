//! Host-owned compaction transactions. The selected source lives on the
//! attempt's lineage; current-state validation does not change those parents.
use super::*;
use crate::{
    extension::caps::CapFuture,
    session::{
        CompactToolScope, ContextCompaction, ContextReplacement, worker_links::WorkerCallSite,
    },
};
use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct State {
    pub(super) prepared: Option<CallInput>,
    pending: Option<CompactionIntent>,
    replacements: Vec<ContextReplacement>,
}

impl TurnRecorder {
    async fn compaction_state(&self) -> anyhow::Result<tokio::sync::MutexGuard<'_, FencedTurn>> {
        let state = self
            .fenced
            .as_ref()
            .context("no scoped context writer")?
            .lock()
            .await;
        ensure!(
            self.fenced_compaction && self.fenced_admitted.load(Ordering::Acquire),
            "context edit outside a running attempt"
        );
        Ok(state)
    }

    /// A current attempt and writer lease are separate from the history
    /// selection. They are not a lease to incorporate concurrent history.
    async fn check_compaction_attempt(&self, db: &Database) -> anyhow::Result<()> {
        let txn = db.new_transaction().await?;
        let attempt = txn
            .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
            .await?
            .get(&self.attempt.attempt_id)
            .await?;
        ensure!(
            attempt == self.attempt,
            "compaction attempt is no longer current"
        );
        Ok(())
    }

    fn compact_tool<'a>(
        &self,
        state: &'a FencedTurn,
        site: &WorkerCallSite,
    ) -> anyhow::Result<(&'a CallInput, ToolCallId)> {
        let prepared = state
            .compaction
            .as_ref()
            .and_then(|scope| scope.prepared.as_ref())
            .context("no prepared invocation for compact")?;
        ensure!(
            site.attempt_id == prepared.attempt
                && site.request_id == self.attempt.request_id
                && site.execution_id.as_deref() == Some(prepared.call.branch.as_str())
                && site.agent == self.identity.agent
                && site.model_sequence == prepared.call.model_sequence,
            "compact belongs to a different invocation"
        );
        let response = state
            .responses
            .get(&site.model_sequence)
            .context("compact response not recorded")?;
        Ok((
            prepared,
            ToolCallId {
                response: response.clone(),
                index: site.call_index.try_into()?,
            },
        ))
    }
}

impl ContextCompaction for TurnRecorder {
    fn tool_scope(&self) -> &dyn CompactToolScope {
        self
    }

    fn historical_members(&self) -> CapFuture<'_, Vec<RepresentationId>> {
        Box::pin(async move {
            let session = self.write_session().await.map_err(anyhow::Error::msg)?;
            let state = self.compaction_state().await?;
            let db = session.database();
            let txn = db.new_transaction_at(&state.source).await?;
            let branch = self
                .identity
                .execution_id
                .as_deref()
                .context("missing branch")?;
            let source = Staging::at(db, &state.source, &txn, branch).await?;
            Ok(source
                .historical_members(&state.input)?
                .into_iter()
                .map(|(_, id)| id)
                .collect())
        })
    }

    fn replacements(&self) -> CapFuture<'_, Vec<ContextReplacement>> {
        Box::pin(async move {
            let state = self.compaction_state().await?;
            Ok(state
                .compaction
                .as_ref()
                .context("missing scope")?
                .replacements
                .clone())
        })
    }

    fn reserve(&self, members: Vec<(RuntimeMessage, RepresentationId)>) -> CapFuture<'_, String> {
        Box::pin(async move {
            let session = self.write_session().await.map_err(anyhow::Error::msg)?;
            let db = session.database();
            let mut state = self.compaction_state().await?;
            Box::pin(self.check_compaction_attempt(db)).await?;
            ensure!(
                state.terminal.is_none(),
                "attempt already returned a terminal response"
            );
            ensure!(
                state
                    .compaction
                    .as_ref()
                    .is_some_and(|scope| scope.pending.is_none()),
                "another compaction intent is pending"
            );
            let txn = state.transaction(db).await?;
            let branch = self
                .identity
                .execution_id
                .as_deref()
                .context("missing branch")?;
            let source = Staging::at(db, &state.frontier, &txn, branch).await?;
            let historical = source.historical_members(&state.input)?;
            let intent = CompactionIntent {
                id: uuid::Uuid::new_v4().to_string(),
                input: state.input.clone(),
                observed: source.observed.clone(),
                targets: members.iter().map(|(_, id)| id.clone()).collect(),
                tool: None,
            };
            source.validate_compaction(&intent)?;
            ensure!(
                members.iter().all(|member| historical.contains(member)),
                "compaction source bytes changed"
            );
            ensure!(
                !source
                    .compactions()
                    .iter()
                    .any(|old| old.tool.is_none() && old.targets == intent.targets),
                "automatic compaction already attempted for these members"
            );
            ensure!(
                Box::pin(current_targets(db, &intent.targets)).await?,
                "compaction targets are stale"
            );
            stage_observation(
                &txn,
                Observation::CompactionIntent {
                    intent: intent.clone(),
                },
            )
            .await?;
            state.frontier = Snapshot::from([txn.commit().await?]);
            let id = intent.id.clone();
            state.compaction.as_mut().context("missing scope")?.pending = Some(intent);
            Ok(id)
        })
    }

    fn settle(&self, id: String, summary: Option<String>) -> CapFuture<'_, ()> {
        Box::pin(async move {
            let session = self.write_session().await.map_err(anyhow::Error::msg)?;
            let db = session.database();
            let mut state = self.compaction_state().await?;
            Box::pin(self.check_compaction_attempt(db)).await?;
            // Take before touching storage. A failed or ambiguous write is not
            // a lease to settle again or to dispatch another summary.
            let pending = &mut state.compaction.as_mut().context("missing scope")?.pending;
            ensure!(
                pending.as_ref().is_some_and(|intent| intent.id == id),
                "unmatched compaction outcome"
            );
            let intent = pending.take().expect("pending identity checked");
            let txn = state.transaction(db).await?;
            let (replacement, status) = if let Some(summary) = summary {
                ensure!(
                    state.terminal.is_none(),
                    "cannot apply compaction after a terminal response"
                );
                if Box::pin(current_targets(db, &intent.targets)).await? {
                    let branch = self
                        .identity
                        .execution_id
                        .as_deref()
                        .context("missing branch")?;
                    let at = &intent.observed.frontier;
                    let source_txn = db.new_transaction_at(at).await?;
                    let source = Staging::at(db, at, &source_txn, branch).await?;
                    let replacement = source.replace_context(&txn, &intent, summary).await?;
                    (Some(replacement), CompactionSettlement::Applied)
                } else {
                    (None, CompactionSettlement::Stale)
                }
            } else {
                (None, CompactionSettlement::Unapplied)
            };
            stage_observation(&txn, Observation::CompactionSettled { id, status }).await?;
            state.frontier = Snapshot::from([txn.commit().await?]);
            if let Some(replacement) = replacement {
                state
                    .compaction
                    .as_mut()
                    .context("missing scope")?
                    .replacements
                    .push(replacement);
            }
            ensure!(
                status != CompactionSettlement::Stale,
                "compaction targets are stale; summary not applied"
            );
            Ok(())
        })
    }
}

impl CompactToolScope for TurnRecorder {
    fn targets<'a>(&'a self, site: &'a WorkerCallSite) -> CapFuture<'a, Vec<RepresentationId>> {
        Box::pin(async move {
            let session = self.write_session().await.map_err(anyhow::Error::msg)?;
            let db = session.database();
            let state = self.compaction_state().await?;
            let (prepared, _) = self.compact_tool(&state, site)?;
            let at = &prepared.observed.frontier;
            let txn = db.new_transaction_at(at).await?;
            let source = Staging::at(db, at, &txn, &prepared.call.branch).await?;
            let history: BTreeSet<_> = source
                .historical_members(&state.input)?
                .into_iter()
                .map(|(_, id)| id)
                .collect();
            Ok(prepared
                .representations
                .iter()
                .filter(|id| history.contains(*id))
                .cloned()
                .collect())
        })
    }

    fn replace<'a>(
        &'a self,
        site: &'a WorkerCallSite,
        targets: Vec<RepresentationId>,
        summary: String,
    ) -> CapFuture<'a, ()> {
        Box::pin(async move {
            let session = self.write_session().await.map_err(anyhow::Error::msg)?;
            let db = session.database();
            let mut state = self.compaction_state().await?;
            Box::pin(self.check_compaction_attempt(db)).await?;
            let (prepared, tool) = self.compact_tool(&state, site)?;
            let branch = &prepared.call.branch;
            let txn = state.transaction(db).await?;
            let current = Staging::at(db, &state.frontier, &txn, branch).await?;
            ensure!(
                current.compact_tool_summary(&tool).as_deref() == Some(summary.as_str()),
                "compact summary differs from the recorded tool arguments"
            );
            ensure!(
                !current
                    .compactions()
                    .iter()
                    .any(|intent| intent.tool.as_ref() == Some(&tool)),
                "compact invocation already settled"
            );
            ensure!(
                targets
                    .iter()
                    .all(|id| prepared.representations.contains(id)),
                "compact targets outside prepared request"
            );
            let intent = CompactionIntent {
                id: uuid::Uuid::new_v4().to_string(),
                input: state.input.clone(),
                observed: prepared.observed.clone(),
                targets,
                tool: Some(tool),
            };
            let at = &intent.observed.frontier;
            let source_txn = db.new_transaction_at(at).await?;
            let source = Staging::at(db, at, &source_txn, branch).await?;
            source.validate_compaction(&intent)?;
            let (status, replacement) = if Box::pin(current_targets(db, &intent.targets)).await? {
                (
                    CompactionSettlement::Applied,
                    Some(source.replace_context(&txn, &intent, summary).await?),
                )
            } else {
                (CompactionSettlement::Stale, None)
            };
            stage_observation(
                &txn,
                Observation::CompactionSettled {
                    id: intent.id.clone(),
                    status,
                },
            )
            .await?;
            stage_observation(&txn, Observation::CompactionIntent { intent }).await?;
            state.frontier = Snapshot::from([txn.commit().await?]);
            state
                .compaction
                .as_mut()
                .context("missing scope")?
                .replacements
                .extend(replacement);
            ensure!(
                status == CompactionSettlement::Applied,
                "compact targets are stale"
            );
            Ok(())
        })
    }
}
