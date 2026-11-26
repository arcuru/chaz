//! Scheduling operations in one Agent-selected, owned Executor DB.
//!
//! Definitions stay on the Agent; reservations and accepted handoffs do not.
//! A reservation is conservative after a crash: only a proven pre-dispatch
//! refusal may refund it. Neither this ledger nor the Session implies replay.
use crate::agent_db::{Schedule, ScheduleFire};
use chrono::{DateTime, Utc};
use eidetica::{Database, store::Table};
use serde::{Deserialize, Serialize};

pub mod lifecycle;
pub mod readers;
pub(crate) mod serialization;

const STATE: &str = "schedule_state";
const RESERVATIONS: &str = "schedule_reservations";
const FIRES: &str = "schedule_fires";
pub(crate) const HELD: &str = "dispatch reservation held; outcome uncertain (no automatic replay)";
pub(crate) const LEGACY_ONE_SHOT_HELD: &str =
    "legacy one-shot outcome unknown; no automatic replay";
pub(crate) const LEGACY_MIGRATION_HELD: &str = "legacy definition needs migration";

pub(crate) fn effective_status(schedule: &Schedule) -> &'static str {
    match schedule.last_error.as_deref() {
        Some(HELD) => " (held: dispatch outcome uncertain; no automatic replay)",
        Some(LEGACY_ONE_SHOT_HELD) => {
            " (held: legacy one-shot outcome unknown; no automatic replay)"
        }
        Some(LEGACY_MIGRATION_HELD) => " (held: legacy definition needs migration)",
        _ if !schedule.enabled => " (disabled)",
        _ => "",
    }
}

