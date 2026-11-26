//! Bounded reconciliation before point mutations. Certificates are disposable
//! process-local observations of an exact selected-ledger snapshot, not stored
//! authority or a second ledger. Unexpected scheduling or metadata writes
//! discard them; unrelated Executor activity is checked through the native DAG.
//! No partial scan or unchecked head can certify capacity.
use super::*;
use crate::session::scheduled::ScheduleDeliveryEvidence;
use eidetica::{Snapshot, Transaction, entry::ID, store::TableCursor};
use std::collections::{HashMap, HashSet};

#[derive(Debug, thiserror::Error)]
#[error("schedule accounting reconciliation required before mutation")]
pub(crate) struct ReconciliationRequired;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountingProgress {
    /// Rows scanned in this phase (or the final state probe), at most the limit.
    /// Each occurrence also needs a constant number of point-link reads.
    pub scanned: usize,
    pub complete: bool,
}

#[derive(Debug, Default)]
pub(crate) struct AccountingCache {
    scope: Option<(ID, Snapshot)>,
    ready: HashMap<String, Certified>,
    pending: HashMap<String, Audit>,
    refresh: Option<Refresh>,
    unpublished_only: bool,
}

/// Prove that every path from a newer frontier reaches the audited frontier
/// without changing any accounting input. Use the Database's identity-bound
/// entry reader, not the raw backend (which may have a different service key).
#[derive(Debug)]
struct Refresh {
    target: Snapshot,
    remaining: Vec<ID>,
    visited: HashSet<ID>,
    reached: HashSet<ID>,
}

impl Refresh {
    fn new(target: Snapshot) -> Self {
        Self {
            remaining: target.tips().to_vec(),
            target,
            visited: HashSet::new(),
            reached: HashSet::new(),
        }
    }

    async fn advance(
        &mut self,
        db: &Database,
        boundary: &Snapshot,
        limit: usize,
    ) -> anyhow::Result<Option<bool>> {
        let mut checked = 0;
        loop {
            let Some(id) = self.remaining.last().cloned() else {
                // Dropping an old branch could remove accounting evidence.
                return Ok(Some(self.reached.len() == boundary.len()));
            };
            if boundary.tips().contains(&id) {
                self.reached.insert(id);
            } else if !self.visited.contains(&id) {
                if checked == limit {
                    return Ok(None);
                }
                let entry = db.get_entry(&id).await?;
                checked += 1;
                if entry.subtrees().iter().any(|name| {
                    name.starts_with('_')
                        || matches!(
                            name.as_str(),
                            OCCURRENCES
                                | RESERVATIONS
                                | FIRES
                                | STATE
                                | settlement::SETTLEMENTS
                                | order::HEAD
                                | order::HANDOFFS
                                | clock::CLOCKS
                                | clock::MISSED
                        )
                }) {
                    return Ok(Some(false));
                }
                let parents = entry.parents()?;
                if parents.is_empty() {
                    return Ok(Some(false));
                }
                // Advance only after the complete entry and its links are read.
                self.remaining.pop();
                self.visited.insert(id);
                self.remaining.extend(parents);
                continue;
            }
            self.remaining.pop();
        }
    }
}

#[derive(Debug)]
struct Certified {
    schedule_id: String,
    state: AccountingState,
}

impl AccountingCache {
    pub(super) fn unpublished_only(&self) -> bool {
        self.unpublished_only
    }

    async fn observe(
        &mut self,
        db: &Database,
        snapshot: Snapshot,
        limit: usize,
    ) -> anyhow::Result<bool> {
        let scope = (db.root_id().clone(), snapshot.clone());
        if self.scope.as_ref() == Some(&scope) {
            self.refresh = None;
            return Ok(true);
        }
        if let Some((ledger, boundary)) = &self.scope
            && ledger == db.root_id()
        {
            let refresh = self.refresh.get_or_insert_with(|| Refresh::new(snapshot));
            match refresh.advance(db, boundary, limit).await? {
                None => return Ok(false),
                Some(true) => {
                    // The pinned audit's stores are byte-identical at this
                    // proven descendant. Do not certify a newer unexamined head.
                    self.scope = Some((ledger.clone(), refresh.target.clone()));
                    self.refresh = None;
                    return Ok(self.scope.as_ref() == Some(&scope));
                }
                Some(false) => {}
            }
        }
        self.ready.clear();
        self.unpublished_only = false;
        self.pending.clear();
        self.refresh = None;
        self.scope = Some(scope);
        Ok(true)
    }

