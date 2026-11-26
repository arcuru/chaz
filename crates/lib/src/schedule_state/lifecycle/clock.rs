//! Durable clock custody in the selected Executor. Preparation pins the exact
//! due slot; handoff advances its successor in the same transaction as ordered
//! publication. A peer-local timer cursor is only a projection of this record.
//! A missed window (a durable slot already due before the engine started) is
//! recorded here: catch-up off skips the cursor past it without an occurrence;
//! catch-up on lets that one slot become the window's single occurrence. None
//! of this authorizes restart/retry of an existing delivery.
use super::*;
use crate::routine::Trigger;
use std::str::FromStr;

pub(super) const CLOCKS: &str = "schedule_clocks";
pub(super) const MISSED: &str = "schedule_missed_windows";
/// Bound on counting coalesced cron slots; larger windows report a minimum.
const MISSED_COUNT_LIMIT: u64 = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockHandoff {
    pub at: DateTime<Utc>,
    pub next: Option<DueSlot>,
}

/// Versioned, prompt-free custody carried by a timed occurrence. Prior strict
/// readers refuse occurrence version 4 instead of ignoring its clock boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockTransfer {
    pub version: u32,
    pub position: u64,
    pub trigger: Trigger,
    pub handoff: Option<ClockHandoff>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockCursor {
    pub version: u32,
    pub owner_agent_db_id: String,
    pub lifetime: String,
    pub revision: String,
    pub position: u64,
    pub next: Option<DueSlot>,
    pub last: Option<ScheduleReference>,
    /// Version 2 only: the slot the last handoff (or initial seed) named,
    /// when missed windows since then were skipped without an occurrence.
    /// It keeps the predecessor link verifiable; it is not runnable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed_from: Option<DueSlot>,
}

impl ClockCursor {
    /// The successor the predecessor handoff actually committed.
    fn origin(&self) -> Option<DueSlot> {
        self.missed_from.clone().or_else(|| self.next.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MissedDisposition {
    /// Catch-up off: no occurrence; the clock moved to `next`.
    Skipped { next: Option<DueSlot> },
    /// Catch-up on: the first slot is the window's one occurrence and the
    /// remaining slots are coalesced into it.
    CaughtUp,
}

/// Prompt-free, immutable record of one missed window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissedWindow {
    pub version: u32,
    pub owner_agent_db_id: String,
    pub schedule_id: String,
    pub lifetime: String,
    pub revision: String,
    pub first: DueSlot,
    pub first_due: DateTime<Utc>,
    pub last_due: DateTime<Utc>,
    pub slots: u64,
    /// False when `slots` stopped at the counting bound.
    pub slots_exact: bool,
    pub observed_at: DateTime<Utc>,
    pub disposition: MissedDisposition,
}

/// Result of applying the catch-up policy to a missed slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MissedClock {
    Skipped(Option<DueSlot>),
    CatchUp,
    /// The slot already has an occurrence; it is not a missed trigger.
    Owned,
}

fn slot_due(trigger: &Trigger, slot: &DueSlot) -> anyhow::Result<DateTime<Utc>> {
    Ok(match (trigger, slot) {
        (Trigger::Interval { period }, DueSlot::Interval { anchor, .. }) => {
            crate::routine::types::next_interval_fire(*period, *anchor)
                .ok_or_else(|| anyhow::anyhow!("invalid interval clock"))?
        }
        (Trigger::Cron { .. } | Trigger::OneShot { .. }, DueSlot::Calendar { due_at }) => *due_at,
        _ => anyhow::bail!("clock trigger and slot differ"),
    })
}

/// First/last due boundary and slot count of a window ending at `now`.
fn missed_span(
    trigger: &Trigger,
    slot: &DueSlot,
    now: DateTime<Utc>,
) -> anyhow::Result<(DateTime<Utc>, DateTime<Utc>, u64, bool)> {
    let first = slot_due(trigger, slot)?;
    anyhow::ensure!(first <= now, "missed slot is not due");
    Ok(match trigger {
        Trigger::OneShot { .. } => (first, first, 1, true),
        Trigger::Cron { expr } => {
            let cron = cron::Schedule::from_str(expr)?;
            let (mut last, mut slots) = (first, 1u64);
            for due in cron.after(&first).take_while(|due| *due <= now) {
                if slots == MISSED_COUNT_LIMIT {
                    return Ok((first, last, slots, false));
                }
                last = due;
                slots += 1;
            }
            (first, last, slots, true)
        }
        Trigger::Interval { period } => {
            let period = chrono::Duration::from_std(*period)?;
            let steps = (now - first).num_milliseconds() / period.num_milliseconds().max(1);
            match i32::try_from(steps)
                .ok()
                .and_then(|steps| period.checked_mul(steps))
                .and_then(|span| first.checked_add_signed(span))
            {
                Some(last) => (first, last, u64::try_from(steps)? + 1, true),
                None => (first, first, 1, false),
            }
        }
    })
}

fn successor(
    trigger: &Trigger,
    slot: &DueSlot,
    at: DateTime<Utc>,
) -> anyhow::Result<Option<DueSlot>> {
    Ok(match (trigger, slot) {
        (Trigger::OneShot { fire_at }, DueSlot::Calendar { due_at }) if fire_at == due_at => None,
        (Trigger::Cron { expr }, DueSlot::Calendar { due_at }) => {
            let cron = cron::Schedule::from_str(expr)?;
            anyhow::ensure!(
                cron.includes(*due_at),
                "clock slot is not on its cron schedule"
            );
            // Clock rollback must not return the same already-consumed slot.
            cron.after(&at.max(*due_at))
                .next()
                .map(|due_at| DueSlot::Calendar { due_at })
        }
        (Trigger::Interval { period }, DueSlot::Interval { anchor, ordinal }) => {
            let due = crate::routine::types::next_interval_fire(*period, *anchor)
                .ok_or_else(|| anyhow::anyhow!("invalid interval clock"))?;
            anyhow::ensure!(at >= due, "interval handoff predates its due slot");
            crate::routine::types::next_interval_fire(*period, at)
                .ok_or_else(|| anyhow::anyhow!("interval successor is unrepresentable"))?;
            Some(DueSlot::Interval {
                anchor: at,
                ordinal: ordinal
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("interval ordinal overflow"))?,
            })
        }
        _ => anyhow::bail!("clock trigger and slot differ"),
    })
}

