//! Native callbacks read a pinned view; the terminal consumes only the latest result.
use super::*;
use eidetica::{Database, Snapshot, WriteCallback};
use std::sync::{Mutex, atomic::AtomicU64};
use tokio::sync::{Notify, oneshot};

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
    wake: Arc<Notify>,
    reads: Arc<Preparations>,
    preparations: Arc<Preparations>,
}

pub(super) fn channel() -> (Sender, Arc<Notify>) {
    let wake = Arc::new(Notify::new());
    (
        Sender {
            wake: wake.clone(),
            reads: Arc::new(Preparations::new(4)),
            preparations: Arc::new(Preparations::new(2)),
        },
        wake,
    )
}

impl Sender {
    pub(super) async fn prepare<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T, String>> + Send + 'static,
    ) -> Result<T, String> {
        self.preparations.prepare(work).await
    }

    #[cfg(test)]
    pub(super) async fn hold_reads(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.reads
            .permits
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap()
    }

    pub(super) async fn drain(&self) {
        tokio::join!(self.preparations.drain(), self.reads.drain());
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

/// A registration keeps one completed result, even if the GUI stops reading.
/// The shared wake-up carries no data and can be coalesced across registrations.
pub(super) struct Latest<T> {
    state: Mutex<(Option<T>, Option<tokio::time::Instant>)>,
    wake: Arc<Notify>,
}

impl<T> Latest<T> {
    pub(super) fn new(wake: Arc<Notify>) -> Self {
        Self {
            state: Mutex::new((None, None)),
            wake,
        }
    }

    fn start(&self) {
        self.state.lock().expect("projection lock poisoned").1 = Some(tokio::time::Instant::now());
        self.wake.notify_one();
    }

    pub(super) fn publish(&self, value: T) {
        // Drop a displaced transcript outside the lock.
        let old = std::mem::replace(
            &mut *self.state.lock().expect("projection lock poisoned"),
            (Some(value), None),
        );
        self.wake.notify_one();
        drop(old);
    }

    pub(super) fn take(&self) -> Option<T> {
        self.state
            .lock()
            .expect("projection lock poisoned")
            .0
            .take()
    }

    pub(super) fn started(&self) -> Option<tokio::time::Instant> {
        self.state.lock().expect("projection lock poisoned").1
    }
}

pub(super) async fn subscribe<T: Send + 'static, F: Future<Output = T> + Send + 'static>(
    db: &Database,
    latest: Arc<Latest<T>>,
    read: impl Fn(Snapshot) -> F + Send + Sync + 'static,
) -> anyhow::Result<(WriteCallback, oneshot::Receiver<()>)> {
    let (first, initial) = oneshot::channel();
    let first = Mutex::new(Some(first));
    let callback = db
        .on_write(move |tips, _| {
            let first = first.lock().expect("initial delivery lock poisoned").take();
            let latest = latest.clone();
            let result = read(tips.clone());
            async move {
                latest.start();
                // The native watch serializes reads, not frames. A GUI that is
                // busy drawing (or closed) cannot hold up the next callback.
                latest.publish(result.await);
                if let Some(first) = first {
                    let _ = first.send(());
                }
                Ok(())
            }
        })
        .await?;
    Ok((callback, initial))
}

pub(super) type TabSnapshot = (
    refresh::SessionSnapshot,
    Option<chaz_core::session::jobs::JobState>,
);

pub(super) struct Loaded {
    pub(super) result: Result<Box<TabSnapshot>, String>,
    partial: Option<Partial>,
}

struct Partial {
    status: Result<Vec<String>, String>,
    steering: Result<chaz_core::session::steering::SteeringView, String>,
}

pub(super) struct SessionWatch {
    _callback: WriteCallback,
    initial: oneshot::Receiver<()>,
    latest: Arc<Latest<Loaded>>,
}

impl SessionWatch {
    pub(super) async fn open(db: &Database, tx: Sender, job: bool) -> anyhow::Result<Self> {
        let latest = Arc::new(Latest::new(tx.wake));
        let source = db.clone();
        let (callback, initial) = subscribe(db, latest.clone(), move |tips| {
            let db = source.clone();
            let pool = tx.reads.clone();
            async move {
                // This read must finish even if its native callback is retired
                // mid-verification: promotion can notify other registrations.
                let result = pool
                    .prepare(async move { load(&db, tips, job).await })
                    .await;
                result.unwrap_or_else(|error| Loaded {
                    result: Err(error),
                    partial: None,
                })
            }
        })
        .await?;
        Ok(Self {
            _callback: callback,
            initial,
            latest,
        })
    }

    pub(super) async fn initial(&mut self) -> Result<Loaded, String> {
        tokio::time::timeout(supervised::READ_TIMEOUT, &mut self.initial)
            .await
            .map_err(|_| "Initial session projection timed out".to_owned())?
            .map_err(|_| "Session observer stopped".to_owned())?;
        self.take()
            .ok_or_else(|| "Initial session projection missing".into())
    }

    #[cfg(test)]
    pub(super) fn has_snapshot(&self, tips: &Snapshot) -> bool {
        self.latest
            .state
            .lock()
            .unwrap()
            .0
            .as_ref()
            .and_then(|loaded| loaded.result.as_ref().ok())
            .is_some_and(|loaded| loaded.0.tips == *tips)
    }

    pub(super) fn started(&self) -> Option<tokio::time::Instant> {
        self.latest.started()
    }

    pub(super) fn take(&self) -> Option<Loaded> {
        self.latest.take()
    }
}

async fn load(db: &Database, tips: Snapshot, job: bool) -> Result<Loaded, String> {
    let result = if job {
        jobs::snapshot_at(db, tips.clone())
            .await
            .map(|loaded| (loaded.session, Some(loaded.state)))
    } else {
        refresh::load_session_snapshot_at(db, tips.clone())
            .await
            .map(|loaded| (loaded, None))
    };
    let partial = if result.is_err() {
        // Invalid transcript/meta must not hide readable status at that
        // frontier. Never bypass current native authority.
        let partial = match chaz_core::session::jobs::require_observer_write(db).await {
            Ok(_) => {
                let (status, steering) = tokio::join!(
                    refresh::load_status_at(db, &tips),
                    Session::steering_view_at(db, &tips),
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
    Ok(Loaded {
        result: result.map(Box::new),
        partial,
    })
}

fn overdue_at(tab: &Tab) -> Option<tokio::time::Instant> {
    tab.live
        .snapshot_started()
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

pub(super) fn apply_latest(app: &mut App, mut derive: impl FnMut(&mut App, usize)) -> bool {
    let mut changed = false;
    for idx in 0..app.tabs.len() {
        let Some(loaded) = app.tabs[idx]
            .live
            .observation
            .as_ref()
            .and_then(|o| o.watch.take())
        else {
            continue;
        };
        app.tabs[idx].live.snapshot_overdue = false;
        if refresh::apply_watched(app, idx, loaded.result.map(|snapshot| *snapshot)) {
            derive(app, idx);
        }
        if let Some(partial) = loaded.partial {
            refresh::replace_status(&mut app.tabs[idx], partial.status);
            refresh::replace_steering(app, idx, partial.steering);
        }
        let id = app.tabs[idx].session_db_id.clone();
        session_pages::changed(app, &id);
        changed = true;
    }
    changed
}
