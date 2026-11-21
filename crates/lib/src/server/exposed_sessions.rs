//! Discover bridge-exposed sessions from each hosted Agent DB and adopt them.
//!
//! A bridge creates a session DB, attaches a daemon-hosted agent to it and
//! records a [`SessionRef`](crate::agent_db::SessionRef) with a non-empty
//! `exposed_on` in that agent's registry. The registry is the durable truth for
//! which sessions this daemon should run; this module turns changes to it into
//! session registrations.
//!
//! Two halves, kept apart on purpose:
//!
//! - **Discovery** — one watcher per hosted Agent DB. It subscribes from a
//!   snapshot, then reads that agent's exposure list, and re-reads it after every
//!   later write to *that* DB. A write never causes a scan of any other agent.
//! - **Acquisition** — one coordinator for the whole daemon. It diffs each
//!   agent's exposure list against the last one it saw, so only a newly exposed
//!   session starts work. A session whose tree is already local registers at
//!   once; a missing one gets an immediate first pull under bounded concurrency,
//!   then retries on a single capped-backoff schedule that later registry writes
//!   neither repeat nor reset.
//!
//! Once registered, a session is watched individually by
//! [`Server::watch_session`](super::Server::watch_session); the Agent DB watch
//! only selects which session DBs to adopt.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::ops::Bound::{Excluded, Unbounded};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use eidetica::Snapshot;
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::agent_db::AgentDb;
use crate::hosted_index::DbEntry;

// Keep local registration independent of the network bound. A missing session
// can spend a full peer deadline in a pull; a ready one need not wait for it.
const MAX_CONCURRENT_PULLS: usize = 4;
const MAX_CONCURRENT_LOCAL: usize = 8;