pub(super) fn validate(row: &Occurrence) -> anyhow::Result<()> {
    let Some(clock) = &row.clock else {
        anyhow::ensure!(row.version != 4, "timed occurrence lacks clock custody");
        return Ok(());
    };
    anyhow::ensure!(
        row.version == 4 && clock.version == 1,
        "unsupported clock custody"
    );
    match (&row.preparation, &clock.handoff) {
        (Preparation::HandedOff { at, .. }, Some(handoff)) => anyhow::ensure!(
            handoff.at == *at && handoff.next == successor(&clock.trigger, &row.slot, *at)?,
            "clock successor differs from handoff"
        ),
        (Preparation::Prepared | Preparation::NotApplied, None) => {
            // Validate the trigger/slot even before a handoff exists.
            successor(
                &clock.trigger,
                &row.slot,
                slot_due(&clock.trigger, &row.slot)?,
            )?;
        }
        _ => anyhow::bail!("clock handoff state differs"),
    }
    Ok(())
}

async fn cursor_in(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    lifetime: &str,
    revision: &str,
) -> anyhow::Result<Option<ClockCursor>> {
    let Some(cursor) = optional(
        &txn.get_store::<Table<ClockCursor>>(CLOCKS).await?,
        revision,
    )
    .await?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        (cursor.version == 1 && cursor.missed_from.is_none()
            || cursor.version == 2 && cursor.missed_from.is_some())
            && cursor.owner_agent_db_id == agent.id().to_string()
            && cursor.lifetime == lifetime
            && cursor.revision == revision,
        "schedule clock identity differs"
    );
    match (&cursor.last, cursor.position) {
        (None, 0) => anyhow::ensure!(cursor.origin().is_some(), "initial clock slot missing"),
        (Some(reference), position) if position > 0 => {
            reference.validate()?;
            anyhow::ensure!(
                reference.owner_agent_db_id == cursor.owner_agent_db_id
                    && reference.lifetime == lifetime
                    && reference.revision == revision,
                "clock predecessor identity differs"
            );
            let row = txn
                .get_store::<Table<Occurrence>>(OCCURRENCES)
                .await?
                .get(&reference.occurrence_id)
                .await?;
            row.validate(agent)?;
            order::validate_link(txn, agent, &row).await?;
            validate_source(agent, &row).await?;
            let clock = row
                .clock
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("clock predecessor is untimed"))?;
            anyhow::ensure!(
                row.reference == *reference
                    && clock.position.checked_add(1) == Some(position)
                    && clock
                        .handoff
                        .as_ref()
                        .is_some_and(|handoff| handoff.next == cursor.origin()),
                "clock predecessor link differs"
            );
        }
        _ => anyhow::bail!("clock predecessor missing"),
    }
    Ok(Some(cursor))
}

