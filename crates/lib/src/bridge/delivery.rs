//! Durable outbound delivery for transport bridges.
//!
//! A bridge delivers a session's agent `Message` entries to one transport
//! channel. Progress is persisted per [`DeliveryBinding`] — the transport,
//! login, channel, and session a bridge routes on — in the peer-local
//! `chaz_peer` database. Two frontends sharing one Eidetica login therefore
//! keep independent progress, and a restarted or reconnected bridge resumes
//! at its last acknowledged send instead of treating existing history as
//! delivered.
//!
//! [`DeliveryProgress`] has two parts. `delivered_through` is an Eidetica
//! snapshot of the session at which every agent message then present had been
//! delivered; rows committed after it are pending unless `entries` marks them.
//! `entries` holds per-row marks (the acknowledged chunk prefix, or complete)
//! for rows delivered since that snapshot. When a pass ends with nothing
//! pending the snapshot advances and the marks are cleared, so the record
//! stays bounded by in-flight work rather than session length.
//!
//! A mark is written only after the transport acknowledged the send. A crash
//! between the send and the mark repeats that chunk on the next pass. Every
//! chunk carries a stable idempotency key derived from its binding, row, and
//! index, so a transport that deduplicates (Matrix transaction IDs, Discord
//! enforced nonces) collapses the repeat; one that cannot may show it twice.
//! Delivery is at-least-once and never claims exactly-once.
//!
//! A failed send stops the pass in order and schedules a bounded-backoff retry
//! that does not wait for unrelated session activity. Session writes still
//! trigger a pass, so whichever arrives first resumes delivery.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use eidetica::store::DocStore;
use eidetica::{Database, Snapshot};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tracing::{debug, error, info, warn};

use super::render_outbound;
use crate::agent::AgentRegistry;
use crate::session::{EntryType, Session, SessionEntry, TurnRequestId};

/// `chaz_peer` store holding one [`DeliveryProgress`] document per binding.
const DELIVERY_STORE: &str = "transport_delivery";
const DURABLE_RETRY_BASE: Duration = Duration::from_secs(1);
const DURABLE_RETRY_MAX: Duration = Duration::from_secs(60);
/// Discord caps a message nonce at 25 characters; Matrix transaction IDs are
/// opaque. One length serves both.
const IDEMPOTENCY_KEY_LEN: usize = 24;

/// The stable frontend identity delivery progress is scoped to.
///
/// A login belongs to one bridge process, a channel is bound to one session,
/// and the same session may be bound to several channels (fan-out), so the
/// full tuple is what makes progress independent per frontend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeliveryBinding {
    pub transport: String,
    pub login_id: String,
    pub channel: String,
    pub session_db_id: String,
}

impl DeliveryBinding {
    pub fn new(
        transport: impl Into<String>,
        login_id: impl Into<String>,
        channel: impl Into<String>,
        session_db_id: impl Into<String>,
    ) -> Self {
        Self {
            transport: transport.into(),
            login_id: login_id.into(),
            channel: channel.into(),
            session_db_id: session_db_id.into(),
        }
    }

    fn hash_input(&self) -> Vec<u8> {
        let mut input = Vec::new();
        for part in [
            &self.transport,
            &self.login_id,
            &self.channel,
            &self.session_db_id,
        ] {
            input.extend_from_slice(&(part.len() as u64).to_le_bytes());
            input.extend_from_slice(part.as_bytes());
        }
        input
    }

    /// Persisted document key for this binding.
    pub fn key(&self) -> String {
        blake3::hash(&self.hash_input()).to_hex().to_string()
    }

    /// Stable transport idempotency key for one chunk of one row. Identical
    /// across retries, restarts, and reconnects, so a deduplicating transport
    /// suppresses a repeat sent after a lost acknowledgement.
    pub fn idempotency_key(&self, entry_id: &str, chunk_index: usize) -> String {
        let mut input = self.hash_input();
        input.extend_from_slice(&(entry_id.len() as u64).to_le_bytes());
        input.extend_from_slice(entry_id.as_bytes());
        input.extend_from_slice(&(chunk_index as u64).to_le_bytes());
        blake3::hash(&input).to_hex()[..IDEMPOTENCY_KEY_LEN].to_string()
    }
}

