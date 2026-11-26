//! Durable preparation and handoff in the existing selected scheduling ledger.
//!
//! These APIs do not run work, start attempts or retry anything. Settlement
//! projects Session-owned delivery and terminal evidence, not handoff success.
//! Lifecycle writes share local serialization with definition edits. The
//! admission caller validates clock lineage, home, keys and membership. Reopening this
//! ledger never selects another DB or clears uncertain reservations. Activation
//! also requires supported readers and the common targeted runner; old writers
//! deliberately refuse the versioned accounting rows written here.
use super::readers::{AccountingState, DueSlot, ScheduleReference};
use super::*;
use crate::agent_db::{AgentDb, ScheduleTarget};
use crate::session::{Session, scheduled::ScheduleInputAnchor};

pub mod clock;
mod order;
pub use order::HandoffScan;
mod accounting;
pub(crate) use accounting::AccountingCache;
pub(crate) use accounting::ReconciliationRequired;
pub use accounting::{AccountingProgress, reconcile};
mod settlement;
pub use settlement::settle;
pub(crate) use settlement::{retire_unused, superseded};
pub(crate) mod fresh;
pub(crate) mod retry;

const OCCURRENCES: &str = "schedule_occurrences";

/// A bounded, prompt-free discovery scan of one immutable operational snapshot.
/// The cursor is local and carries no delivery/retry authority. Restarting a
/// scan can repeat observations, never allocate a new occurrence or Session.
pub struct OccurrenceScan {
    agent_id: eidetica::entry::ID,
    ledger_id: eidetica::entry::ID,
    snapshot: eidetica::Snapshot,
    cursor: Option<eidetica::store::TableCursor>,
    finished: bool,
}

impl OccurrenceScan {
    pub async fn open(agent: &AgentDb) -> anyhow::Result<Option<Self>> {
        let Some(db) = agent.scheduling_database().await? else {
            return Ok(None);
        };
        Ok(Some(Self {
            agent_id: agent.id(),
            ledger_id: db.root_id().clone(),
            snapshot: db.snapshot().await?,
            cursor: None,
            finished: false,
        }))
    }