/// Read-only inspection of an exact revision. Absence is not proof that no
/// prior work exists: admission still needs the complete accounting audit.
/// Missing/changed custody on a timed occurrence refuses that audit.
pub async fn cursor(agent: &AgentDb, schedule: &Schedule) -> anyhow::Result<Option<ClockCursor>> {
    let Some(db) = agent.scheduling_database().await? else {
        return Ok(None);
    };
    cursor_in(
        &db.new_transaction().await?,
        agent,
        &schedule.lifetime_key(),
        &schedule.revision_key()?,
    )
    .await
}

/// Read-only discovery of the current slot's already-owned preparation. This
/// allows a no-op reload to retain its final reservation, not admit a new slot.
/// Actual handoff still requires complete accounting and current authority.
pub(crate) async fn pending(agent: &AgentDb, schedule: &Schedule) -> anyhow::Result<bool> {
    let Some(db) = agent.scheduling_database().await? else {
        return Ok(false);
    };
    let txn = db.new_transaction().await?;
    let Some(cursor) = cursor_in(
        &txn,
        agent,
        &schedule.lifetime_key(),
        &schedule.revision_key()?,
    )
    .await?
    else {
        return Ok(false);
    };
    let Some(slot) = &cursor.next else {
        return Ok(false);
    };
    let id = schedule.occurrence_key(&agent.id().to_string(), slot)?;
    Ok(read_occurrence(&txn, agent, &id)
        .await?
        .is_some_and(|row| row.preparation == Preparation::Prepared))
}

async fn validate_source(agent: &AgentDb, row: &Occurrence) -> anyhow::Result<()> {
    let clock = row
        .clock
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("timed source missing"))?;
    anyhow::ensure!(
        archived_definition(agent, &row.reference).await?.trigger == clock.trigger,
        "clock trigger differs from its immutable source"
    );
    Ok(())
}

pub(super) async fn validate_link(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    row: &Occurrence,
) -> anyhow::Result<()> {
    let Some(clock) = &row.clock else {
        return Ok(());
    };
    validate_source(agent, row).await?;
    let cursor = cursor_in(txn, agent, &row.reference.lifetime, &row.reference.revision)
        .await?
        .ok_or_else(|| anyhow::anyhow!("timed occurrence clock missing; cannot reset lineage"))?;
    match &clock.handoff {
        None => anyhow::ensure!(
            cursor.position == clock.position && cursor.next.as_ref() == Some(&row.slot),
            "prepared clock slot differs"
        ),
        Some(handoff) => {
            anyhow::ensure!(
                cursor.position > clock.position,
                "clock has not advanced with handoff"
            );
            if cursor.position == clock.position + 1 {
                anyhow::ensure!(
                    cursor.last.as_ref() == Some(&row.reference) && cursor.origin() == handoff.next,
                    "clock handoff link differs"
                );
            }
        }
    }
    Ok(())
}

pub(super) async fn prepare(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    schedule: &Schedule,
    row: &mut Occurrence,
) -> anyhow::Result<()> {
    // The caller has audited every existing occurrence, so an absent cursor
    // cannot conceal another timed occurrence, including a different lifetime.
    let cursor = cursor_in(txn, agent, &row.reference.lifetime, &row.reference.revision)
        .await?
        .unwrap_or(ClockCursor {
            version: 1,
            owner_agent_db_id: agent.id().to_string(),
            lifetime: row.reference.lifetime.clone(),
            revision: row.reference.revision.clone(),
            position: 0,
            next: Some(row.slot.clone()),
            last: None,
            missed_from: None,
        });
    anyhow::ensure!(
        cursor.next.as_ref() == Some(&row.slot),
        "dispatch does not name the current clock slot"
    );
    row.version = 4;
    row.clock = Some(ClockTransfer {
        version: 1,
        position: cursor.position,
        trigger: schedule.trigger.clone(),
        handoff: None,
    });
    row.validate(agent)?;
    txn.get_store::<Table<ClockCursor>>(CLOCKS)
        .await?
        .set(&row.reference.revision, cursor)
        .await?;
    Ok(())
}