    pub(super) async fn load(
        &mut self,
        agent: &AgentDb,
        schedule: &Schedule,
    ) -> anyhow::Result<(Transaction, AccountingState)> {
        let db = selected(agent).await?;
        let snapshot = db.snapshot().await?;
        anyhow::ensure!(
            self.observe(db, snapshot.clone(), 16).await?,
            ReconciliationRequired
        );
        let certified = self
            .ready
            .get(&schedule.lifetime_key())
            .ok_or(ReconciliationRequired)?;
        anyhow::ensure!(
            certified.schedule_id == schedule.id,
            "schedule lifetime is shared by different definitions"
        );
        let state = &certified.state;
        state.validate()?;
        anyhow::ensure!(
            state.legacy_baseline >= schedule.fire_count,
            "schedule legacy baseline regressed"
        );
        Ok((db.new_transaction_at(&snapshot).await?, state.clone()))
    }

    pub(super) async fn commit(
        &mut self,
        txn: Transaction,
        lifetime: &str,
        state: AccountingState,
    ) -> anyhow::Result<()> {
        state.validate()?;
        let (ledger, _) = self
            .scope
            .take()
            .ok_or_else(|| anyhow::anyhow!("schedule accounting snapshot missing"))?;
        // Clear before awaiting: cancellation or an uncertain write must not
        // leave a certificate usable for another effect. Never certify a fresh
        // db.snapshot() here: it could include an unrelated concurrent write.
        self.pending.clear();
        self.refresh = None;
        let mut ready = std::mem::take(&mut self.ready);
        let certified = ready
            .get_mut(lifetime)
            .ok_or_else(|| anyhow::anyhow!("schedule accounting certificate missing"))?;
        certified.state = state;
        let committed = txn.commit().await?;
        self.scope = Some((ledger, Snapshot::from([committed])));
        self.ready = ready;
        Ok(())
    }
}

async fn selected(agent: &AgentDb) -> anyhow::Result<&Database> {
    agent
        .scheduling_database()
        .await?
        .ok_or_else(|| anyhow::anyhow!("scheduling Executor missing"))
}

/// Perform one read-only page of reconciliation. Callers schedule subsequent
/// pages outside timer admission; this never waits for a model/idle target or
/// initializes/migrates a database. All stores use the same pinned snapshot.
/// Local lifecycle commits retain completed certificates by exact known delta;
/// unexpected scheduling/metadata writes or restarts require a fresh audit.
/// Up to `limit` additional native entry reads prove an unrelated-write delta;
/// a larger delta retains bounded progress without certifying its unchecked head.
/// This is not distributed fencing.
pub async fn reconcile(
    agent: &AgentDb,
    schedule: &Schedule,
    limit: usize,
) -> anyhow::Result<AccountingProgress> {
    anyhow::ensure!((1..=128).contains(&limit), "invalid accounting page size");
    let mut cache = agent.lock_scheduling().await;
    let db = selected(agent).await?;
    let snapshot = db.snapshot().await?;
    if !cache.observe(db, snapshot.clone(), limit).await? {
        return Ok(AccountingProgress {
            scanned: 0,
            complete: false,
        });
    }
    let lifetime = schedule.lifetime_key();
    if let Some(certified) = cache.ready.get(&lifetime) {
        anyhow::ensure!(
            certified.schedule_id == schedule.id,
            "schedule lifetime is shared by different definitions"
        );
        let state = &certified.state;
        anyhow::ensure!(
            state.legacy_baseline >= schedule.fire_count,
            "schedule legacy baseline regressed"
        );
        return Ok(AccountingProgress {
            scanned: 0,
            complete: true,
        });
    }
    let audit = cache
        .pending
        .entry(lifetime.clone())
        .or_insert_with(|| Audit {
            schedule_id: schedule.id.clone(),
            baseline: schedule.fire_count,
            phase: Phase::Occurrences,
            unpublished_only: true,
            cursor: None,
            deliveries: HashSet::new(),
            occurrences: 0,
            successful: 0,
            held: 0,
            fires: 0,
        });
    anyhow::ensure!(
        audit.schedule_id == schedule.id && audit.baseline == schedule.fire_count,
        "schedule accounting audit scope changed"
    );
    let txn = db.new_transaction_at(&snapshot).await?;
    let (scanned, state) = audit.advance(&txn, agent, schedule, limit).await?;
    let complete = state.is_some();
    if let Some(state) = state {
        cache.unpublished_only = audit.unpublished_only;
        cache.pending.remove(&lifetime);
        cache.ready.insert(
            lifetime,
            Certified {
                schedule_id: schedule.id.clone(),
                state,
            },
        );
    }
    Ok(AccountingProgress { scanned, complete })
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Occurrences,
    Reservations,
    Fires,
    Settlements,
    State,
}

