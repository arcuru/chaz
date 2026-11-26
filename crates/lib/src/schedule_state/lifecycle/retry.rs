//! An explicitly retried failure gets a new delivery in the same occurrence
//! and Session. The prior receipt, handoff and clock remain unchanged.
use super::*;
use crate::session::{
    outcome::TurnOutcome,
    scheduled::{ScheduleDeliveryEvidence, ScheduleExecutor},
    steering::JobInputState,
};
use std::num::NonZeroU32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retry {
    ordinal: NonZeroU32,
    prior_attempt: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    interruption: bool,
}

impl Retry {
    pub(super) fn is_valid(&self, reference: &ScheduleReference) -> bool {
        !self.prior_attempt.is_empty()
            && reference.delivery_id == retry_id(&reference.occurrence_id, self.ordinal)
    }
}

// The command's idempotency key is the exact predecessor, not a random ID
// allocated by the caller. One predecessor can authorize at most one retry.
fn retry_id(occurrence_id: &str, ordinal: NonZeroU32) -> String {
    format!("retry:{occurrence_id}:{ordinal}")
}

fn ordinal(row: &Occurrence) -> u32 {
    row.retry.as_ref().map_or(0, |retry| retry.ordinal.get())
}

fn head(root: &Occurrence) -> u32 {
    root.latest_retry.map_or(0, NonZeroU32::get)
}

async fn root_for(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    occurrence_id: &str,
) -> anyhow::Result<Occurrence> {
    read_occurrence(txn, agent, occurrence_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("original schedule occurrence missing"))
}

fn check_retry(root: &Occurrence, row: &Occurrence) -> anyhow::Result<()> {
    let Some(retry) = &row.retry else {
        anyhow::bail!("schedule retry row missing its allocation");
    };
    let mut reference = root.reference.clone();
    reference.delivery_id = retry_id(&root.reference.occurrence_id, retry.ordinal);
    let (
        Preparation::HandedOff {
            input: original, ..
        },
        Preparation::HandedOff { input, .. },
    ) = (&root.preparation, &row.preparation)
    else {
        anyhow::bail!("schedule retry lacks an exact handoff");
    };
    anyhow::ensure!(
        root.publication_sequence < row.publication_sequence
            && root.publication_sequence.is_some()
            && ordinal(row) <= head(root)
            && row.reference == reference
            && row.slot == root.slot
            && row.target == root.target
            && row.fresh_root == root.fresh_root
            && input.session_db_id == original.session_db_id,
        "schedule retry changed its occurrence or bound Session"
    );
    Ok(())
}

pub(super) async fn read_delivery(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    occurrence_id: &str,
    delivery_id: &str,
) -> anyhow::Result<Option<Occurrence>> {
    let Some(root) = read_occurrence(txn, agent, occurrence_id).await? else {
        return Ok(None);
    };
    if root.reference.delivery_id == delivery_id {
        return Ok(Some(root));
    }
    let row = read_record(txn, agent, delivery_id).await?;
    if let Some(row) = &row {
        check_retry(&root, row)?;
    }
    Ok(row)
}

pub(super) async fn is_latest(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    row: &Occurrence,
) -> anyhow::Result<bool> {
    let Some(retry) = &row.retry else {
        return Ok(row.latest_retry.is_none());
    };
    let root = root_for(txn, agent, &row.reference.occurrence_id).await?;
    Ok(Some(retry.ordinal) == root.latest_retry)
}

pub(super) fn eligible(
    prior: &Occurrence,
    evidence: &ScheduleDeliveryEvidence,
) -> anyhow::Result<bool> {
    let capacity = settlement::capacity(prior, evidence)?;
    let Some(completion) = &evidence.completion else {
        return Ok(false);
    };
    Ok(capacity != settlement::Capacity::Successful
        && match &prior.target {
            ScheduleTarget::Fresh => completion.outcome == TurnOutcome::Failure,
            ScheduleTarget::Pinned { .. } => match &evidence.input.state {
                JobInputState::NotApplied { .. } => completion.outcome != TurnOutcome::Cancelled,
                JobInputState::Uncertain { .. } => completion.outcome == TurnOutcome::Failure,
                _ => false,
            },
        })
}

fn interrupted(prior: &Occurrence, evidence: &ScheduleDeliveryEvidence) -> anyhow::Result<bool> {
    Ok(
        settlement::capacity(prior, evidence)? == settlement::Capacity::Held
            && evidence.completion.is_none()
            && evidence.input.executor.is_some()
            && matches!(
                evidence.input.state,
                JobInputState::Queued
                    | JobInputState::Accepted
                    | JobInputState::Dispatching { .. }
                    | JobInputState::Included { .. }
            ),
    )
}

async fn check_interruption(
    session: &Session,
    evidence: &ScheduleDeliveryEvidence,
    executor: &ScheduleExecutor,
) -> anyhow::Result<bool> {
    if !evidence.input.executor.as_ref().is_some_and(|prior| {
        prior.home_key == executor.home_key && prior.incarnation != executor.incarnation
    }) {
        return Ok(false);
    }
    session.schedule_interruption(evidence).await
}