    /// Reads at most 128 records per call in native persisted-key order. A
    /// malformed/unavailable page does not advance the cursor. Consumers must
    /// still revalidate current source/authority and exact handoff evidence;
    /// a scan is discovery, not a validated accounting projection or runnable
    /// queue. Concurrent writes appear in a subsequent scan, not halfway here.
    pub async fn next_page(
        &mut self,
        agent: &AgentDb,
        limit: usize,
    ) -> anyhow::Result<Option<Vec<Occurrence>>> {
        anyhow::ensure!(
            (1..=128).contains(&limit),
            "invalid schedule scan page size"
        );
        anyhow::ensure!(agent.id() == self.agent_id, "wrong schedule scan Agent");
        let db = agent
            .scheduling_database()
            .await?
            .ok_or_else(|| anyhow::anyhow!("scheduling Executor missing during scan"))?;
        anyhow::ensure!(
            db.root_id() == &self.ledger_id,
            "schedule scan ledger differs"
        );
        if self.finished {
            return Ok(None);
        }
        let txn = db.new_transaction_at(&self.snapshot).await?;
        let page = txn
            .get_store::<Table<Occurrence>>(OCCURRENCES)
            .await?
            .scan_page(self.cursor.as_ref(), limit)
            .await?;
        let mut rows = Vec::with_capacity(page.rows.len());
        for (key, row) in page.rows {
            row.validate(agent)?;
            order::validate_link(&txn, agent, &row).await?;
            clock::validate_link(&txn, agent, &row).await?;
            anyhow::ensure!(key == row.storage_key(), "schedule occurrence key differs");
            retry::validate_lineage(&txn, agent, &row).await?;
            rows.push(row);
        }
        self.finished = page.next.is_none();
        self.cursor = page.next;
        Ok(Some(rows))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Preparation {
    Prepared,
    HandedOff {
        input: Box<ScheduleInputAnchor>,
        at: DateTime<Utc>,
    },
    /// Only a positively unused, pre-handoff reservation may be released here.
    NotApplied,
}

/// Durable provisioning intent, not permission to run or create a replacement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum FreshRoot {
    Requested,
    Bound { session_db_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Occurrence {
    pub version: u32,
    pub reference: ScheduleReference,
    pub slot: DueSlot,
    pub target: ScheduleTarget,
    pub prepared_at: DateTime<Utc>,
    pub preparation: Preparation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_root: Option<FreshRoot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<clock::ClockTransfer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry: Option<retry::Retry>,
    // The original row retains its handoff unchanged. Only this index moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_retry: Option<std::num::NonZeroU32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unused: bool,
}

impl Occurrence {
    pub(crate) fn unused(&self) -> bool {
        self.unused
    }

    fn storage_key(&self) -> &str {
        if self.retry.is_some() {
            &self.reference.delivery_id
        } else {
            &self.reference.occurrence_id
        }
    }

    fn validate(&self, agent: &AgentDb) -> anyhow::Result<()> {
        self.reference.validate()?;
        anyhow::ensure!(
            matches!(self.version, 1..=5)
                && self.reference.owner_agent_db_id == agent.id().to_string()
                && self.reference.occurrence_id
                    == super::readers::slot_key(
                        &self.reference.owner_agent_db_id,
                        &self.reference.lifetime,
                        &self.reference.revision,
                        &self.slot,
                    )?,
            "schedule occurrence identity conflicts or has an unsupported version"
        );
        let handed_off = matches!(self.preparation, Preparation::HandedOff { .. });
        anyhow::ensure!(
            !self.unused || (handed_off && self.latest_retry.is_none()),
            "invalid unused handoff"
        );
        anyhow::ensure!(
            match &self.retry {
                Some(retry) =>
                    self.version == 5
                        && handed_off
                        && self.clock.is_none()
                        && self.latest_retry.is_none()
                        && retry.is_valid(&self.reference),
                None =>
                    self.version < 5
                        && (self.latest_retry.is_none() || (self.version >= 3 && handed_off)),
            },
            "schedule retry identity differs"
        );
        anyhow::ensure!(
            match self.publication_sequence {
                Some(sequence) => sequence > 0 && self.version >= 3 && handed_off,
                None => self.version < 3 || (self.version == 4 && !handed_off),
            },
            "schedule publication identity differs"
        );
        anyhow::ensure!(
            self.fresh_root.is_none()
                || (self.version >= 2 && matches!(self.target, ScheduleTarget::Fresh)),
            "Fresh root on an incompatible occurrence"
        );
        if let Some(FreshRoot::Bound { session_db_id }) = &self.fresh_root {
            eidetica::entry::ID::parse(session_db_id)?;
        }
        if let Preparation::HandedOff { input, .. } = &self.preparation {
            if self.version >= 2 && matches!(self.target, ScheduleTarget::Fresh) {
                anyhow::ensure!(
                    self.fresh_root
                        == Some(FreshRoot::Bound {
                            session_db_id: input.session_db_id.clone()
                        }),
                    "Fresh handoff lacks its exact bound root"
                );
            }
            anyhow::ensure!(
                input.version == 1 && input.reference == self.reference,
                "schedule handoff reference differs"
            );
            if let ScheduleTarget::Pinned { session_db_id } = &self.target {
                anyhow::ensure!(
                    session_db_id == &input.session_db_id,
                    "schedule handoff target differs"
                );
            }
        }
        clock::validate(self)?;
        Ok(())
    }
}

/// Inspect immutable identities without resolving any private definition. An
/// unsupported or conflicting row holds the ledger, not an empty result.
pub async fn occurrences(agent: &AgentDb) -> anyhow::Result<Vec<Occurrence>> {
    let Some(mut scan) = OccurrenceScan::open(agent).await? else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    let mut deliveries = std::collections::HashSet::new();
    while let Some(page) = scan.next_page(agent, 128).await? {
        for row in page {
            anyhow::ensure!(
                deliveries.insert(row.reference.delivery_id.clone()),
                "schedule occurrence/delivery identity conflicts"
            );
            rows.push(row);
        }
    }
    Ok(rows)
}

async fn read_occurrence(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    occurrence_id: &str,
) -> anyhow::Result<Option<Occurrence>> {
    let row = read_record(txn, agent, occurrence_id).await?;
    anyhow::ensure!(
        row.as_ref().is_none_or(|row| row.retry.is_none()),
        "schedule occurrence key names a retry"
    );
    Ok(row)
}

async fn read_record(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    key: &str,
) -> anyhow::Result<Option<Occurrence>> {
    let row = optional(&txn.get_store::<Table<Occurrence>>(OCCURRENCES).await?, key).await?;
    if let Some(row) = &row {
        row.validate(agent)?;
        order::validate_link(txn, agent, row).await?;
        clock::validate_link(txn, agent, row).await?;
        anyhow::ensure!(row.storage_key() == key, "schedule occurrence key differs");
    }
    Ok(row)
}

async fn write(
    cache: &mut AccountingCache,
    txn: eidetica::Transaction,
    row: &Occurrence,
    state: AccountingState,
) -> anyhow::Result<()> {
    state.validate()?;
    txn.get_store::<Table<AccountingState>>(STATE)
        .await?
        .set(&row.reference.lifetime, state.clone())
        .await?;
    txn.get_store::<Table<Occurrence>>(OCCURRENCES)
        .await?
        .set(row.storage_key(), row.clone())
        .await?;
    cache.commit(txn, &row.reference.lifetime, state).await
}

/// Allocate one delivery and one success-capacity reservation for an exact
/// clock slot. Repeating the slot returns the same record, including a refused
/// one: it does not replenish capacity or implicitly retry a delivery.
pub async fn prepare(
    agent: &AgentDb,
    schedule_id: &str,
    expected_revision: &str,
    slot: DueSlot,
    now: DateTime<Utc>,
) -> anyhow::Result<Occurrence> {
    prepare_inner(agent, schedule_id, expected_revision, slot, now, false).await
}

/// Pin a timer's exact slot and interval lineage before Session preparation.
/// The caller retains/revalidates its disarmed engine token through handoff;
/// this API grants neither missing-generation authority nor automatic replay.
pub async fn prepare_clock(
    agent: &AgentDb,
    schedule_id: &str,
    expected_revision: &str,
    slot: DueSlot,
    now: DateTime<Utc>,
) -> anyhow::Result<Occurrence> {
    prepare_inner(agent, schedule_id, expected_revision, slot, now, true).await
}

/// Apply catch-up policy to a durable clock slot the engine saw already due
/// before it started. An existing occurrence for the slot is not a missed
/// trigger (`Owned`). Skipping writes no occurrence, reservation or Session.
pub async fn miss_clock(
    agent: &AgentDb,
    schedule_id: &str,
    expected_revision: &str,
    slot: DueSlot,
    now: DateTime<Utc>,
) -> anyhow::Result<clock::MissedClock> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = agent
        .find_schedule(schedule_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule was removed"))?;
    anyhow::ensure!(
        !schedule.legacy_migration_held(),
        "legacy schedule has no executable migration state"
    );
    anyhow::ensure!(
        schedule.revision_key()? == expected_revision,
        "clock dispatch names a stale schedule revision"
    );
    let (txn, state) = cache.load(agent, &schedule).await?;
    let occurrence_id = schedule.occurrence_key(&agent.id().to_string(), &slot)?;
    if read_occurrence(&txn, agent, &occurrence_id)
        .await?
        .is_some()
    {
        return Ok(clock::MissedClock::Owned);
    }
    let (outcome, changed) = clock::miss(&txn, agent, &schedule, &slot, now).await?;
    state.validate()?;
    if changed {
        cache.commit(txn, &schedule.lifetime_key(), state).await?;
    }
    Ok(outcome)
}

async fn prepare_inner(
    agent: &AgentDb,
    schedule_id: &str,
    expected_revision: &str,
    slot: DueSlot,
    now: DateTime<Utc>,
    timed: bool,
) -> anyhow::Result<Occurrence> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = agent
        .find_schedule(schedule_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule was removed"))?;
    anyhow::ensure!(
        !schedule.legacy_migration_held(),
        "legacy schedule has no executable migration state"
    );
    anyhow::ensure!(
        schedule.revision_key()? == expected_revision,
        "clock dispatch names a stale schedule revision"
    );
    match (&schedule.trigger, &slot) {
        (crate::routine::Trigger::OneShot { fire_at }, DueSlot::Calendar { due_at }) => {
            anyhow::ensure!(
                fire_at == due_at && *due_at <= now,
                "one-shot slot differs or is not due"
            );
        }
        (crate::routine::Trigger::Cron { .. }, DueSlot::Calendar { due_at }) => {
            anyhow::ensure!(*due_at <= now, "calendar slot is not due");
        }
        (crate::routine::Trigger::Interval { period }, DueSlot::Interval { anchor, .. }) => {
            anyhow::ensure!(
                crate::routine::types::next_interval_fire(*period, *anchor)
                    .is_some_and(|due| due <= now),
                "interval slot is not due"
            );
        }
        _ => anyhow::bail!("schedule trigger and slot kinds differ"),
    }
    let (txn, mut state) = cache.load(agent, &schedule).await?;
    let occurrence_id = schedule.occurrence_key(&agent.id().to_string(), &slot)?;
    if let Some(row) = read_occurrence(&txn, agent, &occurrence_id).await? {
        anyhow::ensure!(
            row.target == schedule.target,
            "schedule occurrence target differs"
        );
        anyhow::ensure!(
            row.clock.is_some() == timed,
            "cannot change an occurrence's clock protocol"
        );
        return Ok(row);
    }
    anyhow::ensure!(
        schedule.enabled
            && !state.retired
            && !state.consumed
            && schedule.expires_at.is_none_or(|expiry| now < expiry)
            && schedule.max_fires.is_none_or(|max| state.fire_count < max),
        "schedule is disabled, retired, consumed, expired or at capacity"
    );
    let one_shot = matches!(schedule.trigger, crate::routine::Trigger::OneShot { .. });
    anyhow::ensure!(
        !one_shot || state.reservations == 0,
        "one-shot reservation held"
    );
    let mut row = Occurrence {
        version: 2,
        reference: ScheduleReference {
            version: 1,
            owner_agent_db_id: agent.id().to_string(),
            schedule_id: schedule.id.clone(),
            lifetime: schedule.lifetime_key(),
            revision: schedule.revision_key()?,
            occurrence_id,
            delivery_id: uuid::Uuid::new_v4().to_string(),
        },
        slot,
        target: schedule.target.clone(),
        prepared_at: now,
        preparation: Preparation::Prepared,
        publication_sequence: None,
        fresh_root: None,
        clock: None,
        retry: None,
        latest_retry: None,
        unused: false,
    };
    if timed {
        clock::prepare(&txn, agent, &schedule, &mut row).await?;
    }
    agent
        .resolve_schedule_reference(&row.reference, now)
        .await?;
    state.reservations = state
        .reservations
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("schedule reservation overflow"))?;
    state.fire_count = state
        .fire_count
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("schedule capacity overflow"))?;
    write(&mut cache, txn, &row, state).await?;
    Ok(row)
}

/// Acknowledge an exact persisted inert Session input, bind Fresh's Session
/// before publication, and consume the original trigger. No attempt is Started
/// here. A failed/uncertain commit must be read back by these same identities;
/// never refund it or run work solely because the Session Ack exists.
pub async fn handoff(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session: &Session,
    now: DateTime<Utc>,
) -> anyhow::Result<Occurrence> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = agent.resolve_schedule_reference(reference, now).await?;
    let input = session
        .schedule_input(reference)
        .await?
        .ok_or_else(|| anyhow::anyhow!("inert scheduled input missing"))?;
    if let ScheduleTarget::Pinned { session_db_id } = &schedule.target {
        anyhow::ensure!(
            session_db_id == &input.session_db_id,
            "scheduled target differs"
        );
    }
    let (txn, mut state) = cache.load(agent, &schedule).await?;
    let mut row = read_occurrence(&txn, agent, &reference.occurrence_id)
        .await?
        .filter(|row| row.reference == *reference)
        .ok_or_else(|| anyhow::anyhow!("prepared schedule occurrence missing"))?;
    anyhow::ensure!(
        row.target == schedule.target,
        "schedule occurrence target differs"
    );
    match &row.preparation {
        Preparation::HandedOff { input: prior, .. } => {
            anyhow::ensure!(
                prior.as_ref() == &input,
                "schedule input rebound after handoff"
            );
            return Ok(row);
        }
        Preparation::NotApplied => anyhow::bail!("schedule preparation was released"),
        Preparation::Prepared => {}
    }
    // Its own final-slot reservation is already included in fire_count.
    anyhow::ensure!(
        !state.retired
            && !state.consumed
            && schedule.max_fires.is_none_or(|max| state.fire_count <= max),
        "schedule is retired, consumed or over capacity"
    );
    let fresh = matches!(schedule.target, ScheduleTarget::Fresh);
    anyhow::ensure!(
        matches!(row.version, 2 | 4),
        "legacy preparation requires explicit reconciliation"
    );
    if fresh {
        anyhow::ensure!(
            row.fresh_root
                == Some(FreshRoot::Bound {
                    session_db_id: input.session_db_id.clone()
                }),
            "Fresh session must be bound before handoff"
        );
    }
    write_fire(&txn, &input, fresh, now).await?;
    row.publication_sequence =
        Some(order::append(&txn, agent, reference, cache.unpublished_only()).await?);
    clock::advance(&txn, agent, &schedule, &mut row, now).await?;
    row.version = if row.clock.is_some() { 4 } else { 3 };
    row.preparation = Preparation::HandedOff {
        input: Box::new(input),
        at: now,
    };
    state.handoff_count = state
        .handoff_count
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("schedule handoff overflow"))?;
    state.consumed |= matches!(schedule.trigger, crate::routine::Trigger::OneShot { .. });
    write(&mut cache, txn, &row, state).await?;
    Ok(row)
}