pub(super) async fn advance(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    schedule: &Schedule,
    row: &mut Occurrence,
    at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let Some(clock) = &mut row.clock else {
        return Ok(());
    };
    anyhow::ensure!(
        clock.trigger == schedule.trigger && clock.handoff.is_none(),
        "clock source differs"
    );
    let mut cursor = cursor_in(txn, agent, &row.reference.lifetime, &row.reference.revision)
        .await?
        .ok_or_else(|| anyhow::anyhow!("prepared clock missing"))?;
    anyhow::ensure!(
        cursor.position == clock.position && cursor.next.as_ref() == Some(&row.slot),
        "clock admission is no longer current"
    );
    let next = successor(&clock.trigger, &row.slot, at)?;
    cursor.position = cursor
        .position
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("clock position overflow"))?;
    cursor.next = next.clone();
    cursor.last = Some(row.reference.clone());
    cursor.version = 1;
    cursor.missed_from = None;
    clock.handoff = Some(ClockHandoff { at, next });
    txn.get_store::<Table<ClockCursor>>(CLOCKS)
        .await?
        .set(&row.reference.revision, cursor)
        .await?;
    Ok(())
}

/// Apply the catch-up policy to the current clock slot, which the engine
/// observed as already due before it started. The caller holds the scheduling
/// boundary, has audited accounting in `txn`, and has checked that no
/// occurrence owns this slot. Records are idempotent per slot and disposition.
pub(super) async fn miss(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    schedule: &Schedule,
    slot: &DueSlot,
    now: DateTime<Utc>,
) -> anyhow::Result<(MissedClock, bool)> {
    let lifetime = schedule.lifetime_key();
    let revision = schedule.revision_key()?;
    let mut cursor = cursor_in(txn, agent, &lifetime, &revision)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missed slot has no durable clock"))?;
    let catch_up = schedule.effective_catch_up();
    let owner = agent.id().to_string();
    let key = format!(
        "{}:{}",
        schedule.occurrence_key(&owner, slot)?,
        if catch_up { "caught_up" } else { "skipped" }
    );
    let windows = txn.get_store::<Table<MissedWindow>>(MISSED).await?;
    let prior = optional(&windows, &key).await?;
    if let Some(prior) = &prior {
        anyhow::ensure!(
            prior.version == 1
                && prior.owner_agent_db_id == owner
                && prior.schedule_id == schedule.id
                && prior.lifetime == lifetime
                && prior.revision == revision
                && &prior.first == slot,
            "missed window identity differs"
        );
    }
    if cursor.next.as_ref() != Some(slot) {
        if let Some(MissedWindow {
            disposition: MissedDisposition::Skipped { next },
            ..
        }) = prior
            && !catch_up
            && cursor.version == 2
            && cursor.next == next
        {
            return Ok((MissedClock::Skipped(next), false));
        }
        anyhow::bail!("missed slot is not the current clock slot");
    }
    let (first_due, last_due, slots, slots_exact) = missed_span(&schedule.trigger, slot, now)?;
    let disposition = if catch_up {
        MissedDisposition::CaughtUp
    } else {
        MissedDisposition::Skipped {
            next: successor(&schedule.trigger, slot, now)?,
        }
    };
    let record = MissedWindow {
        version: 1,
        owner_agent_db_id: owner,
        schedule_id: schedule.id.clone(),
        lifetime,
        revision: revision.clone(),
        first: slot.clone(),
        first_due,
        last_due,
        slots,
        slots_exact,
        observed_at: now,
        disposition: disposition.clone(),
    };
    let new_window = prior.is_none();
    if new_window {
        windows.set(&key, record).await?;
    }
    let MissedDisposition::Skipped { next } = disposition else {
        return Ok((MissedClock::CatchUp, new_window));
    };
    cursor.missed_from = cursor.origin();
    cursor.version = 2;
    cursor.next = next.clone();
    txn.get_store::<Table<ClockCursor>>(CLOCKS)
        .await?
        .set(&revision, cursor)
        .await?;
    Ok((MissedClock::Skipped(next), true))
}

/// Prompt-free missed-window history for one schedule lifetime, oldest first.
pub async fn missed_windows(
    agent: &AgentDb,
    schedule: &Schedule,
) -> anyhow::Result<Vec<MissedWindow>> {
    let Some(db) = agent.scheduling_database().await? else {
        return Ok(Vec::new());
    };
    let lifetime = schedule.lifetime_key();
    let owner = agent.id().to_string();
    let mut rows: Vec<MissedWindow> = db
        .new_transaction()
        .await?
        .get_store::<Table<MissedWindow>>(MISSED)
        .await?
        .search(|row: &MissedWindow| row.lifetime == lifetime)
        .await?
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    for row in &rows {
        anyhow::ensure!(
            row.version == 1 && row.owner_agent_db_id == owner && row.schedule_id == schedule.id,
            "missed window identity differs"
        );
    }
    rows.sort_by_key(|row| (row.observed_at, row.first_due));
    Ok(rows)
}
