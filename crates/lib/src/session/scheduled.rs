//! Inert, idempotent scheduled-input preparation. An Ack is a real request
//! identity, not a runnable message or a Started attempt. The selected Agent
//! ledger must acknowledge handoff before a targeted adapter may use it.
use super::*;
use crate::schedule_state::readers::ScheduleReference;

const INPUTS: &str = "scheduled_inputs";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleInputAnchor {
    pub version: u32,
    pub reference: ScheduleReference,
    pub session_db_id: String,
    pub request_id: TurnRequestId,
    pub prepared_at: DateTime<Utc>,
}

fn request_id(reference: &ScheduleReference) -> TurnRequestId {
    TurnRequestId::parse(format!(
        "scheduled:{}",
        blake3::hash(reference.delivery_id.as_bytes())
    ))
}

impl Session {
    /// Caller serializes publication of a delivery. A lost commit reply must be
    /// reconciled using this same delivery ID, never by allocating another one.
    /// This writes references only and grants no target or execution authority.
    pub async fn prepare_schedule_input(
        &mut self,
        reference: &ScheduleReference,
    ) -> anyhow::Result<ScheduleInputAnchor> {
        reference.validate()?;
        jobs::require_observer_write(&self.database).await?;
        let txn = Self::legacy_history_transaction(&self.database).await?;
        if let Some(anchor) = self.schedule_input_in_txn(&txn, reference).await? {
            return Ok(anchor);
        }
        let request_id = request_id(reference);
        let entries = txn
            .get_store::<Table<SessionEntry>>(&self.store_name)
            .await?;
        match entries.get(request_id.as_str()).await {
            Ok(_) => anyhow::bail!("scheduled Ack exists without its reference; outcome held"),
            Err(error) if error.is_not_found() => {}
            Err(error) => return Err(error.into()),
        }
        let entry = SessionEntry {
            sender: "system".into(),
            content: serde_json::to_string(reference)?,
            timestamp: Utc::now(),
            entry_type: EntryType::Ack,
            metadata: None,
            routing: None,
        };
        entries.set(request_id.as_str(), entry.clone()).await?;
        let anchor = ScheduleInputAnchor {
            version: 1,
            reference: reference.clone(),
            session_db_id: self.database.root_id().to_string(),
            request_id,
            prepared_at: entry.timestamp,
        };
        txn.get_store::<Table<ScheduleInputAnchor>>(INPUTS)
            .await?
            .set(&reference.delivery_id, anchor.clone())
            .await?;
        txn.commit().await?;
        self.entries.push(entry);
        self.entry_ids.push(anchor.request_id.clone());
        Ok(anchor)
    }

    /// Validate both sides of the inert Session-local transaction in one
    /// snapshot. A sidecar alone, unsupported version or missing/changed Ack
    /// is not a valid input. Read failure is never permission to recreate it.
    pub async fn schedule_input(
        &self,
        reference: &ScheduleReference,
    ) -> anyhow::Result<Option<ScheduleInputAnchor>> {
        reference.validate()?;
        let txn = self.database.new_transaction().await?;
        self.schedule_input_in_txn(&txn, reference).await
    }

    async fn schedule_input_in_txn(
        &self,
        txn: &eidetica::Transaction,
        reference: &ScheduleReference,
    ) -> anyhow::Result<Option<ScheduleInputAnchor>> {
        reference.validate()?;
        let rows = txn.get_store::<Table<ScheduleInputAnchor>>(INPUTS).await?;
        let anchor = match rows.get(&reference.delivery_id).await {
            Ok(anchor) => anchor,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            anchor.version == 1
                && anchor.reference == *reference
                && anchor.request_id == request_id(reference)
                && anchor.session_db_id == self.database.root_id().to_string(),
            "scheduled input reference conflicts or has an unsupported version"
        );
        let entry = txn
            .get_store::<Table<SessionEntry>>(&self.store_name)
            .await?
            .get(anchor.request_id.as_str())
            .await?;
        anyhow::ensure!(
            entry.entry_type == EntryType::Ack
                && entry.sender == "system"
                && entry.timestamp == anchor.prepared_at
                && entry.metadata.is_none()
                && entry.routing.is_none()
                && serde_json::from_str::<ScheduleReference>(&entry.content)? == *reference,
            "scheduled input anchor differs"
        );
        Ok(Some(anchor))
    }
}