async fn write_fire(
    txn: &eidetica::Transaction,
    input: &ScheduleInputAnchor,
    fresh: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let reference = &input.reference;
    let fires = txn.get_store::<Table<ScheduleFire>>(FIRES).await?;
    anyhow::ensure!(
        optional(&fires, &reference.delivery_id).await?.is_none(),
        "schedule handoff identity already exists"
    );
    fires
        .set(
            &reference.delivery_id,
            ScheduleFire {
                schedule_id: reference.schedule_id.clone(),
                firing_id: Some(reference.delivery_id.clone()),
                lifetime: Some(reference.lifetime.clone()),
                request_id: Some(input.request_id.clone()),
                attempt_id: None,
                fired_at: now,
                session_db_id: input.session_db_id.clone(),
                fresh,
                usage: None,
            },
        )
        .await?;
    Ok(())
}

/// Revalidate the durable successor of the clock's admission guard. A consumed
/// trigger is expected here: only its exact committed handoff authorizes input,
/// never an inert Ack, a missing engine generation, or a retained definition.
/// No writer initialization or accounting reconciliation runs on this path.
pub async fn authorized_input(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session: &Session,
    now: DateTime<Utc>,
) -> anyhow::Result<Schedule> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = agent.resolve_schedule_reference(reference, now).await?;
    let (txn, state) = cache.load(agent, &schedule).await?;
    let row = validate_input_handoff(agent, reference, session, &txn, &schedule).await?;
    anyhow::ensure!(
        !state.retired && schedule.max_fires.is_none_or(|max| state.fire_count <= max),
        "scheduled accounting authority differs"
    );
    anyhow::ensure!(!row.unused, "scheduled delivery was closed without binding");
    anyhow::ensure!(
        retry::is_latest(&txn, agent, &row).await?,
        "scheduled delivery was superseded"
    );
    Ok(schedule)
}

