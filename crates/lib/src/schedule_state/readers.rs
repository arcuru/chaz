//! Reader-first lifecycle identities and accounting. No occurrence/input writer
//! or replay is enabled here. Existing dispatch records remain evidence, not
//! inferred successful execution; rollback to a writer ignoring this split is
//! unsafe and must not be mistaken for a data rollback.
use super::*;
use crate::agent_db::{AgentDb, ScheduleTarget};

/// A Session/Executor may retain this reference, never the private definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleReference {
    pub version: u32,
    pub owner_agent_db_id: String,
    pub schedule_id: String,
    pub lifetime: String,
    pub revision: String,
    pub occurrence_id: String,
    pub delivery_id: String,
}

impl ScheduleReference {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "unsupported scheduled reference version");
        anyhow::ensure!(
            [
                &self.owner_agent_db_id,
                &self.schedule_id,
                &self.lifetime,
                &self.revision,
                &self.occurrence_id,
                &self.delivery_id
            ]
            .iter()
            .all(|id| !id.is_empty()),
            "incomplete scheduled reference"
        );
        Ok(())
    }
}

/// Calendar slots and interval lineages cannot alias each other. An interval's
/// anchor/ordinal is durable, not recomputed from a new process's clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum DueSlot {
    Calendar { due_at: DateTime<Utc> },
    Interval { anchor: DateTime<Utc>, ordinal: u64 },
}

pub fn occurrence_id(
    owner_agent_db_id: &str,
    schedule: &Schedule,
    slot: &DueSlot,
) -> anyhow::Result<String> {
    slot_key(
        owner_agent_db_id,
        &schedule.lifetime_key(),
        &schedule.revision_key()?,
        slot,
    )
}

pub(super) fn slot_key(
    owner_agent_db_id: &str,
    lifetime: &str,
    revision: &str,
    slot: &DueSlot,
) -> anyhow::Result<String> {
    Ok(format!(
        "v1:{}",
        blake3::hash(&serde_json::to_vec(&(
            owner_agent_db_id,
            lifetime,
            revision,
            slot
        ))?)
    ))
}

/// The next writer's shape, understood before that writer is enabled. Keep the
/// old capacity field conservative for inspection; it is not a success count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountingState {
    pub version: u32,
    pub fire_count: u32,
    pub legacy_baseline: u32,
    pub successful_fires: u32,
    pub reservations: u32,
    pub handoff_count: u32,
    pub consumed: bool,
    pub retired: bool,
}

impl AccountingState {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 2, "unsupported schedule accounting version");
        let used = self
            .legacy_baseline
            .checked_add(self.successful_fires)
            .and_then(|count| count.checked_add(self.reservations));
        anyhow::ensure!(
            used == Some(self.fire_count),
            "schedule accounting capacity differs"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum AccountingView {
    /// Old accepted dispatches may have failed or be uncertain. Preserve their
    /// capacity without relabelling them as successes or refunding them.
    Legacy {
        capacity_used: u32,
        consumed: bool,
        retired: bool,
    },
    Current(AccountingState),
}

pub(super) fn accounting(raw: serde_json::Value) -> anyhow::Result<AccountingView> {
    match raw.get("version") {
        None => {
            let state: ScheduleState = serde_json::from_value(raw)?;
            Ok(AccountingView::Legacy {
                capacity_used: state.fire_count,
                consumed: state.consumed,
                retired: state.retired,
            })
        }
        Some(version) if version.as_u64() == Some(2) => {
            let state: AccountingState = serde_json::from_value(raw)?;
            state.validate()?;
            Ok(AccountingView::Current(state))
        }
        _ => anyhow::bail!("unsupported schedule accounting version"),
    }
}

/// Pure inspection output: no prompt body, no claim of delivery/fulfillment,
/// no new database and no authorization to migrate, run or refund legacy work.
#[derive(Clone, Debug, Serialize)]
pub struct SchedulePreview {
    pub schedule_id: String,
    pub lifetime: String,
    pub revision: String,
    pub legacy_identity: bool,
    pub target: ScheduleTarget,
    pub enabled: bool,
    pub operational_initialized: bool,
    pub accounting: AccountingView,
    pub pending_legacy_reservations: usize,
    pub recorded_handoffs: usize,
}

/// Read one selected operational snapshot. A present but unavailable pointer,
/// unknown schema, malformed state or missing reservation state holds the read;
/// none of them is an empty ledger or a reason to initialize replacement data.
pub async fn preview(agent: &AgentDb) -> anyhow::Result<Vec<SchedulePreview>> {
    let operations = agent.scheduling_database().await?;
    let txn = match operations {
        Some(db) => Some(db.new_transaction_at(&db.snapshot().await?).await?),
        None => None,
    };
    let mut result = Vec::new();
    for schedule in agent.list_schedules().await? {
        let mut pending = 0;
        let mut handed_off = 0;
        let state = if let Some(txn) = &txn {
            let states = txn.get_store::<Table<serde_json::Value>>(STATE).await?;
            let raw = optional(&states, &schedule.lifetime_key()).await?;
            pending = txn
                .get_store::<Table<Reservation>>(RESERVATIONS)
                .await?
                .search(|r: &Reservation| r.lifetime == schedule.lifetime_key())
                .await?
                .len();
            handed_off = txn
                .get_store::<Table<ScheduleFire>>(FIRES)
                .await?
                .search(|fire: &ScheduleFire| {
                    fire.lifetime.as_deref() == Some(schedule.lifetime_key().as_str())
                })
                .await?
                .len();
            anyhow::ensure!(
                raw.is_some() || (pending == 0 && handed_off == 0),
                "schedule reservation/handoff state missing"
            );
            raw.map(accounting)
                .transpose()?
                .unwrap_or(AccountingView::Legacy {
                    capacity_used: schedule.fire_count,
                    consumed: false,
                    retired: false,
                })
        } else {
            AccountingView::Legacy {
                capacity_used: schedule.fire_count,
                consumed: false,
                retired: false,
            }
        };
        match &state {
            AccountingView::Legacy { capacity_used, .. } => {
                let minimum = pending
                    .checked_add(handed_off)
                    .and_then(|count| u32::try_from(count).ok())
                    .and_then(|count| schedule.fire_count.checked_add(count));
                anyhow::ensure!(
                    minimum.is_some_and(|minimum| *capacity_used >= minimum),
                    "legacy schedule capacity is below its baseline/reservations"
                );
            }
            AccountingView::Current(state) => {
                anyhow::ensure!(
                    state.legacy_baseline >= schedule.fire_count,
                    "schedule legacy baseline regressed"
                );
                anyhow::ensure!(
                    pending == 0,
                    "legacy reservations require reconciliation before versioned accounting"
                );
            }
        }
        result.push(SchedulePreview {
            schedule_id: schedule.id.clone(),
            lifetime: schedule.lifetime_key(),
            revision: schedule.revision_key()?,
            legacy_identity: schedule.lifetime.is_none(),
            target: schedule.target,
            enabled: schedule.enabled,
            operational_initialized: operations.is_some(),
            accounting: state,
            pending_legacy_reservations: pending,
            recorded_handoffs: handed_off,
        });
    }
    result.sort_by(|a, b| a.schedule_id.cmp(&b.schedule_id));
    Ok(result)
}

#[cfg(test)]
mod tests;