/// Receipt for private input in an actual ordinary turn. It contains no wake
/// text. Binding/receipt writes do not authorize execution: the common runner
/// must validate the committed ledger handoff and exact Agent at both guards.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleInputReceipt {
    pub version: u32,
    pub anchor: ScheduleInputAnchor,
    pub request_id: TurnRequestId,
    pub attempt_id: String,
    pub state: steering::JobInputState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) executor: Option<ScheduleExecutor>,
}

/// Written with binding only for opted-in deliveries. The home key is not a
/// lock: recovery assumes a single executor for the Session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScheduleExecutor {
    pub(crate) home_key: String,
    pub(crate) incarnation: String,
}

/// One coherent Session snapshot, suitable for an operational projection. A
/// missing completion is unknown, not a successful or safely unused attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDeliveryEvidence {
    pub version: u32,
    pub input: ScheduleInputReceipt,
    pub completion: Option<outcome::TurnCompletion>,
}

const RECEIPTS: &str = "scheduled_input_receipts";

impl Session {
    /// Awaited cleanup also applies when common context preparation fails
    /// before the runtime recorder is constructed. Unknown dispatch stays held.
    pub(crate) async fn finish_scheduled_inputs(&self, attempt_id: &str) -> anyhow::Result<()> {
        use steering::JobInputState::*;
        for input in self.scheduled_inputs_for_attempt(attempt_id).await? {
            let state = match input.state {
                Queued | Accepted => NotApplied {
                    reason: "containing turn ended before scheduled dispatch".into(),
                },
                Dispatching { .. } => Uncertain {
                    reason: "containing turn ended without a scheduled response receipt".into(),
                },
                _ => continue,
            };
            self.record_schedule_input(&input.anchor.reference, attempt_id, state)
                .await?;
        }
        Ok(())
    }

    /// Start the initial targeted idle/Fresh invocation and bind its private
    /// reference in one commit. The caller owns capacity and session-local
    /// serialization, and has checked the ledger handoff/current Agent. A lost
    /// reply is reconciled by this delivery, never another Started attempt.
    pub(crate) async fn start_schedule_input_as(
        &self,
        reference: &ScheduleReference,
        attempt_id: String,
        executor: Option<ScheduleExecutor>,
    ) -> anyhow::Result<Option<TurnAttempt>> {
        jobs::require_observer_write(&self.database).await?;
        let txn = Self::legacy_history_transaction(&self.database).await?;
        let anchor = self
            .schedule_input_in_txn(&txn, reference)
            .await?
            .ok_or_else(|| anyhow::anyhow!("scheduled input anchor missing"))?;
        let receipts = txn
            .get_store::<Table<ScheduleInputReceipt>>(RECEIPTS)
            .await?;
        match receipts.get(&reference.delivery_id).await {
            Ok(prior) => {
                self.validate_schedule_receipt(&txn, reference, &prior)
                    .await?;
                return Ok(None);
            }
            Err(error) if error.is_not_found() => {}
            Err(error) => return Err(error.into()),
        }
        let attempts = txn
            .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
            .await?;
        anyhow::ensure!(
            attempts
                .search(|attempt| attempt.request_id == anchor.request_id)
                .await?
                .is_empty(),
            "scheduled request already attempted without its receipt; held"
        );
        anyhow::ensure!(
            matches!(attempts.get(&attempt_id).await, Err(error) if error.is_not_found()),
            "scheduled attempt identity unavailable or already used"
        );
        let attempt = TurnAttempt {
            attempt_id,
            request_id: anchor.request_id.clone(),
            started_at: Utc::now(),
            generation: 1,
            status: TurnAttemptStatus::Started,
            completed_at: None,
        };
        attempts.set(&attempt.attempt_id, attempt.clone()).await?;
        receipts
            .set(
                &reference.delivery_id,
                ScheduleInputReceipt {
                    version: 1,
                    anchor,
                    request_id: attempt.request_id.clone(),
                    attempt_id: attempt.attempt_id.clone(),
                    state: steering::JobInputState::Queued,
                    executor,
                },
            )
            .await?;
        txn.commit().await?;
        Ok(Some(attempt))
    }