async fn validate_input_handoff(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session: &Session,
    txn: &eidetica::Transaction,
    schedule: &Schedule,
) -> anyhow::Result<Occurrence> {
    let row = retry::read_delivery(txn, agent, &reference.occurrence_id, &reference.delivery_id)
        .await?
        .filter(|row| row.reference == *reference && row.target == schedule.target)
        .ok_or_else(|| anyhow::anyhow!("scheduled occurrence differs"))?;
    let Preparation::HandedOff { input, .. } = &row.preparation else {
        anyhow::bail!("scheduled input has no committed handoff");
    };
    anyhow::ensure!(
        session.schedule_input(reference).await?.as_ref() == Some(input.as_ref()),
        "scheduled handoff target differs"
    );
    Ok(row)
}

/// Once Included, edits/removal are prospective: they cannot replace private
/// text or fail the containing human turn. Only this exact still-Started
/// attempt may retain its immutable input. The caller must still revalidate
/// current Agent keys, home and membership; this is not historical replay.
pub(crate) async fn retained_input(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session: &Session,
    attempt_id: &str,
) -> anyhow::Result<Schedule> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = archived_definition(agent, reference).await?;
    let (txn, _) = cache.load(agent, &schedule).await?;
    validate_input_handoff(agent, reference, session, &txn, &schedule).await?;
    let evidence = session
        .schedule_delivery_evidence(reference)
        .await?
        .ok_or_else(|| anyhow::anyhow!("retained input receipt missing"))?;
    anyhow::ensure!(
        evidence.input.attempt_id == attempt_id
            && matches!(evidence.input.state, crate::session::steering::JobInputState::Included { .. })
            && evidence.completion.is_none()
            && session.read_turn_attempt(attempt_id).await?.is_some_and(|attempt|
                attempt.status == crate::session::TurnAttemptStatus::Started),
        "retained input is not Included in this open attempt"
    );
    Ok(schedule)
}

