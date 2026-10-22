//! Write callbacks invalidate evidence; the renewal clock only ages it and sends
//! Matrix leases. Each binding owns its subscription and one serial reader.
use super::*;
use chaz_core::session::TurnActivity;
use futures::FutureExt;
use std::{future::Future, panic::AssertUnwindSafe, time::Duration};
use tokio::sync::watch;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct Watcher(tokio::task::JoinHandle<()>);
impl Drop for Watcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl Watcher {
    pub(super) fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.0.abort_handle()
    }

    pub(super) async fn finish(mut self) {
        // The driver cancels a pending read/send when its generation stops and
        // bounds the final release. Abort is only the fallback for a stuck task.
        if tokio::time::timeout(TYPING_LEASE, &mut self.0)
            .await
            .is_err()
        {
            self.0.abort();
            let _ = (&mut self.0).await;
        }
    }
}

pub(super) async fn attach(
    db: &eidetica::Database,
    room: Room,
    stopped: watch::Receiver<bool>,
) -> anyhow::Result<Watcher> {
    let changed = Arc::new(Notify::new());
    let wake = changed.clone();
    // Subscribe BEFORE the initial read. Cursor registration is not an eager
    // replay in embedded mode; no state read before registration is trusted.
    let subscription = db
        .on_write(move |_, _| {
            wake.notify_one();
            async { Ok(()) }
        })
        .await?;
    let db = db.clone();
    Ok(Watcher(tokio::spawn(async move {
        let _subscription = subscription;
        let result = AssertUnwindSafe(run(
            changed,
            stopped,
            || Session::turn_activity(&db),
            chrono::Utc::now,
            || room.state() == RoomState::Joined,
            |active| send_turn_typing(&room, active),
        ))
        .catch_unwind()
        .await;
        if result.is_err() {
            error!("Matrix activity watcher panicked");
        }
    })))
}