    /// Bind an already-started attempt only once. Targeted idle/Fresh attempts
    /// use the Ack request; busy Pinned input names its containing human turn.
    /// The timer never starts an attempt or calls this while awaiting capacity.
    pub async fn bind_schedule_input(
        &self,
        reference: &ScheduleReference,
        attempt_id: &str,
    ) -> anyhow::Result<ScheduleInputReceipt> {
        self.bind_schedule_input_owned(reference, attempt_id, None)
            .await
    }

    pub(crate) async fn bind_schedule_input_owned(
        &self,
        reference: &ScheduleReference,
        attempt_id: &str,
        executor: Option<ScheduleExecutor>,
    ) -> anyhow::Result<ScheduleInputReceipt> {
        jobs::require_observer_write(&self.database).await?;
        let txn = Self::legacy_history_transaction(&self.database).await?;
        let anchor = self
            .schedule_input_in_txn(&txn, reference)
            .await?
            .ok_or_else(|| anyhow::anyhow!("scheduled input anchor missing"))?;
        let rows = txn
            .get_store::<Table<ScheduleInputReceipt>>(RECEIPTS)
            .await?;
        match rows.get(&reference.delivery_id).await {
            Ok(prior) => {
                self.validate_schedule_receipt(&txn, reference, &prior)
                    .await?;
                anyhow::ensure!(
                    prior.attempt_id == attempt_id,
                    "scheduled input already bound to another attempt"
                );
                return Ok(prior);
            }
            Err(error) if error.is_not_found() => {}
            Err(error) => return Err(error.into()),
        }
        let attempt = txn
            .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
            .await?
            .get(attempt_id)
            .await?;
        anyhow::ensure!(
            attempt.attempt_id == attempt_id && attempt.status == TurnAttemptStatus::Started,
            "scheduled input needs an actual started attempt"
        );
        anyhow::ensure!(
            Session::steering_attempts(&self.database)
                .await?
                .contains(&attempt.attempt_id)
                && !steering::input_is_closed(&txn, attempt_id).await?,
            "scheduled input acceptance is closed or attempt superseded"
        );
        let receipt = ScheduleInputReceipt {
            version: 1,
            anchor,
            request_id: attempt.request_id,
            attempt_id: attempt_id.into(),
            state: steering::JobInputState::Queued,
            executor,
        };
        rows.set(&reference.delivery_id, receipt.clone()).await?;
        txn.commit().await?;
        Ok(receipt)
    }

    async fn validate_schedule_receipt(
        &self,
        txn: &eidetica::Transaction,
        reference: &ScheduleReference,
        receipt: &ScheduleInputReceipt,
    ) -> anyhow::Result<TurnAttempt> {
        let anchor = self
            .schedule_input_in_txn(txn, reference)
            .await?
            .ok_or_else(|| anyhow::anyhow!("scheduled input anchor missing"))?;
        let attempt = txn
            .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
            .await?
            .get(&receipt.attempt_id)
            .await?;
        anyhow::ensure!(
            receipt.version == 1
                && receipt.anchor == anchor
                && receipt.attempt_id == attempt.attempt_id
                && receipt.request_id == attempt.request_id,
            "scheduled input receipt identity differs or has an unsupported version"
        );
        if let Some(executor) = &receipt.executor {
            anyhow::ensure!(
                !executor.home_key.is_empty()
                    && uuid::Uuid::parse_str(&executor.incarnation).is_ok(),
                "scheduled receipt executor identity differs"
            );
        }
        Ok(attempt)
    }