/// Release positively unused preparation only. This cannot refund a handoff,
/// erase its history, rearm a consumed one-shot or change a definition. Late
/// cleanup can settle an old revision but never a remove/recreate replacement.
pub async fn release_prepared(
    agent: &AgentDb,
    reference: &ScheduleReference,
) -> anyhow::Result<()> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = archived_definition(agent, reference).await?;
    // Archived material supplies the old accounting identity only, not renewed
    // execution authority. The definition can have been removed/replaced.
    let (txn, mut state) = cache.load(agent, &schedule).await?;
    let mut row = read_occurrence(&txn, agent, &reference.occurrence_id)
        .await?
        .filter(|row| row.reference == *reference)
        .ok_or_else(|| anyhow::anyhow!("prepared schedule occurrence missing"))?;
    match row.preparation {
        Preparation::NotApplied => return Ok(()),
        Preparation::HandedOff { .. } => {
            anyhow::bail!("handoff outcome may be uncertain; cannot release preparation")
        }
        Preparation::Prepared => {}
    }
    state.reservations = state
        .reservations
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("schedule reservation missing"))?;
    state.fire_count = state
        .fire_count
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("schedule capacity missing"))?;
    row.preparation = Preparation::NotApplied;
    write(&mut cache, txn, &row, state).await
}

