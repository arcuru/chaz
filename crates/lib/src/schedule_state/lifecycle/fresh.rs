//! One inert Fresh root per delivery, before runnable handoff. A persisted
//! creation intent with no observable root is unknown, not permission to create
//! another. Only the caller that acknowledged a new intent may create once.
use super::*;

pub(crate) fn source(reference: &ScheduleReference) -> anyhow::Result<String> {
    reference.validate()?;
    Ok(format!(
        "schedule-delivery:{}:{}",
        reference.owner_agent_db_id, reference.delivery_id
    ))
}

pub(crate) async fn request(
    agent: &AgentDb,
    reference: &ScheduleReference,
    now: DateTime<Utc>,
) -> anyhow::Result<(Occurrence, bool)> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = agent.resolve_schedule_reference(reference, now).await?;
    anyhow::ensure!(
        matches!(schedule.target, ScheduleTarget::Fresh),
        "not a Fresh schedule"
    );
    let (txn, state) = cache.load(agent, &schedule).await?;
    let mut row = read_occurrence(&txn, agent, &reference.occurrence_id)
        .await?
        .filter(|row| row.reference == *reference)
        .ok_or_else(|| anyhow::anyhow!("prepared Fresh occurrence missing"))?;
    anyhow::ensure!(
        matches!(row.version, 2 | 4) && row.target == ScheduleTarget::Fresh,
        "legacy or incompatible Fresh occurrence"
    );
    anyhow::ensure!(
        matches!(row.preparation, Preparation::Prepared),
        "Fresh preparation is no longer pending"
    );
    if row.fresh_root.is_some() {
        return Ok((row, false));
    }
    row.fresh_root = Some(FreshRoot::Requested);
    write(&mut cache, txn, &row, state).await?;
    Ok((row, true))
}

/// The registry caller proves the immutable creation marker and initializes
/// only this root. Local serialization protects the point binding from edits;
/// no Agent lock is held across user/registry operations.
pub(crate) async fn bind(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session_db_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Occurrence> {
    eidetica::entry::ID::parse(session_db_id)?;
    let mut cache = agent.lock_scheduling().await;
    let schedule = agent.resolve_schedule_reference(reference, now).await?;
    anyhow::ensure!(
        matches!(schedule.target, ScheduleTarget::Fresh),
        "not a Fresh schedule"
    );
    let (txn, state) = cache.load(agent, &schedule).await?;
    let mut row = read_occurrence(&txn, agent, &reference.occurrence_id)
        .await?
        .filter(|row| row.reference == *reference)
        .ok_or_else(|| anyhow::anyhow!("prepared Fresh occurrence missing"))?;
    anyhow::ensure!(
        matches!(row.version, 2 | 4) && row.target == ScheduleTarget::Fresh,
        "legacy or incompatible Fresh occurrence"
    );
    anyhow::ensure!(
        matches!(row.preparation, Preparation::Prepared),
        "Fresh preparation is no longer pending"
    );
    match &row.fresh_root {
        Some(FreshRoot::Bound {
            session_db_id: existing,
        }) => {
            anyhow::ensure!(existing == session_db_id, "Fresh root cannot be rebound");
            return Ok(row);
        }
        Some(FreshRoot::Requested) => {}
        None => anyhow::bail!("Fresh creation intent missing"),
    }
    row.fresh_root = Some(FreshRoot::Bound {
        session_db_id: session_db_id.into(),
    });
    write(&mut cache, txn, &row, state).await?;
    Ok(row)
}