#[derive(Clone, Copy)]
pub(super) struct RetryPolicy {
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(5),
            cap: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    fn delay(self, failures: u32) -> Duration {
        self.base
            .saturating_mul(1 << failures.saturating_sub(1).min(16))
            .min(self.cap)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Exposer {
    pub entry: DbEntry,
    pub adb: AgentDb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LocalOutcome {
    Registered,
    Missing,
    // Present but not ready; a new pull would not help.
    Retry,
    NotAttached,
    Revoked,
    Denied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PullOutcome {
    Pulled,
    Unavailable,
    Denied,
}

pub(super) trait Acquire: Send + Sync + 'static {
    fn exposed(
        &self,
        exposer: &Exposer,
        snapshot: &Snapshot,
    ) -> impl Future<Output = anyhow::Result<BTreeSet<String>>> + Send;
    fn register_local(
        &self,
        exposer: &Exposer,
        session: &str,
    ) -> impl Future<Output = LocalOutcome> + Send;
    fn pull(&self, exposer: &Exposer, session: &str) -> impl Future<Output = PullOutcome> + Send;
}

struct ExposureDelta {
    agent: Arc<Exposer>,
    added: Vec<String>,
    removed: Vec<String>,
}

#[derive(PartialEq, Eq)]
enum SlotState {
    Running,
    Waiting {
        at: Instant,
        agent: eidetica::entry::ID,
    },
    Registered,
    Denied(eidetica::entry::ID),
}

struct Slot {
    agents: BTreeMap<eidetica::entry::ID, Arc<Exposer>>,
    failures: u32,
    state: SlotState,
}

type Completion = (String, eidetica::entry::ID, LocalOutcome);

struct Coordinator<A> {
    acquirer: Arc<A>,
    policy: RetryPolicy,
    local: Arc<Semaphore>,
    network: Arc<Semaphore>,
    slots: HashMap<String, Slot>,
    retries: BTreeSet<(Instant, String)>,
    // One polled future per pending ID, not a spawned task or timer per ID.
    attempts: FuturesUnordered<BoxFuture<'static, Completion>>,
}

impl<A: Acquire> Coordinator<A> {
    fn new(acquirer: Arc<A>, policy: RetryPolicy) -> Self {
        Self {
            acquirer,
            policy,
            local: Arc::new(Semaphore::new(MAX_CONCURRENT_LOCAL)),
            network: Arc::new(Semaphore::new(MAX_CONCURRENT_PULLS)),
            slots: HashMap::new(),
            retries: BTreeSet::new(),
            attempts: FuturesUnordered::new(),
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<ExposureDelta>) {
        loop {
            let next = self.retries.first().map(|(at, _)| *at);
            tokio::select! {
                update = rx.recv() => match update {
                    Some(update) => self.apply(update),
                    None => break,
                },
                Some(result) = self.attempts.next() => self.finish(result),
                _ = sleep_until(next) => self.start_due(),
            }
        }
    }

    fn apply(&mut self, delta: ExposureDelta) {
        let agent_id = &delta.agent.entry.db_id;
        for session in delta.removed {
            let Some(slot) = self.slots.get_mut(&session) else {
                continue;
            };
            slot.agents.remove(agent_id);
            match slot.state {
                // Keep a running attempt until it drains even if the last
                // exposer left. Register/pull consult the durable list again;
                // a revoke then re-expose cannot start an overlapping attempt.
                SlotState::Running => continue,
                SlotState::Waiting { at, .. } if slot.agents.is_empty() => {
                    self.retries.remove(&(at, session.clone()));
                }
                SlotState::Denied(ref actor) if actor == agent_id && !slot.agents.is_empty() => {
                    self.start(session);
                    continue;
                }
                _ if !slot.agents.is_empty() => continue,
                _ => {}
            }
            self.slots.remove(&session);
        }
        for session in delta.added {
            match self.slots.entry(session.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry
                        .get_mut()
                        .agents
                        .insert(agent_id.clone(), delta.agent.clone());
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Slot {
                        agents: BTreeMap::from([(agent_id.clone(), delta.agent.clone())]),
                        state: SlotState::Running,
                        failures: 0,
                    });
                    self.start(session);
                }
            }
        }
    }

    fn start(&mut self, session: String) {
        let slot = self
            .slots
            .get_mut(&session)
            .expect("one slot per pending session");
        let next = match &slot.state {
            SlotState::Waiting { agent, .. } => slot.agents.get_key_value(agent),
            _ => None,
        };
        let agent = next
            .or_else(|| slot.agents.first_key_value())
            .expect("pending session has an exposer")
            .1
            .clone();
        slot.state = SlotState::Running;
        let acquirer = self.acquirer.clone();
        let local = self.local.clone();
        let network = self.network.clone();
        self.attempts.push(Box::pin(async move {
            let result = AssertUnwindSafe(attempt(&*acquirer, &local, &network, &agent, &session))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    warn!(session_db_id = %session, "Exposed-session acquisition panicked");
                    LocalOutcome::Retry
                });
            (session, agent.entry.db_id.clone(), result)
        }));
    }

    fn finish(&mut self, (session, agent, result): Completion) {
        let slot = self
            .slots
            .get_mut(&session)
            .expect("running attempt retains its slot");
        if slot.agents.is_empty() {
            self.slots.remove(&session);
            return;
        }
        // An obsolete attempt cannot mark a different exposer denied. Pick a
        // current one without waiting for another registry write.
        if result != LocalOutcome::Registered && !slot.agents.contains_key(&agent) {
            self.start(session);
            return;
        }
        match result {
            LocalOutcome::Registered => slot.state = SlotState::Registered,
            LocalOutcome::Denied => slot.state = SlotState::Denied(agent),
            LocalOutcome::Missing
            | LocalOutcome::Retry
            | LocalOutcome::NotAttached
            | LocalOutcome::Revoked => {
                slot.failures = slot.failures.saturating_add(1);
                let delay = self.policy.delay(slot.failures);
                let at = Instant::now() + delay;
                // Try another current exposer only when the verified local
                // session shows this agent is not attached. An opaque peer
                // miss is not permission to try a different signing key.
                let agent = if result == LocalOutcome::NotAttached {
                    slot.agents
                        .range((Excluded(&agent), Unbounded))
                        .next()
                        .or_else(|| slot.agents.first_key_value())
                        .expect("retrying slot has an exposer")
                        .0
                        .clone()
                } else {
                    agent
                };
                slot.state = SlotState::Waiting { at, agent };
                debug!(session_db_id = %session, failures = slot.failures, ?delay, "Retrying exposed-session acquisition later");
                self.retries.insert((at, session));
            }
        }
    }

