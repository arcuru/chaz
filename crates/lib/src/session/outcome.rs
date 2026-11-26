//! Prompt-free terminal evidence committed by the ordinary turn completion path.
//! A Completed attempt alone predates this distinction and is not success proof.
use super::*;

const OUTCOMES: &str = "turn_outcomes";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum TurnOutcome {
    Success,
    Failure,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCompletion {
    pub version: u32,
    pub request_id: TurnRequestId,
    pub attempt_id: String,
    pub completed_at: DateTime<Utc>,
    pub outcome: TurnOutcome,
}

/// Called in the same transaction as the final entry/transcript and Completed
/// attempt. No reply/error text is copied into this operational observation.
pub(super) async fn write_in_txn(
    txn: &eidetica::Transaction,
    attempt: &TurnAttempt,
    final_entry: Option<&SessionEntry>,
    transcript: Option<&TurnTranscriptRecord>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        attempt.status == TurnAttemptStatus::Completed
            && !attempt.attempt_id.is_empty()
            && !attempt.request_id.as_str().is_empty(),
        "turn completion identity missing"
    );
    if let Some(record) = transcript {
        anyhow::ensure!(
            record.request_id == attempt.request_id && record.attempt_id == attempt.attempt_id,
            "terminal transcript belongs to another attempt"
        );
    }
    let cancelled =
        transcript.is_some_and(|record| record.message == TurnTranscriptMessage::Cancelled);
    let outcome = match final_entry {
        Some(entry) if entry.entry_type == EntryType::Error => {
            if cancelled {
                TurnOutcome::Cancelled
            } else {
                TurnOutcome::Failure
            }
        }
        Some(entry) if entry.entry_type == EntryType::Message && !cancelled => TurnOutcome::Success,
        None if cancelled => TurnOutcome::Cancelled,
        None => TurnOutcome::Success,
        Some(_) => anyhow::bail!("turn completion requires a consistent terminal entry"),
    };
    let rows = txn.get_store::<Table<TurnCompletion>>(OUTCOMES).await?;
    match rows.get(&attempt.attempt_id).await {
        Ok(_) => {
            anyhow::bail!("turn outcome already recorded; reconcile instead of completing again")
        }
        Err(error) if error.is_not_found() => {}
        Err(error) => return Err(error.into()),
    }
    let completion = TurnCompletion {
        version: 1,
        request_id: attempt.request_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        completed_at: attempt
            .completed_at
            .ok_or_else(|| anyhow::anyhow!("turn completion time missing"))?,
        outcome,
    };
    rows.set(&attempt.attempt_id, completion).await?;
    Ok(())
}

/// Read against the same Verified snapshot as the input receipt. Missing legacy
/// evidence remains unknown; an orphan/conflicting row is an error, not success.
pub(super) async fn read_in_txn(
    txn: &eidetica::Transaction,
    attempt_id: &str,
) -> anyhow::Result<Option<TurnCompletion>> {
    let rows = txn.get_store::<Table<TurnCompletion>>(OUTCOMES).await?;
    let completion = match rows.get(attempt_id).await {
        Ok(row) => row,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let attempt = txn
        .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
        .await?
        .get(attempt_id)
        .await?;
    anyhow::ensure!(
        completion.version == 1
            && completion.attempt_id == attempt_id
            && completion.request_id == attempt.request_id
            && attempt.attempt_id == attempt_id
            && attempt.status == TurnAttemptStatus::Completed
            && attempt.completed_at == Some(completion.completed_at),
        "turn completion evidence differs or has an unsupported version"
    );
    Ok(Some(completion))
}

impl Session {
    pub async fn turn_completion(
        &self,
        attempt_id: &str,
    ) -> anyhow::Result<Option<TurnCompletion>> {
        read_in_txn(&self.database.new_transaction().await?, attempt_id).await
    }
}