/// Acknowledged progress on one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryMark {
    /// This many leading chunks were acknowledged by the transport.
    Chunks(usize),
    /// Every chunk was acknowledged.
    Complete,
}

/// Persisted delivery progress for one [`DeliveryBinding`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeliveryProgress {
    /// Session snapshot at which every agent message then present had been
    /// delivered. `None` means no history has been acknowledged.
    #[serde(default)]
    pub delivered_through: Option<Snapshot>,
    /// Marks for rows delivered after `delivered_through`, keyed by row id.
    #[serde(default)]
    pub entries: BTreeMap<String, DeliveryMark>,
}

/// Reads and writes [`DeliveryProgress`] records in the peer-local database.
#[derive(Clone)]
pub struct DeliveryStore {
    db: Database,
}

impl DeliveryStore {
    /// `db` is the peer-local `chaz_peer` database; it never syncs, which is
    /// what keeps one peer's transport progress from leaking to another.
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn load(
        &self,
        binding: &DeliveryBinding,
    ) -> anyhow::Result<Option<DeliveryProgress>> {
        let txn = self.db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(DELIVERY_STORE).await?;
        match store.get_string(binding.key()).await {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(_) => Ok(None),
        }
    }

    pub async fn save(
        &self,
        binding: &DeliveryBinding,
        progress: &DeliveryProgress,
    ) -> anyhow::Result<()> {
        let txn = self.db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(DELIVERY_STORE).await?;
        store
            .set_string(binding.key(), serde_json::to_string(progress)?)
            .await?;
        txn.commit().await?;
        Ok(())
    }
}

/// One rendered chunk handed to a transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundChunk {
    pub body: String,
    /// See [`DeliveryBinding::idempotency_key`].
    pub idempotency_key: String,
}

type SendFuture = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>;
type SendFn = dyn Fn(OutboundChunk) -> SendFuture + Send + Sync;
type ChunkFn = dyn Fn(&str) -> Vec<String> + Send + Sync;
type BackoffFn = dyn Fn(u32) -> Duration + Send + Sync;

/// Exponential durable-retry delay, bounded so a transport returning after a
/// long outage is noticed promptly. `attempt` is zero-based.
pub fn durable_retry_delay(attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
    DURABLE_RETRY_BASE
        .saturating_mul(factor)
        .min(DURABLE_RETRY_MAX)
}

struct ReconcileState {
    progress: DeliveryProgress,
    /// Row ids present at `progress.delivered_through`, cached so a pass does
    /// not reread the historical view.
    baseline: Option<(Snapshot, HashSet<String>)>,
    retry_attempt: u32,
    retry_scheduled: bool,
}

/// Converges one transport channel to its session's committed agent messages.
///
/// Passes are serialized per binding; concurrent session writes and retry
/// timers cannot interleave sends or double-emit a chunk.
pub struct DeliveryReconciler {
    binding: DeliveryBinding,
    session_db: Database,
    store: DeliveryStore,
    agents: Arc<AgentRegistry>,
    owning_agent: String,
    chunk: Box<ChunkFn>,
    send: Box<SendFn>,
    backoff: Box<BackoffFn>,
    state: Mutex<ReconcileState>,
    stop: watch::Sender<bool>,
}

