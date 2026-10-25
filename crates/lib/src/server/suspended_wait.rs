//! In-process exact-wait suspension. Only the descriptor and the session's
//! processing reservation survive; persisted records decide readiness/resume.
use super::*;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

#[derive(Clone, Debug)]
pub(super) struct SuspendedWait {
    pub attempt_id: String,
    pub child: String,
    pub deadline: chrono::DateTime<Utc>,
}

impl Server {
    pub(super) fn suspend_job_wait<'a>(
        self: &'a Arc<Self>,
        sid: String,
        wait: SuspendedWait,
        cancel: runtime::TurnCancel,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.suspended_waits
                .lock()
                .await
                .insert(sid.clone(), wait.clone());
            let server = Arc::downgrade(self);
            self.track_task(tokio::spawn(async move {
                let Some(owner) = server.upgrade() else {
                    return;
                };
                let result = AssertUnwindSafe(owner.observe_suspended_wait(&sid, &wait, &cancel))
                    .catch_unwind().await;
                match result {
                    Ok(Ok(true)) => {} // the readmitted turn now owns the reservation
                    Ok(Ok(false)) => owner.release_suspended_wait(&sid, &wait).await,
                    Ok(Err(error)) => {
                        warn!(%error, session_db_id = %sid, attempt_id = %wait.attempt_id,
                        "Suspended wait could not advance; holding for explicit retry/restart");
                        owner.release_suspended_wait(&sid, &wait).await;
                    }
                    Err(_) => {
                        error!(session_db_id = %sid, "Suspended wait observer panicked; holding attempt");
                        owner.release_suspended_wait(&sid, &wait).await;
                    }
                }
            }));
        })
    }

    async fn release_suspended_wait(&self, sid: &str, wait: &SuspendedWait) {
        self.suspended_waits.lock().await.remove(sid);
        self.live_attempts.lock().await.remove(&wait.attempt_id);
        self.turns.lock().await.remove(sid);
        self.processing.lock().await.remove(sid);
        let _ = self
            .notify_tx
            .send(ProcessingCommand::Wake(sid.to_string()))
            .await;
    }

    async fn observe_suspended_wait(
        self: &Arc<Self>,
        sid: &str,
        wait: &SuspendedWait,
        cancel: &runtime::TurnCancel,
    ) -> anyhow::Result<bool> {
        let notify = Arc::new(tokio::sync::Notify::new());
        // Hints are optional. Register before the first durable status read,
        // and periodically reconcile even if callbacks fail or are lost.
        let child = match self.registry.open_session(&wait.child).await {
            Ok(opened) => Ok(opened),
            Err(_) => self.registry.open_job_session(&wait.child).await,
        };
        #[cfg(test)]
        let hints = self.suspended_wait_hints.load(Ordering::SeqCst);
        #[cfg(not(test))]
        let hints = true;
        let _callback = match child {
            Ok((_, db)) if hints => {
                let on_write = notify.clone();
                match db
                    .on_write_at_tips(db.snapshot().await?, move |_, _| {
                        let notify = on_write.clone();
                        async move {
                            notify.notify_one();
                            Ok(())
                        }
                    })
                    .await
                {
                    Ok(callback) => Some(callback),
                    Err(error) => {
                        warn!(%error, "Suspended wait hints unavailable; using reconciliation");
                        None
                    }
                }
            }
            _ => None, // periodic reconciliation also covers missing hints
        };
        loop {
            // These are short-lived views, never retained across the wait.
            let (_, db) = self.registry.open_session(sid).await?;
            let session = Session::new(ConversationId(sid.to_string()), db.clone()).await;
            self.observe_stop_requests(sid, &session).await;
            if let Some(branch) =
                crate::session::execution::latest_branch(&db, &wait.attempt_id).await?
            {
                anyhow::ensure!(
                    branch.incarnation == self.job_incarnation,
                    "suspended execution lineage changed to another executor"
                );
                if crate::session::execution::observe(&db, &branch.id)
                    .await?
                    .is_some()
                {
                    return Ok(false);
                }
            }
            drop(session);
            drop(db);
            let ready = cancel.is_cancelled()
                || Utc::now() >= wait.deadline
                || match self.job_status(&wait.child).await {
                    Ok(status) => {
                        status.state.is_terminal()
                            || matches!(
                                status.state,
                                crate::session::jobs::JobState::Interrupted { .. }
                            )
                    }
                    Err(_) => true,
                };
            if ready {
                break;
            }
            #[cfg(test)]
            self.suspended_wait_reconciliations
                .fetch_add(1, Ordering::SeqCst);
            let remaining = (wait.deadline - Utc::now()).to_std().unwrap_or_default();
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = notify.notified() => {},
                _ = tokio::time::sleep(remaining.min(std::time::Duration::from_secs(1))) => {},
            }
        }
        // No loaded Session/context waits for capacity. A stop wins even if
        // all execution slots are occupied and settles without effects.
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            permit = self.semaphore.clone().acquire_owned() => Some(permit?),
        };
        if self.shutting_down.load(Ordering::Acquire) {
            return Ok(false);
        }
        let (_, db) = self.registry.open_session(sid).await?;
        let session = Session::new(ConversationId(sid.to_string()), db.clone()).await;
        self.observe_stop_requests(sid, &session).await;
        if let Some(branch) =
            crate::session::execution::latest_branch(&db, &wait.attempt_id).await?
        {
            anyhow::ensure!(
                branch.incarnation == self.job_incarnation,
                "suspended execution lineage changed during capacity admission"
            );
            if crate::session::execution::observe(&db, &branch.id)
                .await?
                .is_some()
            {
                return Ok(false);
            }
        }
        let checkpoint = session
            .latest_job_wait_checkpoint(&wait.attempt_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("suspended wait checkpoint missing"))?;
        let request = session
            .turn_request(
                &checkpoint.request_id,
                |name| self.agents.get(name).is_some(),
                &self.live_attempts.lock().await.clone(),
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("suspended wait request missing"))?;
        anyhow::ensure!(
            matches!(&request.state, TurnRequestState::InFlight { attempt_id }
            if attempt_id == &wait.attempt_id),
            "suspended attempt changed"
        );
        anyhow::ensure!(
            checkpoint.child_session_db_id == wait.child,
            "suspended child changed"
        );
        anyhow::ensure!(
            checkpoint.deadline.min(
                checkpoint
                    .policy_deadline()
                    .ok_or_else(|| anyhow::anyhow!("suspended policy deadline invalid"))?
            ) == wait.deadline,
            "suspended deadline changed"
        );
        let plan = session
            .resumable_job_wait(&request.id, checkpoint)
            .await?
            .map_err(|reason| anyhow::anyhow!(reason))?;
        if cancel.is_cancelled() {
            self.settle_suspended_stop(sid, session, plan).await?;
            return Ok(false);
        }
        let job = if crate::session::jobs::is_job_session(&db).await {
            Some(
                self.validate_accepted_job(&db)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("suspended job no longer accepted"))?,
            )
        } else {
            None
        };
        // The reservation is still ours. Reuse the restart loader and all
        // current attachment/home/scope/participation checks after admission.
        // Remove only our descriptor before spawning: the new segment may
        // checkpoint its next wait before this waiter returns.
        self.suspended_waits.lock().await.remove(sid);
        let resumed = self
            .resume_checkpointed_wait(sid, &db, job.as_ref(), request, plan, permit, true)
            .await?;
        Ok(matches!(resumed, Some(Some(_))))
    }

    async fn settle_suspended_stop(
        &self,
        sid: &str,
        session: Session,
        plan: crate::session::wait_resume::ResumableWait,
    ) -> anyhow::Result<()> {
        let attempt = session
            .read_turn_attempt(&plan.checkpoint.attempt_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("suspended attempt missing"))?;
        let branch =
            crate::session::execution::latest_branch(session.database(), &attempt.attempt_id)
                .await?;
        let job = if crate::session::jobs::is_job_session(session.database()).await {
            self.validate_accepted_job(session.database()).await?
        } else {
            None
        };
        let requested_model = session
            .turn_transcript(&attempt.attempt_id)
            .await?
            .into_iter()
            .rev()
            .find_map(|record| record.identity.map(|identity| identity.requested_model))
            .unwrap_or_default();
        let session = Arc::new(Mutex::new(session));
        let recorder = Arc::new(SessionRuntimeRecorder {
            session: session.clone(),
            request_id: attempt.request_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            sequence: AtomicU32::new(plan.next_sequence as u32),
            terminal: std::sync::Mutex::new(None),
            explicit_bridge_send: AtomicBool::new(false),
            agent_name: plan.checkpoint.agent_name.clone(),
            requested_model,
            context_snapshot: None,
            can_suspend: false,
            branch: branch.as_ref().map(|b| b.id.clone()),
            #[cfg(test)]
            fail_after: self.transcript_fail_after.clone(),
        });
        let metadata = runtime::cancel_suspended_wait(recorder.clone(), plan.runtime)
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        let transcript = recorder.terminal.lock().unwrap().take();
        let mut s = session.lock().await;
        s.complete_turn_attempt_for_branch(
            &attempt,
            Some(SessionEntry {
                sender: plan.checkpoint.agent_name,
                content: TURN_STOPPED_MESSAGE.into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Error,
                metadata,
                routing: None,
            }),
            None,
            transcript,
            branch.as_ref().map(|b| b.id.as_str()),
        )
        .await?;
        if let Some(job) = job {
            let (queue, agent, peer) = self.record_participating_job(s.database(), &job).await?;
            crate::executor_db::reconcile_job(
                &queue,
                s.database(),
                &job,
                &agent,
                &peer,
                &self.live_attempts.lock().await.clone(),
            )
            .await?;
        }
        info!(session_db_id = sid, attempt_id = %attempt.attempt_id, "Stopped unloaded checkpointed wait without resuming effects");
        Ok(())
    }
}
