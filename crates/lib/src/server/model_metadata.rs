//! Peer-local learned model metadata: one native read owner per runtime.
use crate::{
    backends::BackendManager,
    model_info_store::{ModelInfoStore, ModelInfos},
};
use eidetica::{Database, Snapshot, WriteCallback};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelMetadata {
    pub infos: Arc<ModelInfos>,
    pub ready: bool,
    pub error: Option<Arc<str>>,
}

pub struct ModelMetadataUpdate {
    pub state: ModelMetadata,
    // Only the receiving GUI may acknowledge its application.
    pub handled: oneshot::Sender<()>,
}

#[derive(Default)]
pub(super) struct Feed(Mutex<FeedState>);

#[derive(Default)]
struct FeedState {
    latest: ModelMetadata,
    viewer: Option<mpsc::Sender<ModelMetadataUpdate>>,
}

impl Feed {
    pub(super) fn subscribe(&self) -> (ModelMetadata, mpsc::Receiver<ModelMetadataUpdate>) {
        let (tx, rx) = mpsc::channel(1);
        let mut feed = self.0.lock().unwrap();
        feed.viewer = Some(tx);
        (feed.latest.clone(), rx)
    }

    fn commit(&self, state: &ModelMetadata) -> Option<mpsc::Sender<ModelMetadataUpdate>> {
        let mut feed = self.0.lock().unwrap();
        if &feed.latest == state {
            return None;
        }
        feed.latest.clone_from(state);
        feed.viewer.clone()
    }
}

type Delivery = (Snapshot, oneshot::Sender<()>);

async fn subscribe(db: &Database) -> eidetica::Result<(WriteCallback, mpsc::Receiver<Delivery>)> {
    let (tx, rx) = mpsc::channel(1);
    let registration = db
        .on_write(move |snapshot, _| {
            let tx = tx.clone();
            let snapshot = snapshot.clone();
            async move {
                let (handled, applied) = oneshot::channel();
                if tx.send((snapshot, handled)).await.is_ok() {
                    let _ = applied.await;
                }
                Ok(())
            }
        })
        .await?;
    Ok((registration, rx))
}