/// Point-check the head and each predecessor during paged reconciliation. A
/// gap or unlinked retry cannot quietly change the logical fire count.
pub(super) async fn validate_lineage(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    row: &Occurrence,
) -> anyhow::Result<()> {
    let Some(retry) = &row.retry else {
        if let Some(last) = row.latest_retry {
            let latest = read_record(txn, agent, &retry_id(&row.reference.occurrence_id, last))
                .await?
                .ok_or_else(|| anyhow::anyhow!("schedule retry head missing"))?;
            check_retry(row, &latest)?;
        }
        return Ok(());
    };
    predecessor(txn, agent, row, retry).await?;
    Ok(())
}

async fn predecessor(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    row: &Occurrence,
    retry: &Retry,
) -> anyhow::Result<ScheduleDeliveryEvidence> {
    let root = root_for(txn, agent, &row.reference.occurrence_id).await?;
    check_retry(&root, row)?;
    let prior = match NonZeroU32::new(retry.ordinal.get() - 1) {
        Some(previous) => {
            let prior = read_record(
                txn,
                agent,
                &retry_id(&row.reference.occurrence_id, previous),
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("previous schedule retry missing"))?;
            check_retry(&root, &prior)?;
            prior
        }
        None => root,
    };
    let evidence = txn
        .get_store::<Table<ScheduleDeliveryEvidence>>(settlement::SETTLEMENTS)
        .await?
        .get(prior.storage_key())
        .await?;
    anyhow::ensure!(
        prior.publication_sequence < row.publication_sequence
            && evidence.input.attempt_id == retry.prior_attempt
            && if retry.interruption {
                interrupted(&prior, &evidence)?
                    && archived_definition(agent, &row.reference)
                        .await?
                        .interruption_retries
                        .is_some_and(|limit| retry.ordinal <= limit)
            } else {
                eligible(&prior, &evidence)?
            },
        "schedule retry predecessor evidence differs"
    );
    Ok(evidence)
}

pub(crate) async fn is_current(agent: &AgentDb, row: &Occurrence) -> anyhow::Result<bool> {
    let db = agent
        .scheduling_database()
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule ledger unavailable"))?;
    let txn = db.new_transaction().await?;
    let root = root_for(&txn, agent, &row.reference.occurrence_id).await?;
    Ok(ordinal(row) == head(&root))
}

/// Allocation never grants permission to ignore a stop or a successor of the
/// interrupted attempt observed after allocation.
pub(crate) async fn verify_interrupted_predecessor(
    agent: &AgentDb,
    reference: &ScheduleReference,
    session: &Session,
    executor: &ScheduleExecutor,
) -> anyhow::Result<Option<String>> {
    if !reference.delivery_id.starts_with("retry:") {
        return Ok(None);
    }
    let db = agent
        .scheduling_database()
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule ledger unavailable"))?;
    let txn = db.new_transaction().await?;
    let row = read_delivery(
        &txn,
        agent,
        &reference.occurrence_id,
        &reference.delivery_id,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("retry delivery missing"))?;
    let Some(retry) = row.retry.as_ref().filter(|retry| retry.interruption) else {
        return Ok(None);
    };
    let evidence = predecessor(&txn, agent, &row, retry).await?;
    anyhow::ensure!(
        check_interruption(session, &evidence, executor).await?,
        "prior schedule attempt stopped, changed or is not interrupted"
    );
    Ok(Some(retry.prior_attempt.clone()))
}

pub(crate) async fn delivery(
    agent: &AgentDb,
    occurrence_id: &str,
    delivery_id: &str,
) -> anyhow::Result<Occurrence> {
    let db = agent
        .scheduling_database()
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule ledger unavailable"))?;
    read_delivery(
        &db.new_transaction().await?,
        agent,
        occurrence_id,
        delivery_id,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("schedule delivery missing"))
}

/// Caller verifies home, current keys, membership and the selected Session.
/// The shared scheduling lock serializes operator edits and concurrent retry
/// requests. Only the final ledger commit makes the new inert Ack runnable.
pub(crate) async fn explicit(
    agent: &AgentDb,
    prior: &ScheduleReference,
    expected_attempt: &str,
    session: &mut Session,
    now: DateTime<Utc>,
) -> anyhow::Result<ScheduleReference> {
    allocate(agent, prior, expected_attempt, session, now, None).await
}

pub(crate) async fn recover(
    agent: &AgentDb,
    prior: &Occurrence,
    evidence: &ScheduleDeliveryEvidence,
    session: &mut Session,
    executor: &ScheduleExecutor,
) -> anyhow::Result<Option<ScheduleReference>> {
    if !interrupted(prior, evidence)? || !is_current(agent, prior).await? {
        return Ok(None);
    }
    let schedule = agent
        .resolve_schedule_reference(&prior.reference, Utc::now())
        .await?;
    if !schedule
        .interruption_retries
        .is_some_and(|limit| ordinal(prior) < limit.get())
        || !check_interruption(session, evidence, executor).await?
    {
        return Ok(None);
    }
    allocate(
        agent,
        &prior.reference,
        &evidence.input.attempt_id,
        session,
        Utc::now(),
        Some(executor),
    )
    .await
    .map(Some)
}