/// Frozen identity for accounting only; never permission to run old work.
async fn archived_definition(
    agent: &AgentDb,
    reference: &ScheduleReference,
) -> anyhow::Result<Schedule> {
    reference.validate()?;
    anyhow::ensure!(
        reference.owner_agent_db_id == agent.id().to_string(),
        "wrong schedule Agent"
    );
    let archived = agent
        .database()
        .new_transaction()
        .await?
        .get_store::<Table<crate::agent_db::ScheduleRevision>>("schedule_revisions")
        .await?
        .get(&reference.revision)
        .await?;
    anyhow::ensure!(
        archived.version == 1
            && archived.revision == reference.revision
            && archived.definition.revision_key()? == reference.revision
            && archived.definition.id == reference.schedule_id
            && archived.definition.lifetime_key() == reference.lifetime,
        "schedule revision custody differs"
    );
    Ok(archived.definition)
}

/// Bounded accounting maintenance for history whose definition was removed or
/// replaced. Archived identity permits projection only, never renewed delivery.
pub(crate) async fn reconcile_reference(
    agent: &AgentDb,
    reference: &ScheduleReference,
    limit: usize,
) -> anyhow::Result<AccountingProgress> {
    reconcile(agent, &archived_definition(agent, reference).await?, limit).await
}

#[cfg(test)]
mod tests;
