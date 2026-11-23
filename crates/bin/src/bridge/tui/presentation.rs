//! Native Snapshot delivery. A registration owns its callback; the terminal
//! runtime owns the reads that callback requested.
use super::*;
use eidetica::{Database, Snapshot, WriteCallback};
use std::sync::{Mutex, atomic::AtomicU64};
use tokio::sync::oneshot;

const LOAD_LIMIT: usize = 4;
static NEXT_OBSERVATION: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_id() -> u64 {
    NEXT_OBSERVATION.fetch_add(1, Ordering::Relaxed)
}

/// Preparing a tab can outlive the navigation that requested it: dropping an
/// in-progress native verification read can lose another watch's publication.
/// Admission is before spawn, and shutdown drains only already admitted work.
pub(super) struct Preparations {
    permits: Arc<Semaphore>,
    tasks: Mutex<JoinSet<()>>,
}

#[derive(Clone)]
pub(super) struct Sender {
    updates: mpsc::Sender<Delivery>,
    preparations: Arc<Preparations>,
}

pub(super) fn channel() -> (Sender, mpsc::Receiver<Delivery>) {
    let (updates, rx) = mpsc::channel(64);
    (
        Sender {
            updates,
            preparations: Arc::new(Preparations::new(2)),
        },
        rx,
    )
}

impl Sender {
    pub(super) async fn prepare<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T, String>> + Send + 'static,
    ) -> Result<T, String> {
        self.preparations.prepare(work).await
    }

    pub(super) async fn drain(&self) {
        self.preparations.drain().await;
    }
}

impl Preparations {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(limit)),
            tasks: Mutex::new(JoinSet::new()),
        }
    }

    pub(super) async fn prepare<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T, String>> + Send + 'static,
    ) -> Result<T, String> {
        let permit = self.acquire().await?;
        let (reply, result) = oneshot::channel();
        {
            let mut tasks = self.tasks.lock().expect("preparation lock poisoned");
            if self.permits.is_closed() {
                return Err("TUI closed".into());
            }
            while tasks.try_join_next().is_some() {}
            tasks.spawn(async move {
                let _permit = permit;
                let _ = reply.send(supervised::work(work).await);
            });
        }
        result.await.map_err(|_| "Presentation reader stopped")?
    }

    pub(super) async fn acquire(&self) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "TUI closed".into())
    }

    pub(super) async fn drain(&self) {
        self.permits.close();
        let mut tasks = std::mem::take(&mut *self.tasks.lock().expect("preparation lock poisoned"));
        while tasks.join_next().await.is_some() {}
    }
}

pub(super) struct Delivery {
    pub(super) session: String,
    pub(super) observation: u64,
    pub(super) tips: Snapshot,
    pub(super) handled: oneshot::Sender<()>,
}

/// Bootstrap travels privately with the new registration. Native delivery
/// cannot advance to the shared queue until the GUI installs this first view
/// (or discards the whole registration).
pub(super) async fn subscribe(
    db: &Database,
    observation: u64,
    tx: Sender,
) -> anyhow::Result<(WriteCallback, Delivery)> {
    subscribe_to(db, observation, tx.updates).await
}

pub(super) async fn subscribe_to(
    db: &Database,
    observation: u64,
    updates: mpsc::Sender<Delivery>,
) -> anyhow::Result<(WriteCallback, Delivery)> {
    let session = db.root_id().to_string();
    let (first, initial) = oneshot::channel();
    let first = Mutex::new(Some(first));
    let callback = db
        .on_write(move |tips, _| {
            let first = first.lock().expect("initial delivery lock poisoned").take();
            let (handled, applied) = oneshot::channel();
            let change = Delivery {
                session: session.clone(),
                observation,
                tips: tips.clone(),
                handled,
            };
            let updates = updates.clone();
            async move {
                let sent = match first {
                    Some(first) => first.send(change).is_ok(),
                    None => updates.send(change).await.is_ok(),
                };
                if sent {
                    let _ = applied.await;
                }
                Ok(())
            }
        })
        .await?;
    // Only waiting for delivery (not performing a native read) is cancellable.
    let initial = tokio::time::timeout(supervised::READ_TIMEOUT, initial).await??;
    Ok((callback, initial))
}

pub(super) type TabSnapshot = (
    refresh::SessionSnapshot,
    Option<chaz_core::session::jobs::JobState>,
);

pub(super) struct Loaded {
    delivery: Delivery,
    result: Result<Box<TabSnapshot>, String>,
    partial: Option<Partial>,
}

struct Partial {
    status: Result<Vec<String>, String>,
    steering: Result<chaz_core::session::steering::SteeringView, String>,
}