#[derive(Debug)]
struct Audit {
    schedule_id: String,
    baseline: u32,
    phase: Phase,
    unpublished_only: bool,
    cursor: Option<TableCursor>,
    deliveries: HashSet<String>,
    occurrences: u32,
    successful: u32,
    held: u32,
    fires: u32,
}

fn add(a: u32, b: usize) -> anyhow::Result<u32> {
    a.checked_add(u32::try_from(b)?)
        .ok_or_else(|| anyhow::anyhow!("schedule accounting overflow"))
}

impl Audit {
    async fn advance(
        &mut self,
        txn: &Transaction,
        agent: &AgentDb,
        schedule: &Schedule,
        limit: usize,
    ) -> anyhow::Result<(usize, Option<AccountingState>)> {
        let lifetime = schedule.lifetime_key();
        let (scanned, next, phase) = match self.phase {
            Phase::Occurrences => {
                let page = txn
                    .get_store::<Table<Occurrence>>(OCCURRENCES)
                    .await?
                    .scan_page(self.cursor.as_ref(), limit)
                    .await?;
                let mut deliveries = HashSet::new();
                let (mut count, mut successful, mut held) = (0usize, 0usize, 0usize);
                for (key, row) in &page.rows {
                    row.validate(agent)?;
                    order::validate_link(txn, agent, row).await?;
                    clock::validate_link(txn, agent, row).await?;
                    retry::validate_lineage(txn, agent, row).await?;
                    self.unpublished_only &= row.publication_sequence.is_none();
                    anyhow::ensure!(
                        key == row.storage_key()
                            && !self.deliveries.contains(&row.reference.delivery_id)
                            && deliveries.insert(row.reference.delivery_id.clone()),
                        "schedule occurrence/delivery identity conflicts"
                    );
                    if row.reference.lifetime != lifetime {
                        continue;
                    }
                    anyhow::ensure!(
                        row.reference.schedule_id == schedule.id,
                        "schedule lifetime is shared by different definitions"
                    );
                    let fire = optional(
                        &txn.get_store::<Table<ScheduleFire>>(FIRES).await?,
                        &row.reference.delivery_id,
                    )
                    .await?;
                    validate_handoff(row, fire.as_ref())?;
                    let evidence = optional(
                        &txn.get_store::<Table<ScheduleDeliveryEvidence>>(settlement::SETTLEMENTS)
                            .await?,
                        key,
                    )
                    .await?;
                    let capacity = settlement::counted_capacity(row, evidence.as_ref())?;
                    if retry::is_latest(txn, agent, row).await? {
                        match capacity {
                            settlement::Capacity::Successful => successful += 1,
                            settlement::Capacity::Held => held += 1,
                            settlement::Capacity::Released => {}
                        }
                    }
                    count += 1;
                }
                // Publish a page's observations only after every row validates.
                let counts = (
                    add(self.occurrences, count)?,
                    add(self.successful, successful)?,
                    add(self.held, held)?,
                );
                self.deliveries.extend(deliveries);
                (self.occurrences, self.successful, self.held) = counts;
                (page.rows.len(), page.next, Phase::Reservations)
            }
            Phase::Reservations => {
                let page = txn
                    .get_store::<Table<Reservation>>(RESERVATIONS)
                    .await?
                    .scan_page(self.cursor.as_ref(), limit)
                    .await?;
                anyhow::ensure!(
                    page.rows.iter().all(|(_, r)| r.lifetime != lifetime),
                    "legacy reservations require reconciliation before lifecycle writes"
                );
                (page.rows.len(), page.next, Phase::Fires)
            }
            Phase::Fires => {
                let page = txn
                    .get_store::<Table<ScheduleFire>>(FIRES)
                    .await?
                    .scan_page(self.cursor.as_ref(), limit)
                    .await?;
                let mut count = 0;
                for (_, fire) in &page.rows {
                    if fire.lifetime.as_deref() == Some(&lifetime)
                        || (fire.schedule_id == schedule.id && fire.lifetime.is_none())
                    {
                        anyhow::ensure!(
                            fire.lifetime.as_deref() == Some(&lifetime),
                            "schedule handoff lifetime is unknown"
                        );
                        count += 1;
                    }
                }
                self.fires = add(self.fires, count)?;
                (page.rows.len(), page.next, Phase::Settlements)
            }
            Phase::Settlements => {
                let page = txn
                    .get_store::<Table<ScheduleDeliveryEvidence>>(settlement::SETTLEMENTS)
                    .await?
                    .scan_page(self.cursor.as_ref(), limit)
                    .await?;
                for (key, evidence) in &page.rows {
                    if evidence.input.anchor.reference.lifetime != lifetime {
                        continue;
                    }
                    let row = txn
                        .get_store::<Table<Occurrence>>(OCCURRENCES)
                        .await?
                        .get(key)
                        .await?;
                    row.validate(agent)?;
                    anyhow::ensure!(
                        row.reference.lifetime == lifetime && key == row.storage_key(),
                        "schedule settlement occurrence missing"
                    );
                    settlement::capacity(&row, evidence)?;
                }
                (page.rows.len(), page.next, Phase::State)
            }
            Phase::State => {
                let raw = optional(
                    &txn.get_store::<Table<serde_json::Value>>(STATE).await?,
                    &lifetime,
                )
                .await?;
                let state = match raw {
                    Some(raw) if raw.get("version").is_some() => {
                        let state: AccountingState = serde_json::from_value(raw)?;
                        state.validate()?;
                        state
                    }
                    raw => {
                        anyhow::ensure!(
                            self.occurrences == 0,
                            "schedule occurrence accounting missing"
                        );
                        let legacy: ScheduleState = match raw {
                            Some(raw) => serde_json::from_value(raw)?,
                            None => {
                                anyhow::ensure!(
                                    self.fires == 0,
                                    "schedule handoff accounting missing"
                                );
                                baseline(schedule)
                            }
                        };
                        anyhow::ensure!(
                            legacy.fire_count >= add(schedule.fire_count, self.fires as usize)?,
                            "schedule legacy baseline is below its history"
                        );
                        AccountingState {
                            version: 2,
                            fire_count: legacy.fire_count,
                            legacy_baseline: legacy.fire_count,
                            successful_fires: 0,
                            reservations: 0,
                            handoff_count: self.fires,
                            consumed: legacy.consumed,
                            retired: legacy.retired,
                        }
                    }
                };
                anyhow::ensure!(
                    state.legacy_baseline >= schedule.fire_count,
                    "schedule legacy baseline regressed"
                );
                anyhow::ensure!(
                    state.successful_fires == self.successful && state.reservations == self.held,
                    "schedule occurrence accounting differs from its settlement evidence"
                );
                anyhow::ensure!(
                    state.handoff_count == self.fires,
                    "schedule handoff accounting differs"
                );
                return Ok((1, Some(state)));
            }
        };
        self.cursor = next;
        if self.cursor.is_none() {
            self.phase = phase;
        }
        Ok((scanned, None))
    }
}

pub(super) fn validate_handoff(
    row: &Occurrence,
    fire: Option<&ScheduleFire>,
) -> anyhow::Result<()> {
    match (&row.preparation, fire) {
        (Preparation::HandedOff { input, at }, Some(fire)) => anyhow::ensure!(
            fire.firing_id.as_ref() == Some(&row.reference.delivery_id)
                && fire.schedule_id == row.reference.schedule_id
                && fire.lifetime.as_ref() == Some(&row.reference.lifetime)
                && fire.session_db_id == input.session_db_id
                && fire.request_id.as_ref() == Some(&input.request_id)
                && fire.attempt_id.is_none()
                && fire.fired_at == *at
                && fire.fresh == matches!(row.target, ScheduleTarget::Fresh)
                && fire.usage.is_none(),
            "schedule handoff evidence differs"
        ),
        (Preparation::Prepared | Preparation::NotApplied, None) => {}
        _ => anyhow::bail!("schedule occurrence/handoff evidence missing or conflicting"),
    }
    Ok(())
}