pub(super) struct Observation {
    stop: watch::Sender<bool>,
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Observation {
    pub(super) fn start(
        db: Database,
        backend: BackendManager,
        feed: Arc<Feed>,
    ) -> (Self, oneshot::Receiver<()>) {
        let (stop, signal) = watch::channel(false);
        let (ready, initial) = oneshot::channel();
        let task = tokio::spawn(observe(db, backend, feed, signal, ready));
        (
            Self {
                stop,
                task: tokio::sync::Mutex::new(Some(task)),
            },
            initial,
        )
    }

    pub(super) async fn stop(&self) {
        // The join handle is awaited once. Serialize concurrent shutdown, and
        // retain it while awaiting so cancellation can be retried.
        self.stop.send_replace(true);
        let mut task = self.task.lock().await;
        if let Some(join) = task.as_mut() {
            if let Err(error) = join.await {
                tracing::warn!(%error, "Model metadata worker stopped");
            }
            task.take();
        }
    }
}

async fn observe(
    db: Database,
    backend: BackendManager,
    feed: Arc<Feed>,
    mut stop: watch::Receiver<bool>,
    initial: oneshot::Sender<()>,
) {
    let store = ModelInfoStore::new(db.clone());
    let mut state = ModelMetadata::default();
    let mut initial = Some(initial);
    let mut attempt = 0;
    while !*stop.borrow() {
        let failure = match subscribe(&db).await {
            Ok((_registration, mut deliveries)) => {
                let mut first = true;
                let initial_deadline = tokio::time::sleep(Duration::from_secs(10));
                tokio::pin!(initial_deadline);
                loop {
                    let delivery = tokio::select! {
                        biased;
                        _ = stop.changed() => return,
                        delivery = deliveries.recv() => delivery,
                        _ = &mut initial_deadline, if first => break Some("Model metadata initial delivery timed out".into()),
                    };
                    let Some((snapshot, handled)) = delivery else {
                        break Some("Model metadata subscription ended".into());
                    };
                    first = false;
                    // Stop or GUI retirement may not abort this native read.
                    // It can promote verification for another registration.
                    let success = match store.read_at(&snapshot).await {
                        Ok(infos) => {
                            if *state.infos != infos {
                                state.infos = Arc::new(infos);
                                backend.replace_model_info(state.infos.clone());
                            }
                            state.ready = true;
                            state.error = None;
                            attempt = 0;
                            true
                        }
                        Err(error) => {
                            let error = format!("Model metadata read failed: {error:#}");
                            tracing::warn!(%error, "Retaining last model metadata");
                            state.error = Some(error.into());
                            false
                        }
                    };
                    if let Some(initial) = initial.take() {
                        let _ = initial.send(());
                    }
                    if !publish(&feed, &state, &mut stop).await {
                        return;
                    }
                    let _ = handled.send(());
                    if !success {
                        break None;
                    }
                }
            }
            Err(error) => Some(format!("Model metadata registration failed: {error}")),
        };
        if let Some(error) = failure {
            tracing::warn!(%error, "Retaining last model metadata");
            state.error = Some(error.into());
            if let Some(initial) = initial.take() {
                let _ = initial.send(());
            }
            if !publish(&feed, &state, &mut stop).await {
                return;
            }
        }
        // A fresh subscription gets an initial delivery even on a quiet DB.
        // Healthy generations never poll the peer store.
        tokio::select! {
            _ = stop.changed() => return,
            _ = tokio::time::sleep(crate::instance::service_reconnect_delay(attempt)) => {},
        }
        attempt = attempt.saturating_add(1);
    }
}

async fn publish(feed: &Feed, state: &ModelMetadata, stop: &mut watch::Receiver<bool>) -> bool {
    let Some(viewer) = feed.commit(state) else {
        return !*stop.borrow();
    };
    let (handled, applied) = oneshot::channel();
    tokio::select! {
        _ = stop.changed() => false,
        _ = async {
            if viewer.send(ModelMetadataUpdate { state: state.clone(), handled }).await.is_ok() {
                let _ = applied.await;
            }
        } => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backends::ModelInfo, security::SecretStore};

    #[tokio::test]
    async fn model_metadata_retains_last_good_and_reregisters_after_an_error() {
        let (_instance, registry) = crate::test_support::fresh_session_registry().await;
        let db = registry.chaz_peer();
        let store = ModelInfoStore::new(db.clone());
        store
            .put(&ModelInfo {
                id: "m".into(),
                context_window: Some(64_000),
                ..Default::default()
            })
            .await
            .unwrap();
        let backend = BackendManager::new(&None, SecretStore::new(db.clone()).await);
        let feed = Arc::new(Feed::default());
        let (_, mut gui) = feed.subscribe();
        let (watch, ready) = Observation::start(db.clone(), backend.clone(), feed);
        tokio::time::timeout(Duration::from_secs(4), async {
            // Runtime readiness never depends on the GUI acknowledging.
            ready.await.unwrap();
            let initial = gui.recv().await.unwrap();
            let prior = initial.state.infos.clone();
            assert_eq!(backend.context_window("m"), Some(64_000));
            let txn = db.new_transaction().await.unwrap();
            let doc = txn
                .get_store::<eidetica::store::DocStore>("model_info")
                .await
                .unwrap();
            let valid = doc.get_string("in_use").await.unwrap();
            doc.set_string("in_use", "not a map").await.unwrap();
            txn.commit().await.unwrap();
            initial.handled.send(()).unwrap();
            let failed = gui.recv().await.unwrap();
            assert!(failed.state.error.is_some());
            assert_eq!(failed.state.infos, prior);
            assert_eq!(backend.context_window("m"), Some(64_000));
            // Repair before acknowledging the failing delivery. Nothing
            // else writes once the failed registration has been retired.
            let txn = db.new_transaction().await.unwrap();
            txn.get_store::<eidetica::store::DocStore>("model_info")
                .await
                .unwrap()
                .set_string("in_use", valid)
                .await
                .unwrap();
            txn.commit().await.unwrap();
            failed.handled.send(()).unwrap();
            let recovered = gui.recv().await.unwrap();
            assert!(recovered.state.error.is_none());
            assert_eq!(recovered.state.infos, prior);
            // Leaving an ack unhandled cannot stall runtime shutdown.
            watch.stop().await;
        })
        .await
        .unwrap();
    }
}
