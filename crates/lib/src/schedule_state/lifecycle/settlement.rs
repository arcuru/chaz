//! Project Session-owned evidence without turning handoff history into success.
//! The common runner and bounded reconciliation both project the same exact
//! Session evidence; neither path can authorize another execution.
use super::*;
use crate::session::{
    outcome::TurnOutcome, scheduled::ScheduleDeliveryEvidence, steering::JobInputState,
};

pub(super) const SETTLEMENTS: &str = "schedule_settlements";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Capacity {
    Held,
    Successful,
    Released,
}

pub(super) fn capacity(
    row: &Occurrence,
    evidence: &ScheduleDeliveryEvidence,
) -> anyhow::Result<Capacity> {
    let Preparation::HandedOff { input, .. } = &row.preparation else {
        anyhow::bail!("schedule settlement has no committed handoff");
    };
    anyhow::ensure!(
        !row.unused,
        "closed unused schedule handoff has binding evidence"
    );
    anyhow::ensure!(
        evidence.version == 1
            && evidence.input.version == 1
            && &evidence.input.anchor == input.as_ref()
            && !evidence.input.attempt_id.is_empty()
            && !evidence.input.request_id.as_str().is_empty(),
        "schedule settlement input differs or has an unsupported version"
    );
    if matches!(row.target, ScheduleTarget::Fresh) {
        anyhow::ensure!(
            evidence.input.request_id == input.request_id,
            "Fresh schedule evidence names another request"
        );
    }
    if let Some(completion) = &evidence.completion {
        anyhow::ensure!(
            completion.version == 1
                && completion.attempt_id == evidence.input.attempt_id
                && completion.request_id == evidence.input.request_id,
            "schedule completion identity differs or has an unsupported version"
        );
    }
    let included = matches!(evidence.input.state, JobInputState::Included { .. });
    if matches!(row.target, ScheduleTarget::Pinned { .. }) && included {
        // Delivery succeeded even if the containing human turn later failed.
        return Ok(Capacity::Successful);
    }
    if let Some(completion) = &evidence.completion
        && matches!(row.target, ScheduleTarget::Fresh)
    {
        return Ok(match completion.outcome {
            TurnOutcome::Success if included => Capacity::Successful,
            TurnOutcome::Failure | TurnOutcome::Cancelled => Capacity::Released,
            // A successful but unconfirmed input is not successful scheduled work.
            TurnOutcome::Success => Capacity::Held,
        });
    }
    Ok(
        if matches!(evidence.input.state, JobInputState::NotApplied { .. }) {
            Capacity::Released
        } else {
            Capacity::Held
        },
    )
}

pub(super) fn counted_capacity(
    row: &Occurrence,
    evidence: Option<&ScheduleDeliveryEvidence>,
) -> anyhow::Result<Capacity> {
    match evidence {
        Some(evidence) => capacity(row, evidence),
        None if row.preparation == Preparation::NotApplied || row.unused => Ok(Capacity::Released),
        None => Ok(Capacity::Held),
    }
}

pub(crate) async fn superseded(
    agent: &AgentDb,
    reference: &ScheduleReference,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(current) = agent.find_schedule(&reference.schedule_id).await? else {
        return Ok(true);
    };
    Ok(!current.enabled
        || current.lifetime_key() != reference.lifetime
        || current.revision_key()? != reference.revision
        || current.expires_at.is_some_and(|expiry| now >= expiry))
}

/// Close a positively obsolete, unused handoff without erasing it or releasing
/// somebody else's reservation. A read error, missing Ack or orphan Started
/// must not turn into a skip or refund. Definition changes serialize here.
pub(crate) async fn retire_unused(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session: &Session,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = archived_definition(agent, reference).await?;
    let (txn, mut state) = cache.load(agent, &schedule).await?;
    let mut row = validate_input_handoff(agent, reference, session, &txn, &schedule).await?;
    let Preparation::HandedOff { input, .. } = &row.preparation else {
        unreachable!()
    };
    if !retry::is_latest(&txn, agent, &row).await? || !session.schedule_input_unused(input).await? {
        return Ok(false);
    }
    if row.unused {
        return Ok(true);
    }
    if !superseded(agent, reference, now).await? {
        return Ok(false);
    }
    anyhow::ensure!(
        optional(
            &txn.get_store::<Table<ScheduleDeliveryEvidence>>(SETTLEMENTS)
                .await?,
            row.storage_key()
        )
        .await?
        .is_none(),
        "prior schedule binding was observed; no unused refund"
    );
    state.reservations = state
        .reservations
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("schedule reservation missing"))?;
    state.fire_count = state
        .fire_count
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("schedule capacity missing"))?;
    row.unused = true;
    write(&mut cache, txn, &row, state).await?;
    Ok(true)
}