    fn start_due(&mut self) {
        let now = Instant::now();
        while self.retries.first().is_some_and(|(at, _)| *at <= now) {
            let (_, session) = self.retries.pop_first().expect("due retry");
            self.start(session);
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn attempt<A: Acquire>(
    acquirer: &A,
    local: &Semaphore,
    network: &Semaphore,
    exposer: &Exposer,
    session: &str,
) -> LocalOutcome {
    let first = {
        let _permit = local.acquire().await;
        acquirer.register_local(exposer, session).await
    };
    if first != LocalOutcome::Missing {
        return first;
    }
    let pulled = {
        let _permit = network.acquire().await;
        acquirer.pull(exposer, session).await
    };
    match pulled {
        PullOutcome::Pulled => {
            let _permit = local.acquire().await;
            acquirer.register_local(exposer, session).await
        }
        PullOutcome::Unavailable => LocalOutcome::Retry,
        PullOutcome::Denied => LocalOutcome::Denied,
    }
}

/// Keep this agent's own authoritative list at a verified snapshot. Write
/// events bracket the DAG, not the rows; neither embedded registration nor
/// service subscription necessarily replays a write before it became live.
async fn watch_agent<A: Acquire>(
    acquirer: Arc<A>,
    agent: Arc<Exposer>,
    tx: mpsc::Sender<ExposureDelta>,
) -> anyhow::Result<()> {
    let wake = Arc::new(Notify::new());
    let db = agent.adb.database();
    let initial = db.snapshot().await?;
    let first = acquirer.exposed(&agent, &initial).await;
    let _subscription = db
        .on_write_at_tips(initial.clone(), {
            let wake = wake.clone();
            move |_, _| {
                wake.notify_one();
                std::future::ready(Ok(()))
            }
        })
        .await?;

    let mut first = Some(first);
    let mut previous = BTreeSet::new();
    let mut observed = None;
    loop {
        // Always recheck once after subscribing. Subsequent wakes can be
        // coalesced and their post_tips are raw, so read the verified head.
        let read = async {
            let tips = db.snapshot().await?;
            if observed.as_ref() == Some(&tips) {
                return Ok(None);
            }
            let sessions = match first.take() {
                Some(Ok(sessions)) if tips == initial => sessions,
                _ => acquirer.exposed(&agent, &tips).await?,
            };
            Ok::<_, anyhow::Error>(Some((tips, sessions)))
        }
        .await;
        match read {
            Ok(update) => {
                if let Some((tips, sessions)) = update {
                    let added: Vec<_> = sessions.difference(&previous).cloned().collect();
                    let removed: Vec<_> = previous.difference(&sessions).cloned().collect();
                    if (!added.is_empty() || !removed.is_empty())
                        && tx
                            .send(ExposureDelta {
                                agent: agent.clone(),
                                added,
                                removed,
                            })
                            .await
                            .is_err()
                    {
                        return Ok(());
                    }
                    previous = sessions;
                    observed = Some(tips);
                }
                wake.notified().await;
            }
            Err(error) => {
                // A failed read is not a revocation; retry even if no other
                // write follows it. This is per Agent DB, not per session.
                warn!(agent = %agent.entry.display_name, %error, "Could not read session exposures");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Own all discovery work in one server-tracked task. Dropping it drops the
/// coordinator's attempts and the watcher futures, including subscriptions.
pub(super) fn spawn<A: Acquire>(
    acquirer: Arc<A>,
    exposers: Vec<Exposer>,
    policy: RetryPolicy,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    // Capture the notification generation before returning to the caller.
    let shutdown = shutdown.notified_owned();
    tokio::spawn(async move {
        let (tx, rx) = mpsc::channel(exposers.len().max(1));
        let watchers: FuturesUnordered<_> = exposers.into_iter().map(|agent| {
            let tx = tx.clone();
            let acquirer = acquirer.clone();
            async move {
                let agent = Arc::new(agent);
                if let Err(error) = watch_agent(acquirer, agent.clone(), tx).await {
                    warn!(agent = %agent.entry.display_name, %error, "Could not watch agent registry");
                }
            }
        }).collect();
        drop(tx);
        tokio::select! {
            biased;
            _ = shutdown => {},
            _ = futures::future::join(watchers.collect::<()>(), Coordinator::new(acquirer, policy).run(rx)) => {},
        }
    })
}