/// Prompt-free policy and missed-window suffix for existing listings.
/// An unreadable ledger is reported as unknown, never as "nothing missed".
pub(crate) async fn policy_status(agent: &crate::agent_db::AgentDb, schedule: &Schedule) -> String {
    use lifecycle::clock::MissedDisposition;
    let mut policy = String::new();
    if schedule.effective_catch_up() {
        policy.push_str(" [catch-up on]");
    }
    if let Some(limit) = schedule.interruption_retries {
        use std::fmt::Write;
        let _ = write!(policy, " [restart retry limit: {limit}]");
    }
    let windows = match agent.schedule_missed_windows(schedule).await {
        Ok(windows) => windows,
        Err(_) => return format!("{policy} [missed windows unknown]"),
    };
    let Some(last) = windows.last() else {
        return policy;
    };
    let at_least = if windows.iter().all(|w| w.slots_exact) {
        ""
    } else {
        "at least "
    };
    let slots: u64 = windows.iter().map(|w| w.slots).sum();
    let latest = match last.disposition {
        MissedDisposition::Skipped { .. } => "skipped",
        // Selecting the slot precedes the Session input and handoff writes.
        MissedDisposition::CaughtUp => "catch-up selected",
    };
    format!(
        "{policy} [missed: {at_least}{slots} slot(s) in {} window(s); latest {} {latest}]",
        windows.len(),
        last.last_due.format("%Y-%m-%d %H:%M:%SZ"),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleState {
    pub fire_count: u32,
    pub consumed: bool,
    pub retired: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub firing_id: String,
    pub schedule_id: String,
    pub lifetime: String,
    pub one_shot: bool,
}

async fn optional<T: serde::de::DeserializeOwned + Serialize + Clone + Send + Sync + 'static>(
    store: &Table<T>,
    key: &str,
) -> anyhow::Result<Option<T>> {
    match store.get(key).await {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn baseline(schedule: &Schedule) -> ScheduleState {
    ScheduleState {
        fire_count: schedule.fire_count,
        consumed: false,
        retired: false,
    }
}

pub async fn state(db: &Database, schedule: &Schedule) -> anyhow::Result<ScheduleState> {
    let txn = db.new_transaction().await?;
    let rows = txn.get_store::<Table<serde_json::Value>>(STATE).await?;
    Ok(match optional(&rows, &schedule.lifetime_key()).await? {
        Some(raw) => match readers::accounting(raw)? {
            readers::AccountingView::Legacy {
                capacity_used,
                consumed,
                retired,
            } => ScheduleState {
                fire_count: capacity_used,
                consumed,
                retired,
            },
            readers::AccountingView::Current(state) => ScheduleState {
                fire_count: state.fire_count,
                consumed: state.consumed,
                retired: state.retired,
            },
        },
        None => baseline(schedule),
    })
}

/// Effective values are read-only projections, never definition updates.
pub async fn effective(db: &Database, mut schedule: Schedule) -> anyhow::Result<Option<Schedule>> {
    let state = state(db, &schedule).await?;
    if state.consumed {
        return Ok(None);
    }
    schedule.fire_count = state.fire_count;
    schedule.enabled &= !state.retired && schedule.retirement_reason(Utc::now()).is_none();
    if matches!(schedule.trigger, crate::routine::Trigger::OneShot { .. }) {
        let txn = db.new_transaction().await?;
        let held = txn
            .get_store::<Table<Reservation>>(RESERVATIONS)
            .await?
            .search(|r: &Reservation| r.lifetime == schedule.lifetime_key())
            .await?;
        if !held.is_empty() {
            schedule.enabled = false;
            schedule.last_error = Some(HELD.into());
        }
    }
    schedule.hold_unmigrated(Utc::now());
    Ok(Some(schedule))
}

/// Caller holds the Agent's shared process-local scheduling mutation boundary.
#[cfg(test)]
pub async fn reserve(
    db: &Database,
    schedule: &Schedule,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<Reservation>> {
    let txn = db.new_transaction().await?;
    let states = txn.get_store::<Table<ScheduleState>>(STATE).await?;
    let key = schedule.lifetime_key();
    let mut state = optional(&states, &key)
        .await?
        .unwrap_or_else(|| baseline(schedule));
    let mut projected = schedule.clone();
    projected.fire_count = state.fire_count;
    if !schedule.enabled
        || state.retired
        || state.consumed
        || projected.retirement_reason(now).is_some()
    {
        return Ok(None);
    }
    let reservations = txn.get_store::<Table<Reservation>>(RESERVATIONS).await?;
    let one_shot = matches!(schedule.trigger, crate::routine::Trigger::OneShot { .. });
    if one_shot
        && !reservations
            .search(|r: &Reservation| r.lifetime == key)
            .await?
            .is_empty()
    {
        return Ok(None);
    }
    let reservation = Reservation {
        firing_id: uuid::Uuid::new_v4().to_string(),
        schedule_id: schedule.id.clone(),
        lifetime: key.clone(),
        one_shot,
    };
    state.fire_count = state.fire_count.saturating_add(1);
    states.set(&key, state).await?;
    reservations
        .set(&reservation.firing_id, reservation.clone())
        .await?;
    txn.commit().await?;
    Ok(Some(reservation))
}

/// Refund the exact still-pending slot, not a definition or a replacement.
#[cfg(test)]
pub async fn refund(db: &Database, reservation: &Reservation) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let reservations = txn.get_store::<Table<Reservation>>(RESERVATIONS).await?;
    let Some(prior) = optional(&reservations, &reservation.firing_id).await? else {
        return Ok(());
    };
    anyhow::ensure!(
        prior.lifetime == reservation.lifetime,
        "schedule reservation lifetime differs"
    );
    let states = txn.get_store::<Table<ScheduleState>>(STATE).await?;
    let mut state = optional(&states, &reservation.lifetime)
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule reservation state missing"))?;
    state.fire_count = state.fire_count.saturating_sub(1);
    states.set(&reservation.lifetime, state).await?;
    reservations.delete(&reservation.firing_id).await?;
    txn.commit().await?;
    Ok(())
}

/// The acknowledged dispatch boundary. Failure is uncertain: do not refund,
/// retry this commit, or run the turn. Execution results live in the Session.
#[cfg(test)]
pub async fn handoff(
    db: &Database,
    reservation: &Reservation,
    fire: ScheduleFire,
) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let reservations = txn.get_store::<Table<Reservation>>(RESERVATIONS).await?;
    let prior = optional(&reservations, &reservation.firing_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule reservation missing"))?;
    anyhow::ensure!(
        prior.lifetime == reservation.lifetime,
        "schedule reservation lifetime differs"
    );
    let states = txn.get_store::<Table<ScheduleState>>(STATE).await?;
    let mut state = optional(&states, &reservation.lifetime)
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule reservation state missing"))?;
    state.consumed |= reservation.one_shot;
    states.set(&reservation.lifetime, state).await?;
    txn.get_store::<Table<ScheduleFire>>(FIRES)
        .await?
        .set(&reservation.firing_id, fire)
        .await?;
    reservations.delete(&reservation.firing_id).await?;
    txn.commit().await?;
    Ok(())
}

pub async fn retire(db: &Database, schedule: &Schedule, retired: bool) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let states = txn.get_store::<Table<serde_json::Value>>(STATE).await?;
    let raw = optional(&states, &schedule.lifetime_key()).await?;
    let value = match raw.map(readers::accounting).transpose()? {
        Some(readers::AccountingView::Current(mut state)) => {
            state.retired = retired;
            serde_json::to_value(state)?
        }
        prior => {
            let mut state = match prior {
                Some(readers::AccountingView::Legacy {
                    capacity_used,
                    consumed,
                    retired,
                }) => ScheduleState {
                    fire_count: capacity_used,
                    consumed,
                    retired,
                },
                _ => baseline(schedule),
            };
            state.retired = retired;
            serde_json::to_value(state)?
        }
    };
    states.set(schedule.lifetime_key(), value).await?;
    txn.commit().await?;
    Ok(())
}

pub async fn fires(db: &Database) -> anyhow::Result<Vec<ScheduleFire>> {
    let txn = db.new_transaction().await?;
    Ok(txn
        .get_store::<Table<ScheduleFire>>(FIRES)
        .await?
        .search(|_: &ScheduleFire| true)
        .await?
        .into_iter()
        .map(|(_, r)| r)
        .collect())
}