impl DeliveryReconciler {
    /// Load or seed persisted progress, subscribe to session writes, and run
    /// the first pass so responses committed while this frontend was offline
    /// deliver without waiting for new activity.
    ///
    /// A binding with no record — a channel bound before progress was
    /// persisted — is seeded at the current session snapshot: the history it
    /// already shows is the baseline, and only later writes deliver. Every
    /// later start reconciles against the record instead.
    pub async fn attach(
        session_db: Database,
        store: DeliveryStore,
        binding: DeliveryBinding,
        agents: Arc<AgentRegistry>,
        owning_agent: String,
        chunk: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
        send: impl Fn(OutboundChunk) -> SendFuture + Send + Sync + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        Self::attach_with_backoff(
            session_db,
            store,
            binding,
            agents,
            owning_agent,
            chunk,
            send,
            durable_retry_delay,
        )
        .await
    }

    /// [`Self::attach`] with an explicit retry schedule; tests shorten it.
    #[allow(clippy::too_many_arguments)]
    pub async fn attach_with_backoff(
        session_db: Database,
        store: DeliveryStore,
        binding: DeliveryBinding,
        agents: Arc<AgentRegistry>,
        owning_agent: String,
        chunk: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
        send: impl Fn(OutboundChunk) -> SendFuture + Send + Sync + 'static,
        backoff: impl Fn(u32) -> Duration + Send + Sync + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let progress = match store.load(&binding).await? {
            Some(progress) => progress,
            None => {
                let seeded = DeliveryProgress {
                    delivered_through: Some(session_db.snapshot().await?),
                    entries: BTreeMap::new(),
                };
                store.save(&binding, &seeded).await?;
                info!(
                    transport = %binding.transport,
                    channel = %binding.channel,
                    session_db_id = %binding.session_db_id,
                    "No persisted delivery progress for this binding; baselined at current history"
                );
                seeded
            }
        };
        let (stop, _) = watch::channel(false);
        let this = Arc::new(Self {
            binding,
            session_db: session_db.clone(),
            store,
            agents,
            owning_agent,
            chunk: Box::new(chunk),
            send: Box::new(send),
            backoff: Box::new(backoff),
            state: Mutex::new(ReconcileState {
                progress,
                baseline: None,
                retry_attempt: 0,
                retry_scheduled: false,
            }),
            stop,
        });

        // Subscribe at the cursor the first pass reads from, so a write
        // landing between the pass and the subscription reaches one of them.
        let cursor = session_db.snapshot().await?;
        let weak: Weak<Self> = Arc::downgrade(&this);
        session_db
            .on_write_at_tips(cursor, move |event, _| {
                let weak = weak.clone();
                let source = format!("{:?}", event.source());
                Box::pin(async move {
                    if let Some(reconciler) = weak.upgrade() {
                        debug!(
                            session_db_id = %reconciler.binding.session_db_id,
                            source,
                            "Session write; reconciling the transport"
                        );
                        reconciler.run_pass().await;
                    }
                    Ok(())
                })
            })
            .await?
            .detach();
        this.run_pass().await;
        Ok(this)
    }

    pub fn binding(&self) -> &DeliveryBinding {
        &self.binding
    }

    /// Cancel any pending durable retry. Session writes still reach this
    /// reconciler until its database handle's instance is dropped.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    /// Run one pass and, if anything is still undelivered, schedule a retry.
    pub async fn run_pass(self: &Arc<Self>) {
        let outcome = self.reconcile_once().await;
        let mut state = self.state.lock().await;
        match outcome {
            Ok(true) => state.retry_attempt = 0,
            Ok(false) => self.schedule_retry(&mut state),
            Err(error) => {
                error!(
                    session_db_id = %self.binding.session_db_id,
                    channel = %self.binding.channel,
                    %error,
                    "Delivery pass failed"
                );
                self.schedule_retry(&mut state);
            }
        }
    }