fn advances(before: &ScheduleDeliveryEvidence, after: &ScheduleDeliveryEvidence) -> bool {
    use JobInputState::*;
    if before.input.anchor != after.input.anchor
        || before.input.request_id != after.input.request_id
        || before.input.attempt_id != after.input.attempt_id
        || before.input.executor != after.input.executor
        || before
            .completion
            .as_ref()
            .is_some_and(|c| after.completion.as_ref() != Some(c))
    {
        return false;
    }
    if before.input.state == after.input.state {
        return true;
    }
    match (&before.input.state, &after.input.state) {
        (
            Queued,
            Accepted | Dispatching { .. } | Included { .. } | NotApplied { .. } | Uncertain { .. },
        ) => true,
        (Accepted, Dispatching { .. } | Included { .. } | NotApplied { .. } | Uncertain { .. }) => {
            true
        }
        (Dispatching { model_sequence: a }, Included { model_sequence: b }) => a == b,
        (Dispatching { .. }, Uncertain { .. }) => true,
        _ => false,
    }
}

/// Read exact Session evidence and project it once into the selected ledger.
/// Missing/lost/unavailable evidence retains the reservation. Late settlement
/// may use archived identity after removal/disable, never renewed authority or
/// a write to the definition/replacement. Shares local serialization with
/// admission and edits; this is not cross-DB atomicity or distributed CAS.
pub async fn settle(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session: &Session,
) -> anyhow::Result<Option<ScheduleDeliveryEvidence>> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = archived_definition(agent, reference).await?;
    let (txn, mut state) = cache.load(agent, &schedule).await?;
    let row = retry::read_delivery(
        &txn,
        agent,
        &reference.occurrence_id,
        &reference.delivery_id,
    )
    .await?
    .filter(|row| row.reference == *reference)
    .ok_or_else(|| anyhow::anyhow!("schedule occurrence missing"))?;
    let Preparation::HandedOff { input, .. } = &row.preparation else {
        anyhow::bail!("schedule occurrence was not handed off");
    };
    anyhow::ensure!(
        input.session_db_id == session.database().root_id().to_string(),
        "schedule settlement belongs to another Session"
    );
    let Some(evidence) = session.schedule_delivery_evidence(reference).await? else {
        return Ok(None);
    };
    let after = capacity(&row, &evidence)?;
    let store = txn
        .get_store::<Table<ScheduleDeliveryEvidence>>(SETTLEMENTS)
        .await?;
    let prior = optional(&store, row.storage_key()).await?;
    if !retry::is_latest(&txn, agent, &row).await? {
        // Superseded evidence never changes the active delivery's reservation.
        anyhow::ensure!(
            prior.as_ref() == Some(&evidence),
            "superseded schedule evidence changed"
        );
        return Ok(prior);
    }
    let before = match prior {
        Some(prior) => {
            anyhow::ensure!(
                advances(&prior, &evidence),
                "schedule settlement regressed or conflicted"
            );
            if prior == evidence {
                return Ok(Some(prior));
            }
            capacity(&row, &prior)?
        }
        None => Capacity::Held,
    };
    anyhow::ensure!(
        before == Capacity::Held || before == after,
        "settled schedule capacity cannot be refunded or reacquired implicitly"
    );
    match (before, after) {
        (Capacity::Held, Capacity::Successful) => {
            state.reservations = state
                .reservations
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("schedule reservation missing"))?;
            state.successful_fires = state
                .successful_fires
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("schedule success count overflow"))?;
        }
        (Capacity::Held, Capacity::Released) => {
            state.reservations = state
                .reservations
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("schedule reservation missing"))?;
            state.fire_count = state
                .fire_count
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("schedule capacity missing"))?;
        }
        _ => {}
    }
    state.validate()?;
    store.set(row.storage_key(), evidence.clone()).await?;
    txn.get_store::<Table<AccountingState>>(STATE)
        .await?
        .set(&reference.lifetime, state.clone())
        .await?;
    cache.commit(txn, &reference.lifetime, state).await?;
    if retry::eligible(&row, &evidence)? {
        tracing::info!(
            agent_db_id = %reference.owner_agent_db_id,
            schedule_id = %reference.schedule_id,
            occurrence_id = %reference.occurrence_id,
            delivery_id = %reference.delivery_id,
            attempt_id = %evidence.input.attempt_id,
            session_db_id = %input.session_db_id,
            "Scheduled delivery did not succeed; explicit schedule retry available if still authorized"
        );
    }
    Ok(Some(evidence))
}