async fn allocate(
    agent: &AgentDb,
    prior: &ScheduleReference,
    expected_attempt: &str,
    session: &mut Session,
    now: DateTime<Utc>,
    executor: Option<&ScheduleExecutor>,
) -> anyhow::Result<ScheduleReference> {
    let mut cache = agent.lock_scheduling().await;
    let schedule = agent.resolve_schedule_reference(prior, now).await?;
    let (txn, mut state) = cache.load(agent, &schedule).await?;
    let mut root = root_for(&txn, agent, &prior.occurrence_id).await?;
    let previous = validate_input_handoff(agent, prior, session, &txn, &schedule).await?;
    anyhow::ensure!(
        previous.publication_sequence.is_some(),
        "unindexed schedule history cannot be retried"
    );
    let next = ordinal(&previous)
        .checked_add(1)
        .and_then(NonZeroU32::new)
        .ok_or_else(|| anyhow::anyhow!("schedule retry ordinal exhausted"))?;
    let delivery_id = retry_id(&root.reference.occurrence_id, next);
    if let Some(row) = read_record(&txn, agent, &delivery_id).await? {
        validate_lineage(&txn, agent, &row).await?;
        anyhow::ensure!(
            row.retry
                .is_some_and(|retry| retry.prior_attempt == expected_attempt
                    && retry.interruption == executor.is_some()),
            "schedule retry names another prior attempt or policy"
        );
        return Ok(row.reference);
    }
    anyhow::ensure!(
        ordinal(&previous) == head(&root),
        "schedule retry expects a superseded delivery"
    );
    let evidence = session
        .schedule_delivery_evidence(prior)
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule retry has no Session receipt"))?;
    anyhow::ensure!(
        evidence.input.attempt_id == expected_attempt,
        "schedule retry names another prior attempt"
    );
    if let Some(executor) = executor {
        anyhow::ensure!(
            schedule
                .interruption_retries
                .is_some_and(|limit| next <= limit)
                && interrupted(&previous, &evidence)?
                && check_interruption(session, &evidence, executor).await?,
            "schedule interruption retry is not authorized"
        );
    } else {
        anyhow::ensure!(
            eligible(&previous, &evidence)?,
            "schedule retry needs the exact failed or unused delivery, not an interruption or stop"
        );
    }
    anyhow::ensure!(
        txn.get_store::<Table<ScheduleDeliveryEvidence>>(settlement::SETTLEMENTS)
            .await?
            .get(previous.storage_key())
            .await?
            == evidence,
        "schedule predecessor projection differs from the Session"
    );
    let capacity = settlement::capacity(&previous, &evidence)?;
    // Unknown Pinned delivery with a *known* terminal failure can be retried
    // explicitly, but transfers its held slot instead of refunding it.
    let reacquire = capacity == settlement::Capacity::Released;
    anyhow::ensure!(
        !state.retired
            && schedule.max_fires.is_none_or(|max| {
                state.fire_count < max || (!reacquire && state.fire_count == max)
            }),
        "schedule is retired or has no retry capacity"
    );
    if reacquire {
        state.reservations = state
            .reservations
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("schedule reservation overflow"))?;
        state.fire_count = state
            .fire_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("schedule capacity overflow"))?;
    }
    let mut reference = prior.clone();
    reference.delivery_id = delivery_id;
    let input = session.prepare_schedule_input(&reference).await?;
    // Re-read after the cross-DB write. The scheduling lock covers local
    // revisions; a later authority change still holds at the runner boundary.
    agent
        .resolve_schedule_reference(&reference, Utc::now())
        .await?;
    if let Some(executor) = executor {
        anyhow::ensure!(
            check_interruption(session, &evidence, executor).await?,
            "prior schedule attempt changed during recovery"
        );
    }
    let row = Occurrence {
        version: 5,
        reference: reference.clone(),
        slot: root.slot.clone(),
        target: root.target.clone(),
        prepared_at: input.prepared_at,
        preparation: Preparation::HandedOff {
            input: Box::new(input),
            at: now,
        },
        publication_sequence: Some(order::append(&txn, agent, &reference, false).await?),
        fresh_root: root.fresh_root.clone(),
        clock: None,
        retry: Some(Retry {
            ordinal: next,
            prior_attempt: expected_attempt.into(),
            interruption: executor.is_some(),
        }),
        latest_retry: None,
        unused: false,
    };
    root.latest_retry = Some(next);
    let Preparation::HandedOff { input, .. } = &row.preparation else {
        unreachable!()
    };
    row.validate(agent)?;
    check_retry(&root, &row)?;
    write_fire(
        &txn,
        input,
        matches!(row.target, ScheduleTarget::Fresh),
        now,
    )
    .await?;
    txn.get_store::<Table<Occurrence>>(OCCURRENCES)
        .await?
        .set(root.storage_key(), root.clone())
        .await?;
    state.handoff_count = state
        .handoff_count
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("schedule handoff overflow"))?;
    write(&mut cache, txn, &row, state).await?;
    Ok(reference)
}