fn tab_index(app: &App, delivery: &Delivery) -> Option<usize> {
    app.tab_index_for(&delivery.session).filter(|&i| {
        let live = &app.tabs[i].live;
        !live.unbound
            && live
                .observation
                .as_ref()
                .is_some_and(|o| o.id == delivery.observation)
    })
}

#[derive(Default)]
pub(super) struct Reads(JoinSet<Loaded>);

impl Reads {
    pub(super) fn has_capacity(&self) -> bool {
        self.0.len() < LOAD_LIMIT
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) async fn join_next(&mut self) -> Option<Result<Loaded, tokio::task::JoinError>> {
        self.0.join_next().await
    }

    /// Rebinds install an authorized handle first. Consume their initial
    /// delivery exactly like a subsequent watch update, once there's capacity.
    pub(super) fn start_initial(&mut self, app: &mut App) {
        while self.has_capacity() {
            let initial = app
                .tabs
                .iter_mut()
                .find_map(|tab| tab.live.observation.as_mut()?.initial.take());
            let Some(initial) = initial else { break };
            self.start(app, initial);
        }
    }

    pub(super) fn start(&mut self, app: &mut App, delivery: Delivery) {
        let Some(idx) = tab_index(app, &delivery) else {
            return;
        };
        debug_assert!(self.has_capacity());
        let tab = &mut app.tabs[idx];
        tab.live.snapshot_started = Some(tokio::time::Instant::now());
        let db = tab.session_db.clone();
        let job = tab.job.is_some();
        let tips = delivery.tips.clone();
        self.0.spawn(async move {
            let loaded = supervised::work(Box::pin(async {
                let result = if job {
                    jobs::snapshot_at(&db, tips.clone())
                        .await
                        .map(|loaded| (loaded.session, Some(loaded.state)))
                } else {
                    refresh::load_session_snapshot_at(&db, tips.clone())
                        .await
                        .map(|loaded| (loaded, None))
                };
                let partial = if result.is_err() {
                    // Invalid transcript/meta must not hide readable status at
                    // that same frontier. Never bypass current native authority.
                    let partial = match chaz_core::session::jobs::require_observer_write(&db).await
                    {
                        Ok(_) => {
                            let (status, steering) = tokio::join!(
                                refresh::load_status_at(&db, &tips),
                                Session::steering_view_at(&db, &tips),
                            );
                            Partial {
                                status,
                                steering: steering.map_err(|e| e.to_string()),
                            }
                        }
                        Err(error) => Partial {
                            status: Err(error.to_string()),
                            steering: Err(error.to_string()),
                        },
                    };
                    Some(partial)
                } else {
                    None
                };
                Ok((result, partial))
            }))
            .await;
            let (result, partial) = loaded.unwrap_or_else(|error| (Err(error), None));
            Loaded {
                delivery,
                result: result.map(Box::new),
                partial,
            }
        });
    }
}

fn overdue_at(tab: &Tab) -> Option<tokio::time::Instant> {
    tab.live
        .snapshot_started
        .filter(|_| !tab.live.snapshot_overdue)
        .map(|started| started + supervised::READ_TIMEOUT)
}

pub(super) fn next_overdue(app: &App) -> Option<tokio::time::Instant> {
    app.tabs.iter().filter_map(overdue_at).min()
}

pub(super) fn mark_overdue(app: &mut App, now: tokio::time::Instant) {
    for tab in &mut app.tabs {
        if overdue_at(tab).is_some_and(|deadline| deadline <= now) {
            tab.live.snapshot_overdue = true;
            tracing::warn!(session = %tab.session_db_id, "Snapshot read overdue; retaining last-good state while it finishes");
        }
    }
}

/// Keep the delivery (including the ack) owned until GUI state is applied.
/// On a closed/rebound route just retire it, even if native work succeeded.
pub(super) fn apply_then(
    app: &mut App,
    loaded: Loaded,
    derive: impl FnOnce(&mut App, usize),
) -> Option<String> {
    let idx = tab_index(app, &loaded.delivery)?;
    app.tabs[idx].live.snapshot_started = None;
    app.tabs[idx].live.snapshot_overdue = false;
    if refresh::apply_watched(app, idx, loaded.result.map(|snapshot| *snapshot)) {
        derive(app, idx);
    }
    if let Some(partial) = loaded.partial {
        refresh::replace_status(&mut app.tabs[idx], partial.status);
        refresh::replace_steering(app, idx, partial.steering);
    }
    let _ = loaded.delivery.handled.send(());
    Some(loaded.delivery.session)
}

#[cfg(test)]
pub(super) fn apply(
    app: &mut App,
    loaded: Loaded,
    resolve: impl FnOnce(&str, &SessionMeta) -> refresh::Resolved,
) -> Option<String> {
    apply_then(app, loaded, |app, idx| {
        refresh::derive_fixture(&mut app.tabs[idx], resolve)
    })
}