    fn schedule_retry(self: &Arc<Self>, state: &mut ReconcileState) {
        if state.retry_scheduled || *self.stop.borrow() {
            return;
        }
        // A torn-down connection takes its subscriptions with it; the
        // replacement generation attaches afresh, so this one stops here.
        if self.session_db.instance().is_err() {
            debug!(
                session_db_id = %self.binding.session_db_id,
                "Instance gone; not scheduling a delivery retry"
            );
            return;
        }
        state.retry_scheduled = true;
        let delay = (self.backoff)(state.retry_attempt);
        state.retry_attempt = state.retry_attempt.saturating_add(1);
        let this = self.clone();
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = stop.changed() => return,
            }
            this.state.lock().await.retry_scheduled = false;
            this.run_pass().await;
        });
    }

    /// Deliver every pending row in order. `Ok(true)` when the channel is
    /// converged; `Ok(false)` when a send failed and the tail remains.
    async fn reconcile_once(&self) -> anyhow::Result<bool> {
        let mut state = self.state.lock().await;
        let snapshot = self.session_db.snapshot().await?;
        let rows = Session::entries_with_ids_at_snapshot(&self.session_db, &snapshot).await?;
        let baseline = self.baseline_ids(&mut state).await?;

        let pending: Vec<&(TurnRequestId, SessionEntry)> = rows
            .iter()
            .filter(|(id, entry)| {
                entry.entry_type == EntryType::Message
                    && self.agents.get(&entry.sender).is_some()
                    && !baseline.contains(id.as_str())
                    && state.progress.entries.get(id.as_str()) != Some(&DeliveryMark::Complete)
            })
            .collect();

        for (id, entry) in pending {
            let body = render_outbound(&self.owning_agent, &entry.sender, &entry.content);
            let chunks = (self.chunk)(&body);
            let start = match state.progress.entries.get(id.as_str()) {
                Some(DeliveryMark::Chunks(count)) => *count,
                _ => 0,
            };
            let total = chunks.len();
            for (index, body) in chunks.into_iter().enumerate().skip(start) {
                let chunk = OutboundChunk {
                    body,
                    idempotency_key: self.binding.idempotency_key(id.as_str(), index),
                };
                if let Err(error) = (self.send)(chunk).await {
                    warn!(
                        session_db_id = %self.binding.session_db_id,
                        channel = %self.binding.channel,
                        entry = %id,
                        chunk = index,
                        %error,
                        "Transport send failed; leaving this entry and its tail for a retry"
                    );
                    return Ok(false);
                }
                let mark = if index + 1 == total {
                    DeliveryMark::Complete
                } else {
                    DeliveryMark::Chunks(index + 1)
                };
                state.progress.entries.insert(id.as_str().to_string(), mark);
                self.store.save(&self.binding, &state.progress).await?;
            }
            if total == 0 {
                state
                    .progress
                    .entries
                    .insert(id.as_str().to_string(), DeliveryMark::Complete);
                self.store.save(&self.binding, &state.progress).await?;
            }
        }

        // Everything at `snapshot` is delivered. Fold the marks into the
        // snapshot so the record stays small; skip the write when there is
        // nothing to fold.
        if !state.progress.entries.is_empty() {
            state.progress = DeliveryProgress {
                delivered_through: Some(snapshot.clone()),
                entries: BTreeMap::new(),
            };
            self.store.save(&self.binding, &state.progress).await?;
            state.baseline = Some((
                snapshot,
                rows.iter().map(|(id, _)| id.as_str().to_string()).collect(),
            ));
        }
        Ok(true)
    }

    async fn baseline_ids(&self, state: &mut ReconcileState) -> anyhow::Result<HashSet<String>> {
        let Some(through) = state.progress.delivered_through.clone() else {
            return Ok(HashSet::new());
        };
        if let Some((cached, ids)) = &state.baseline
            && *cached == through
        {
            return Ok(ids.clone());
        }
        let ids: HashSet<String> =
            Session::entries_with_ids_at_snapshot(&self.session_db, &through)
                .await?
                .into_iter()
                .map(|(id, _)| id.as_str().to_string())
                .collect();
        state.baseline = Some((through, ids.clone()));
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::EntryType;
    use crate::types::ConversationId;
    use chrono::Utc;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;
    use eidetica::{Instance, NewUser};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        _instance: Instance,
        session_db: Database,
        store: DeliveryStore,
        agents: Arc<AgentRegistry>,
    }

    async fn fixture() -> Fixture {
        let (instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("peer"))
                .await
                .unwrap();
        let key = user.get_default_key().unwrap();
        let session_db = user.create_database(Doc::new(), &key).await.unwrap();
        let peer_db = user.create_database(Doc::new(), &key).await.unwrap();
        Fixture {
            _instance: instance,
            session_db,
            store: DeliveryStore::new(peer_db),
            agents: Arc::new(AgentRegistry::with_default_agent()),
        }
    }

    async fn write(db: &Database, sender: &str, content: &str) -> TurnRequestId {
        let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        session
            .add_entry(SessionEntry {
                sender: sender.into(),
                content: content.into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap()
    }

    fn binding(channel: &str, db: &Database) -> DeliveryBinding {
        DeliveryBinding::new("test", "login", channel, db.root_id().to_string())
    }

    /// A transport that records acknowledged bodies and fails the bodies
    /// named in `fail` until they are removed from it.
    #[derive(Default)]
    struct Transport {
        sent: Mutex<Vec<OutboundChunk>>,
        fail: Mutex<HashSet<String>>,
        attempts: AtomicUsize,
    }

    impl Transport {
        fn send_fn(
            self: &Arc<Self>,
        ) -> impl Fn(OutboundChunk) -> SendFuture + Send + Sync + 'static {
            let this = self.clone();
            move |chunk| {
                let this = this.clone();
                Box::pin(async move {
                    this.attempts.fetch_add(1, Ordering::SeqCst);
                    if this.fail.lock().await.contains(&chunk.body) {
                        anyhow::bail!("simulated transport failure for {:?}", chunk.body);
                    }
                    this.sent.lock().await.push(chunk);
                    Ok(())
                })
            }
        }

        async fn bodies(&self) -> Vec<String> {
            self.sent
                .lock()
                .await
                .iter()
                .map(|c| c.body.clone())
                .collect()
        }
    }

    fn one_chunk(body: &str) -> Vec<String> {
        vec![body.to_string()]
    }

    fn pipe_chunks(body: &str) -> Vec<String> {
        body.split('|').map(str::to_string).collect()
    }

    async fn attach(
        fx: &Fixture,
        channel: &str,
        transport: &Arc<Transport>,
        chunk: fn(&str) -> Vec<String>,
    ) -> Arc<DeliveryReconciler> {
        DeliveryReconciler::attach_with_backoff(
            fx.session_db.clone(),
            fx.store.clone(),
            binding(channel, &fx.session_db),
            fx.agents.clone(),
            "default".into(),
            chunk,
            transport.send_fn(),
            |_| Duration::from_millis(20),
        )
        .await
        .unwrap()
    }

    async fn wait_until(what: &str, check: impl AsyncFn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !check().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    #[tokio::test]
    async fn a_response_committed_while_offline_delivers_on_reattach() {
        let fx = fixture().await;
        write(&fx.session_db, "user", "hello").await;
        write(&fx.session_db, "default", "shown already").await;

        // First attach: no record, so existing history is the baseline.
        let transport = Arc::new(Transport::default());
        let reconciler = attach(&fx, "room", &transport, one_chunk).await;
        assert!(transport.bodies().await.is_empty());
        reconciler.stop();
        drop(reconciler);

        // The executor answers while this frontend is gone.
        write(&fx.session_db, "default", "answered offline").await;
        write(&fx.session_db, "user", "still there?").await;

        // Reattach: persisted progress says the answer is undelivered.
        let transport = Arc::new(Transport::default());
        let reconciler = attach(&fx, "room", &transport, one_chunk).await;
        assert_eq!(transport.bodies().await, vec!["answered offline"]);

        // Nothing repeats on a further pass, and later writes still deliver.
        reconciler.run_pass().await;
        assert_eq!(transport.bodies().await, vec!["answered offline"]);
        write(&fx.session_db, "default", "and again").await;
        wait_until("second delivery", async || {
            transport.bodies().await.len() == 2
        })
        .await;
        assert_eq!(
            transport.bodies().await,
            vec!["answered offline", "and again"]
        );
        reconciler.stop();
    }

    #[tokio::test]
    async fn a_failed_send_retries_on_the_timer_without_a_database_write() {
        let fx = fixture().await;
        let transport = Arc::new(Transport::default());
        let reconciler = attach(&fx, "room", &transport, one_chunk).await;
        transport.fail.lock().await.insert("blocked".into());
        write(&fx.session_db, "default", "blocked").await;
        wait_until("first failed attempt", async || {
            transport.attempts.load(Ordering::SeqCst) >= 1
        })
        .await;
        assert!(transport.bodies().await.is_empty());
        let progress = fx.store.load(reconciler.binding()).await.unwrap().unwrap();
        assert!(progress.entries.is_empty(), "a failed send is never marked");

        // No further session write: the durable retry alone must deliver.
        transport.fail.lock().await.clear();
        wait_until("timer retry", async || !transport.bodies().await.is_empty()).await;
        assert_eq!(transport.bodies().await, vec!["blocked"]);
        assert!(transport.attempts.load(Ordering::SeqCst) >= 2);
        reconciler.stop();
    }

    #[tokio::test]
    async fn a_partial_chunk_prefix_survives_restart() {
        let fx = fixture().await;
        let transport = Arc::new(Transport::default());
        transport.fail.lock().await.insert("second".into());
        let reconciler = attach(&fx, "room", &transport, pipe_chunks).await;
        reconciler.stop(); // no timer retry: the restart is what resumes
        let id = write(&fx.session_db, "default", "first|second|third").await;
        wait_until("first chunk", async || !transport.bodies().await.is_empty()).await;
        // Let the failing second chunk settle before inspecting the record.
        wait_until("second chunk attempted", async || {
            transport.attempts.load(Ordering::SeqCst) >= 2
        })
        .await;
        assert_eq!(transport.bodies().await, vec!["first"]);
        let progress = fx.store.load(reconciler.binding()).await.unwrap().unwrap();
        assert_eq!(
            progress.entries.get(id.as_str()),
            Some(&DeliveryMark::Chunks(1))
        );
        let first_key = transport.sent.lock().await[0].idempotency_key.clone();
        drop(reconciler);

        // A fresh process resumes at the failed chunk, never the prefix.
        let transport = Arc::new(Transport::default());
        let reconciler = attach(&fx, "room", &transport, pipe_chunks).await;
        assert_eq!(transport.bodies().await, vec!["second", "third"]);
        let progress = fx.store.load(reconciler.binding()).await.unwrap().unwrap();
        assert!(
            progress.entries.is_empty(),
            "a converged pass folds marks into the snapshot"
        );
        assert!(progress.delivered_through.is_some());
        assert_ne!(
            transport.sent.lock().await[0].idempotency_key,
            first_key,
            "each chunk carries its own key"
        );
        reconciler.stop();
    }

    #[tokio::test]
    async fn a_crash_between_send_and_acknowledgement_repeats_with_the_same_key() {
        let fx = fixture().await;
        let transport = Arc::new(Transport::default());
        let reconciler = attach(&fx, "room", &transport, one_chunk).await;
        let before = fx.store.load(reconciler.binding()).await.unwrap().unwrap();
        write(&fx.session_db, "default", "sent then crashed").await;
        wait_until("send", async || !transport.bodies().await.is_empty()).await;
        let first_key = transport.sent.lock().await[0].idempotency_key.clone();
        reconciler.stop();
        drop(reconciler);

        // Simulate the crash window: the transport saw the chunk, but the
        // acknowledgement never reached the record.
        fx.store
            .save(&binding("room", &fx.session_db), &before)
            .await
            .unwrap();

        let reconciler = attach(&fx, "room", &transport, one_chunk).await;
        let keys: Vec<String> = transport
            .sent
            .lock()
            .await
            .iter()
            .map(|c| c.idempotency_key.clone())
            .collect();
        assert_eq!(
            keys,
            vec![first_key.clone(), first_key],
            "the repeat is the documented duplicate window and carries the same key for the transport to collapse"
        );
        reconciler.stop();
    }

    #[tokio::test]
    async fn frontends_sharing_a_login_keep_independent_progress() {
        let fx = fixture().await;
        let matrix = Arc::new(Transport::default());
        let discord = Arc::new(Transport::default());
        let matrix_side = attach(&fx, "matrix-room", &matrix, one_chunk).await;
        let discord_side = attach(&fx, "discord-channel", &discord, one_chunk).await;
        discord.fail.lock().await.insert("fan out".into());
        discord_side.stop();

        write(&fx.session_db, "default", "fan out").await;
        wait_until("matrix delivery", async || {
            !matrix.bodies().await.is_empty()
        })
        .await;
        wait_until("discord attempt", async || {
            discord.attempts.load(Ordering::SeqCst) >= 1
        })
        .await;
        assert_eq!(matrix.bodies().await, vec!["fan out"]);
        assert!(discord.bodies().await.is_empty());

        let matrix_progress = fx.store.load(matrix_side.binding()).await.unwrap().unwrap();
        let discord_progress = fx
            .store
            .load(discord_side.binding())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(matrix_progress, discord_progress);
        assert!(discord_progress.entries.is_empty());

        // The Discord side recovers on its own once its transport does.
        discord.fail.lock().await.clear();
        drop(discord_side);
        let discord_side = attach(&fx, "discord-channel", &discord, one_chunk).await;
        assert_eq!(discord.bodies().await, vec!["fan out"]);
        assert_eq!(matrix.bodies().await, vec!["fan out"]);
        matrix_side.stop();
        discord_side.stop();
    }

    #[tokio::test]
    async fn guest_agents_are_prefixed_and_humans_are_never_echoed() {
        let fx = fixture().await;
        let transport = Arc::new(Transport::default());
        let reconciler = attach(&fx, "room", &transport, one_chunk).await;
        write(&fx.session_db, "user", "question").await;
        write(&fx.session_db, "default", "answer").await;
        wait_until("delivery", async || transport.bodies().await.len() == 1).await;
        assert_eq!(transport.bodies().await, vec!["answer"]);
        reconciler.stop();
    }

    #[test]
    fn idempotency_keys_are_stable_and_transport_safe() {
        let binding = DeliveryBinding::new("matrix", "@chaz:example", "!room:example", "sha256:x");
        let key = binding.idempotency_key("row-1", 0);
        assert_eq!(key, binding.idempotency_key("row-1", 0));
        assert_ne!(key, binding.idempotency_key("row-1", 1));
        assert_ne!(key, binding.idempotency_key("row-2", 0));
        assert_ne!(
            key,
            DeliveryBinding::new("discord", "@chaz:example", "!room:example", "sha256:x")
                .idempotency_key("row-1", 0)
        );
        assert_eq!(key.len(), IDEMPOTENCY_KEY_LEN);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn durable_retry_is_bounded() {
        assert_eq!(durable_retry_delay(0), Duration::from_secs(1));
        assert_eq!(durable_retry_delay(3), Duration::from_secs(8));
        assert_eq!(durable_retry_delay(40), DURABLE_RETRY_MAX);
    }

    #[test]
    fn progress_round_trips_through_json() {
        let progress = DeliveryProgress {
            delivered_through: Some(Snapshot::EMPTY),
            entries: BTreeMap::from([
                ("a".to_string(), DeliveryMark::Chunks(2)),
                ("b".to_string(), DeliveryMark::Complete),
            ]),
        };
        let raw = serde_json::to_string(&progress).unwrap();
        assert_eq!(
            serde_json::from_str::<DeliveryProgress>(&raw).unwrap(),
            progress
        );
    }
}