async fn run<R, RF, N, J, S, SF, E>(
    changed: Arc<Notify>,
    mut stopped: watch::Receiver<bool>,
    mut read: R,
    now: N,
    joined: J,
    mut send: S,
) where
    R: FnMut() -> RF,
    RF: Future<Output = anyhow::Result<TurnActivity>>,
    N: Fn() -> chrono::DateTime<chrono::Utc>,
    J: Fn() -> bool,
    S: FnMut(bool) -> SF,
    SF: Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    let mut tick = tokio::time::interval(TYPING_RENEWAL);
    // Consume the interval's immediate tick: the explicit initial load is enough.
    tick.tick().await;
    let mut dirty = true;
    let mut evidence: Option<TurnActivity> = None;
    let mut typing = false;
    loop {
        if *stopped.borrow() || !joined() {
            break;
        }
        if dirty {
            let result = tokio::select! {
                biased;
                _ = stopped.changed() => break,
                result = tokio::time::timeout(READ_TIMEOUT, read()) => result,
            };
            match result {
                Ok(Ok(value)) => {
                    evidence = Some(value);
                    dirty = false;
                }
                Ok(Err(error)) => {
                    error!(%error, "Could not read Matrix turn activity; retaining last evidence")
                }
                Err(_) => error!("Matrix turn activity read timed out; retaining last evidence"),
            }
        }
        if let Some(evidence) = &evidence {
            let active = evidence.active_at(now()).next().is_some();
            let result = tokio::select! {
                biased;
                _ = stopped.changed() => break,
                result = update_typing(&mut typing, active, &mut send) => result,
            };
            if let Err(error) = result {
                error!(%error, "Could not update Matrix typing notice");
            }
        }
        tokio::select! {
            biased;
            _ = stopped.changed() => break,
            _ = changed.notified() => dirty = true,
            _ = tick.tick() => {},
        }
    }
    if typing {
        let _ = tokio::time::timeout(TYPING_LEASE, send(false)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chaz_core::{
        session::{TurnAttempt, TurnRequestId},
        types::ConversationId,
    };
    use eidetica::{Instance, NewAccount, backend::database::InMemory, store::Table};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::{Semaphore, mpsc};

    async fn fixture() -> (Instance, Session) {
        let (owner, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::passwordless("typing"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let session = Session::new(ConversationId(db.root_id().to_string()), db).await;
        (owner, session)
    }

    struct Driver {
        watcher: Watcher,
        stop: watch::Sender<bool>,
        reads: Arc<AtomicUsize>,
        fail_send: Arc<AtomicBool>,
        sent: mpsc::UnboundedReceiver<bool>,
        changed: Arc<Notify>,
        observed: mpsc::UnboundedReceiver<bool>,
    }
    impl Driver {
        async fn start(db: eidetica::Database, gate: Arc<Semaphore>) -> Self {
            let changed = Arc::new(Notify::new());
            let wake = changed.clone();
            let subscription = db
                .on_write(move |_, _| {
                    wake.notify_one();
                    async { Ok(()) }
                })
                .await
                .unwrap();
            let (stop, stopped) = watch::channel(false);
            let reads = Arc::new(AtomicUsize::new(0));
            let fail_send = Arc::new(AtomicBool::new(false));
            let (sent, received) = mpsc::unbounded_channel();
            let (observations, observed) = mpsc::unbounded_channel();
            let base = chrono::Utc::now();
            let clock = tokio::time::Instant::now();
            let watcher = Watcher(tokio::spawn({
                let reads = reads.clone();
                let fail_send = fail_send.clone();
                let changed = changed.clone();
                async move {
                    let _subscription = subscription;
                    run(
                        changed,
                        stopped,
                        || {
                            let reads = reads.clone();
                            let gate = gate.clone();
                            let db = db.clone();
                            let observations = observations.clone();
                            async move {
                                let evidence = Session::turn_activity(&db).await;
                                let _ = observations.send(evidence.is_ok());
                                reads.fetch_add(1, Ordering::SeqCst);
                                // Captured evidence is deliberately gated AFTER reading.
                                if let Ok(permit) = gate.acquire().await {
                                    permit.forget();
                                }
                                evidence
                            }
                        },
                        || base + chrono::Duration::from_std(clock.elapsed()).unwrap(),
                        || true,
                        |active| {
                            sent.send(active).unwrap();
                            let fail = fail_send.swap(false, Ordering::SeqCst);
                            async move {
                                if fail {
                                    Err("fixture send failed")
                                } else {
                                    Ok(())
                                }
                            }
                        },
                    )
                    .await;
                }
            }));
            Self {
                watcher,
                stop,
                reads,
                fail_send,
                sent: received,
                changed,
                observed,
            }
        }
        async fn stop(self) {
            self.stop.send_replace(true);
            self.watcher.finish().await;
        }
    }
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_typing_ticks_renew_cancel_retry_and_expire_without_reads() {
        let (_owner, mut session) = fixture().await;
        let attempt = session
            .start_turn_attempt(TurnRequestId::parse("active"))
            .await
            .unwrap();
        let mut driver =
            Driver::start(session.database().clone(), Arc::new(Semaphore::new(100))).await;
        assert_eq!(driver.sent.recv().await, Some(true));
        assert_eq!(driver.reads.load(Ordering::SeqCst), 1);
        driver.fail_send.store(true, Ordering::SeqCst);
        for _ in 0..10 {
            tokio::time::advance(TYPING_RENEWAL).await;
            assert_eq!(driver.sent.recv().await, Some(true));
        }
        assert_eq!(
            driver.reads.load(Ordering::SeqCst),
            1,
            "renewals reloaded activity tables"
        );
        driver.fail_send.store(true, Ordering::SeqCst);
        session.complete_turn_attempt(&attempt, None).await.unwrap();
        assert_eq!(driver.sent.recv().await, Some(false)); // send failed: retry on clock
        tokio::time::advance(TYPING_RENEWAL).await;
        assert_eq!(driver.sent.recv().await, Some(false));
        Session::renew_turn_activity(session.database(), &attempt.attempt_id)
            .await
            .unwrap();
        settle().await;
        assert!(
            driver.sent.try_recv().is_err(),
            "late heartbeat resurrected completion"
        );
        assert_eq!(driver.reads.load(Ordering::SeqCst), 3);
        eprintln!(
            "MATRIX candidate active ticks=10 activity_reads=0 (initial=1); completion/heartbeat reads=2"
        );
        driver.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn future_start_and_exact_orphan_expiry_use_retained_timestamps() {
        let (_owner, session) = fixture().await;
        let mut attempt = session
            .start_turn_attempt(TurnRequestId::parse("future"))
            .await
            .unwrap();
        attempt.started_at = chrono::Utc::now() + chrono::Duration::seconds(3);
        let tx = session.database().new_transaction().await.unwrap();
        tx.get_store::<Table<TurnAttempt>>("turn_attempts")
            .await
            .unwrap()
            .set(&attempt.attempt_id, attempt.clone())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut driver =
            Driver::start(session.database().clone(), Arc::new(Semaphore::new(100))).await;
        settle().await;
        assert_eq!(driver.reads.load(Ordering::SeqCst), 1);
        assert!(
            driver.sent.try_recv().is_err(),
            "future timestamp is not yet visible"
        );
        tokio::time::advance(TYPING_RENEWAL).await;
        assert_eq!(driver.sent.recv().await, Some(true));
        for _ in 0..14 {
            tokio::time::advance(TYPING_RENEWAL).await;
            assert_eq!(driver.sent.recv().await, Some(true));
        }
        tokio::time::advance(TYPING_RENEWAL).await;
        assert_eq!(
            driver.sent.recv().await,
            Some(false),
            "exact 45s age must release without a write"
        );
        assert_eq!(driver.reads.load(Ordering::SeqCst), 1);
        driver.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn callbacks_during_gated_read_coalesce_and_shutdown_cancels_pending_work() {
        let (_owner, mut session) = fixture().await;
        let attempt = session
            .start_turn_attempt(TurnRequestId::parse("active"))
            .await
            .unwrap();
        let gate = Arc::new(Semaphore::new(0));
        let mut driver = Driver::start(session.database().clone(), gate.clone()).await;
        settle().await;
        assert_eq!(driver.reads.load(Ordering::SeqCst), 1);
        session.complete_turn_attempt(&attempt, None).await.unwrap();
        for _ in 0..20 {
            Session::renew_turn_activity(session.database(), &attempt.attempt_id)
                .await
                .unwrap();
        }
        settle().await;
        assert_eq!(
            driver.reads.load(Ordering::SeqCst),
            1,
            "more than one read in flight"
        );
        gate.add_permits(2);
        assert_eq!(driver.sent.recv().await, Some(true));
        assert_eq!(driver.sent.recv().await, Some(false));
        assert_eq!(
            driver.reads.load(Ordering::SeqCst),
            2,
            "burst must need only one follow-up"
        );
        driver.changed.notify_one();
        settle().await;
        assert_eq!(driver.reads.load(Ordering::SeqCst), 3);
        driver.stop().await; // Must cancel a read waiting on an ungranted permit.
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_releases_active_typing_and_drops_subscription() {
        let (_owner, session) = fixture().await;
        session
            .start_turn_attempt(TurnRequestId::parse("active"))
            .await
            .unwrap();
        let mut driver =
            Driver::start(session.database().clone(), Arc::new(Semaphore::new(100))).await;
        assert_eq!(driver.sent.recv().await, Some(true));
        driver.stop.send_replace(true);
        driver.watcher.finish().await;
        assert_eq!(driver.sent.recv().await, Some(false));
        assert_eq!(driver.sent.recv().await, None);
        Session::renew_turn_activity(session.database(), "unknown")
            .await
            .unwrap();
        settle().await;
        assert_eq!(driver.reads.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn service_loss_and_replacement_keep_last_evidence_and_retire_old_observer() {
        use eidetica::service::ServiceServer;
        let (owner, mut session) = fixture().await;
        let attempt = session
            .start_turn_attempt(TurnRequestId::parse("active"))
            .await
            .unwrap();
        let root = session.database().root_id().clone();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("service.sock");
        let service = ServiceServer::bind(owner.clone(), &socket).await.unwrap();
        let (shutdown, receiver) = watch::channel(());
        let service_task = tokio::spawn(service.run(receiver));
        let client = Instance::connect(format!("unix://{}", socket.display()))
            .await
            .unwrap();
        let user = client.login_account("typing", None).await.unwrap();
        let remote = user.open_database(&root).await.unwrap();
        let mut old = Driver::start(remote, Arc::new(Semaphore::new(100))).await;
        assert_eq!(old.observed.recv().await, Some(true));
        assert_eq!(old.sent.recv().await, Some(true));
        drop(shutdown);
        service_task.await.unwrap().unwrap();
        old.changed.notify_one();
        assert_eq!(old.observed.recv().await, Some(false));
        assert_eq!(
            old.sent.recv().await,
            Some(true),
            "read failure discarded last-good evidence"
        );
        old.stop.send_replace(true);
        old.watcher.finish().await;
        assert_eq!(old.sent.recv().await, Some(false));
        assert_eq!(old.sent.recv().await, None);
        let old_reads = old.reads.load(Ordering::SeqCst);
        session.complete_turn_attempt(&attempt, None).await.unwrap();
        let service = ServiceServer::bind(owner, &socket).await.unwrap();
        let (shutdown, receiver) = watch::channel(());
        let service_task = tokio::spawn(service.run(receiver));
        let client = Instance::connect(format!("unix://{}", socket.display()))
            .await
            .unwrap();
        let user = client.login_account("typing", None).await.unwrap();
        let remote = user.open_database(&root).await.unwrap();
        let mut replacement = Driver::start(remote, Arc::new(Semaphore::new(100))).await;
        assert_eq!(replacement.observed.recv().await, Some(true));
        session
            .start_turn_attempt(TurnRequestId::parse("replacement"))
            .await
            .unwrap();
        assert_eq!(replacement.sent.recv().await, Some(true));
        assert_eq!(old.reads.load(Ordering::SeqCst), old_reads);
        replacement.stop().await;
        drop(shutdown);
        service_task.await.unwrap().unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn backend_loss_retries_but_retained_activity_still_expires_on_clock() {
        let (owner, session) = fixture().await;
        session
            .start_turn_attempt(TurnRequestId::parse("active"))
            .await
            .unwrap();
        let mut driver =
            Driver::start(session.database().clone(), Arc::new(Semaphore::new(100))).await;
        assert_eq!(driver.sent.recv().await, Some(true));
        assert_eq!(driver.observed.recv().await, Some(true));
        drop(owner);
        driver.changed.notify_one();
        assert_eq!(driver.observed.recv().await, Some(false));
        assert_eq!(driver.sent.recv().await, Some(true));
        for _ in 0..14 {
            tokio::time::advance(TYPING_RENEWAL).await;
            assert_eq!(driver.observed.recv().await, Some(false));
            assert_eq!(driver.sent.recv().await, Some(true));
        }
        tokio::time::advance(TYPING_RENEWAL).await;
        assert_eq!(driver.observed.recv().await, Some(false));
        assert_eq!(
            driver.sent.recv().await,
            Some(false),
            "last-good activity never expired during loss"
        );
        driver.stop().await;
    }
    #[tokio::test(start_paused = true)]
    async fn forced_generation_cancellation_cannot_send_an_old_release_after_replacement() {
        let (_owner, session) = fixture().await;
        session
            .start_turn_attempt(TurnRequestId::parse("active"))
            .await
            .unwrap();
        let mut old =
            Driver::start(session.database().clone(), Arc::new(Semaphore::new(100))).await;
        assert_eq!(old.sent.recv().await, Some(true));
        let tasks = Arc::new(std::sync::Mutex::new(vec![old.watcher.abort_handle()]));
        let lifetime = super::super::TypingLifetime {
            stop: old.stop.clone(),
            tasks,
        };
        drop(lifetime); // Mirrors aborting the Matrix bridge future, not cooperative shutdown.
        old.watcher.finish().await;
        assert_eq!(
            old.sent.recv().await,
            None,
            "old generation sent after its owner was cancelled"
        );
        let mut replacement =
            Driver::start(session.database().clone(), Arc::new(Semaphore::new(100))).await;
        assert_eq!(replacement.sent.recv().await, Some(true));
        tokio::time::advance(TYPING_RENEWAL).await;
        assert_eq!(replacement.sent.recv().await, Some(true));
        assert_eq!(old.reads.load(Ordering::SeqCst), 1);
        replacement.stop().await;
    }
}