    /// Awaited recorder boundary only. Definite non-application can be recorded
    /// before dispatch; an uncertain dispatch cannot later be called unused.
    /// Repeated observations are idempotent; terminal evidence is monotonic.
    pub async fn record_schedule_input(
        &self,
        reference: &ScheduleReference,
        attempt_id: &str,
        state: steering::JobInputState,
    ) -> anyhow::Result<()> {
        use steering::JobInputState::*;
        jobs::require_observer_write(&self.database).await?;
        let txn = Self::legacy_history_transaction(&self.database).await?;
        let rows = txn
            .get_store::<Table<ScheduleInputReceipt>>(RECEIPTS)
            .await?;
        let mut receipt = rows.get(&reference.delivery_id).await?;
        let attempt = self
            .validate_schedule_receipt(&txn, reference, &receipt)
            .await?;
        anyhow::ensure!(
            receipt.attempt_id == attempt_id,
            "scheduled input attempt differs"
        );
        if receipt.state == state {
            return Ok(());
        }
        if matches!(state, Accepted | Dispatching { .. } | Included { .. }) {
            anyhow::ensure!(
                attempt.status == TurnAttemptStatus::Started,
                "completed attempt cannot receive scheduled input"
            );
        }
        let valid = match (&receipt.state, &state) {
            (Queued, Accepted) | (Accepted, Dispatching { .. }) => true,
            (
                Dispatching {
                    model_sequence: before,
                },
                Included {
                    model_sequence: after,
                },
            ) => before == after,
            (Queued | Accepted, NotApplied { .. }) => true,
            (Queued | Accepted | Dispatching { .. }, Uncertain { .. }) => true,
            _ => false,
        };
        anyhow::ensure!(
            valid,
            "scheduled input observation would regress or conflict"
        );
        receipt.state = state;
        rows.set(&reference.delivery_id, receipt).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Exact-attempt references only, not a generic Ack/history replay source.
    /// Validate every selected sidecar and its real request in one snapshot.
    pub(crate) async fn scheduled_inputs_for_attempt(
        &self,
        attempt_id: &str,
    ) -> anyhow::Result<Vec<ScheduleInputReceipt>> {
        let txn = self.database.new_transaction().await?;
        let rows = txn
            .get_store::<Table<ScheduleInputReceipt>>(RECEIPTS)
            .await?
            .search(|receipt| receipt.attempt_id == attempt_id)
            .await?;
        let mut inputs = Vec::with_capacity(rows.len());
        for (key, receipt) in rows {
            anyhow::ensure!(
                key == receipt.anchor.reference.delivery_id,
                "scheduled receipt key differs"
            );
            self.validate_schedule_receipt(&txn, &receipt.anchor.reference, &receipt)
                .await?;
            inputs.push(receipt);
        }
        inputs.sort_by(|a, b| {
            a.anchor
                .reference
                .delivery_id
                .cmp(&b.anchor.reference.delivery_id)
        });
        Ok(inputs)
    }

    pub(crate) async fn dispatch_scheduled_inputs_in_txn(
        &self,
        txn: &eidetica::Transaction,
        attempt_id: &str,
        model_sequence: u64,
        references: &[ScheduleReference],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!references.is_empty(), "empty scheduled boundary");
        let rows = txn
            .get_store::<Table<ScheduleInputReceipt>>(RECEIPTS)
            .await?;
        let mut seen = std::collections::HashSet::new();
        for reference in references {
            anyhow::ensure!(
                seen.insert(&reference.delivery_id),
                "duplicate scheduled boundary"
            );
            let mut receipt = rows.get(&reference.delivery_id).await?;
            let attempt = self
                .validate_schedule_receipt(txn, reference, &receipt)
                .await?;
            anyhow::ensure!(
                receipt.attempt_id == attempt_id
                    && attempt.status == TurnAttemptStatus::Started
                    && receipt.state == steering::JobInputState::Accepted,
                "scheduled boundary has no accepted input in this attempt"
            );
            receipt.state = steering::JobInputState::Dispatching { model_sequence };
            rows.set(&reference.delivery_id, receipt).await?;
        }
        Ok(())
    }

    /// Read binding, receipt and terminal result at the same Verified snapshot.
    /// No input receipt means unknown delivery, not permission to refund/replay.
    pub async fn schedule_delivery_evidence(
        &self,
        reference: &ScheduleReference,
    ) -> anyhow::Result<Option<ScheduleDeliveryEvidence>> {
        reference.validate()?;
        let txn = self.database.new_transaction().await?;
        self.schedule_delivery_evidence_in(&txn, reference).await
    }

    async fn schedule_delivery_evidence_in(
        &self,
        txn: &eidetica::Transaction,
        reference: &ScheduleReference,
    ) -> anyhow::Result<Option<ScheduleDeliveryEvidence>> {
        let rows = txn
            .get_store::<Table<ScheduleInputReceipt>>(RECEIPTS)
            .await?;
        let input = match rows.get(&reference.delivery_id).await {
            Ok(input) => input,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        self.validate_schedule_receipt(txn, reference, &input)
            .await?;
        let completion = outcome::read_in_txn(txn, &input.attempt_id).await?;
        Ok(Some(ScheduleDeliveryEvidence {
            version: 1,
            input,
            completion,
        }))
    }

    /// Positive evidence of an unbound handoff: its exact inert Ack remains
    /// present and no receipt or Started attempt exists in the same snapshot.
    pub(crate) async fn schedule_input_unused(
        &self,
        expected: &ScheduleInputAnchor,
    ) -> anyhow::Result<bool> {
        let txn = self.database.new_transaction().await?;
        let reference = &expected.reference;
        anyhow::ensure!(
            self.schedule_input_in_txn(&txn, reference).await?.as_ref() == Some(expected),
            "unused schedule input anchor differs"
        );
        if self
            .schedule_delivery_evidence_in(&txn, reference)
            .await?
            .is_some()
        {
            return Ok(false);
        }
        anyhow::ensure!(
            txn.get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
                .await?
                .search(|attempt| attempt.request_id == expected.request_id)
                .await?
                .is_empty(),
            "scheduled input attempt has no binding evidence; held"
        );
        Ok(true)
    }

    /// Reuse ordinary request generation ordering, without making an Ack a
    /// generic /retry target. The caller separately checks runtime liveness.
    pub(crate) async fn schedule_interruption(
        &self,
        expected: &ScheduleDeliveryEvidence,
    ) -> anyhow::Result<bool> {
        let snapshot = self.database.snapshot().await?;
        let txn = self.database.new_transaction_at(&snapshot).await?;
        let reference = &expected.input.anchor.reference;
        let Some(evidence) = self.schedule_delivery_evidence_in(&txn, reference).await? else {
            return Ok(false);
        };
        if &evidence != expected || evidence.completion.is_some() {
            return Ok(false);
        }
        let input = evidence.input;
        let state = index_attempt_states(
            self.read_turn_attempts_at(&snapshot).await?,
            &Default::default(),
        )
        .remove(&input.request_id);
        if !matches!(state, Some(TurnRequestState::Interrupted { attempt_id }) if attempt_id == input.attempt_id)
        {
            return Ok(false);
        }
        let stops = txn
            .get_store::<Table<SessionEntry>>(&self.store_name)
            .await?
            .search(|entry| entry.entry_type == EntryType::Cancel)
            .await?;
        if stops.iter().any(|(_, entry)| {
            serde_json::from_str::<CancelRequest>(&entry.content).is_ok_and(|stop| {
                stop.request_id == input.request_id && stop.attempt_id == input.attempt_id
            })
        }) {
            return Ok(false);
        }
        // A checkpoint belongs to same-attempt wait recovery, not to an
        // interruption retry, even when the checkpoint cannot be resumed.
        Ok(
            Self::latest_job_wait_checkpoint_in_txn(&txn, &input.attempt_id)
                .await?
                .is_none(),
        )
    }
}

pub(crate) fn validate_references(
    references: &[ScheduleReference],
    seen: &mut std::collections::HashSet<String>,
) -> bool {
    !references.is_empty()
        && references.iter().all(|reference| {
            reference.validate().is_ok() && seen.insert(reference.delivery_id.clone())
        })
}

#[cfg(test)]
mod tests;
