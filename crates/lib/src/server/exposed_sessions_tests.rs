//! Exposed-session discovery and acquisition against a real registry.
//!
//! The production acquirer does the real work; [`Gated`] only decides which
//! sessions look unavailable and counts what each agent and session costs.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use super::Server;
use super::build::RegistryAcquirer;
use super::exposed_sessions::{Acquire, Exposer, LocalOutcome, PullOutcome, RetryPolicy, spawn};
use crate::agent_db::SessionRef;
use crate::hosted_index::DbEntry;
use crate::session::SessionRegistry;

#[derive(Default)]
struct Counts {
    reads: HashMap<String, usize>,
    locals: HashMap<String, usize>,
    pulls: HashMap<String, usize>,
}

struct Gated {
    inner: RegistryAcquirer,
    /// Sessions that look absent locally and that no peer can serve.
    unavailable: Mutex<HashSet<String>>,
    /// Sessions whose pull is refused by policy.
    denied: Mutex<HashSet<String>>,
    /// When set, a pull waits for this before answering.
    hold_pull: Mutex<Option<Arc<Notify>>>,
    pull_started: Notify,
    initial_read: Notify,
    hold_initial_read: Mutex<Option<Arc<Notify>>>,
    counts: Mutex<Counts>,
}

impl Gated {
    fn new(inner: RegistryAcquirer) -> Self {
        Self {
            inner,
            unavailable: Mutex::default(),
            denied: Mutex::default(),
            hold_pull: Mutex::default(),
            pull_started: Notify::new(),
            initial_read: Notify::new(),
            hold_initial_read: Mutex::default(),
            counts: Mutex::default(),
        }
    }
    fn reads(&self, agent: &DbEntry) -> usize {
        let counts = self.counts.lock().unwrap();
        counts
            .reads
            .get(&agent.db_id.to_string())
            .copied()
            .unwrap_or(0)
    }
    fn locals(&self, session: &str) -> usize {
        let counts = self.counts.lock().unwrap();
        counts.locals.get(session).copied().unwrap_or(0)
    }
    fn pulls(&self, session: &str) -> usize {
        let counts = self.counts.lock().unwrap();
        counts.pulls.get(session).copied().unwrap_or(0)
    }
}

impl Acquire for Gated {
    async fn exposed(
        &self,
        exposer: &Exposer,
        snapshot: &eidetica::Snapshot,
    ) -> anyhow::Result<BTreeSet<String>> {
        *self
            .counts
            .lock()
            .unwrap()
            .reads
            .entry(exposer.entry.db_id.to_string())
            .or_default() += 1;
        let result = self.inner.exposed(exposer, snapshot).await;
        let hold = self.hold_initial_read.lock().unwrap().take();
        if let Some(hold) = hold {
            self.initial_read.notify_one();
            hold.notified().await;
        }
        result
    }

    async fn register_local(&self, exposer: &Exposer, session: &str) -> LocalOutcome {
        *self
            .counts
            .lock()
            .unwrap()
            .locals
            .entry(session.to_string())
            .or_default() += 1;
        if self.unavailable.lock().unwrap().contains(session) {
            return LocalOutcome::Missing;
        }
        self.inner.register_local(exposer, session).await
    }

    async fn pull(&self, _exposer: &Exposer, session: &str) -> PullOutcome {
        *self
            .counts
            .lock()
            .unwrap()
            .pulls
            .entry(session.to_string())
            .or_default() += 1;
        self.pull_started.notify_one();
        let hold = self.hold_pull.lock().unwrap().clone();
        if let Some(hold) = hold {
            hold.notified().await;
        }
        if self.denied.lock().unwrap().contains(session) {
            return PullOutcome::Denied;
        }
        if self.unavailable.lock().unwrap().contains(session) {
            return PullOutcome::Unavailable;
        }
        PullOutcome::Pulled
    }
}

const POLICY: RetryPolicy = RetryPolicy {
    base: Duration::from_millis(200),
    cap: Duration::from_millis(400),
};

struct Fixture {
    server: Arc<Server>,
    registry: Arc<SessionRegistry>,
    instance: eidetica::Instance,
}

async fn fixture() -> Fixture {
    let (instance, server, registry) = super::tests::server_fixture_backend(
        Arc::new(crate::tool_host::NativeToolHost::new()),
        Arc::new(crate::test_support::context_hub().await),
        None,
    )
    .await;
    Fixture {
        server,
        registry,
        instance,
    }
}

impl Fixture {
    async fn agent(&self, name: &str) -> Exposer {
        let entry = crate::session::test_helpers::make_agent_entry(&self.registry, name).await;
        self.server.agent_index().register(entry.clone());
        let adb = self
            .registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
            .unwrap()
            .unwrap();
        Exposer { entry, adb }
    }

    async fn exposed_session(&self, agent: &Exposer) -> String {
        let (_c, sdb) = self.registry.create_session(Some("exposed")).await.unwrap();
        let id = sdb.root_id().to_string();
        self.registry
            .attach_agent_to_session(&id, &agent.entry)
            .await
            .unwrap();
        agent.adb.expose_session_on(&id, "matrix").await.unwrap();
        id
    }

    async fn acquirer(&self) -> Arc<Gated> {
        let secrets = crate::security::SecretStore::new(self.registry.chaz_peer().clone()).await;
        Arc::new(Gated::new(RegistryAcquirer {
            server: self.server.clone(),
            registry: self.registry.clone(),
            default_backend: crate::backends::BackendManager::new(&None, secrets),
        }))
    }

    fn claimed(&self, session: &str) -> bool {
        self.server.runtime_owner_of(session).is_some()
    }
}

async fn eventually(what: &str, within: Duration, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(within, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

/// An unrelated agent DB write. It changes the registry without changing what
/// it exposes.
async fn noise(agent: &Exposer, n: usize) {
    agent
        .adb
        .register_session_ref(SessionRef {
            session_db_id: format!("noise-{n}"),
            exposed_on: Vec::new(),
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unavailable_session_under_write_storm_leaves_others_prompt() {
    let f = fixture().await;
    let alpha = f.agent("alpha").await;
    let beta = f.agent("beta").await;
    let stuck = f.exposed_session(&alpha).await;
    let acquirer = f.acquirer().await;
    acquirer.unavailable.lock().unwrap().insert(stuck.clone());
    let blocked = Arc::new(Notify::new());
    *acquirer.hold_pull.lock().unwrap() = Some(blocked.clone());
    let shutdown = Arc::new(Notify::new());
    let task = spawn(
        acquirer.clone(),
        vec![alpha.clone(), beta.clone()],
        POLICY,
        shutdown.clone(),
    );

    // A missing session gets its first pull straight away.
    eventually(
        "first pull of the missing session",
        Duration::from_secs(5),
        || acquirer.pulls(&stuck) >= 1,
    )
    .await;
    eventually("beta's initial read", Duration::from_secs(5), || {
        acquirer.reads(&beta.entry) >= 1
    })
    .await;
    let beta_reads = acquirer.reads(&beta.entry);

    // Keep A's pull outstanding while B registers locally. A's later
    // failures must still back off through continuing writes to its agent.
    let storm = std::time::Instant::now();
    let alpha_reads = acquirer.reads(&alpha.entry);
    let noisy = alpha.clone();
    let writes = tokio::spawn(async move {
        for n in 0..40 {
            noise(&noisy, n).await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    });
    eventually(
        "alpha's writes are observed",
        Duration::from_secs(5),
        || acquirer.reads(&alpha.entry) > alpha_reads,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        acquirer.reads(&beta.entry),
        beta_reads,
        "alpha's storm must not rescan beta"
    );
    // A session newly exposed on another agent mid-storm registers from local
    // data without touching the network.
    let fresh = f.exposed_session(&beta).await;
    eventually(
        "newly exposed local session registers",
        Duration::from_secs(5),
        || f.claimed(&fresh),
    )
    .await;
    assert_eq!(
        acquirer.pulls(&stuck),
        1,
        "no overlap even while another session registers"
    );
    assert_eq!(acquirer.pulls(&fresh), 0, "a local session never dials");
    assert_eq!(
        acquirer.locals(&fresh),
        1,
        "registered in exactly one attempt"
    );

    *acquirer.hold_pull.lock().unwrap() = None;
    blocked.notify_one();
    writes.await.unwrap();
    eventually("a real retry fires", Duration::from_secs(3), || {
        acquirer.pulls(&stuck) >= 2
    })
    .await;
    // Pulls for the stuck session follow the retry schedule, not the storm.
    let elapsed = storm.elapsed();
    let allowed = 1 + (elapsed.as_millis() / POLICY.base.as_millis()) as usize;
    assert!(
        acquirer.pulls(&stuck) <= allowed,
        "pulls {} exceed backoff bound {allowed} over {elapsed:?}",
        acquirer.pulls(&stuck)
    );
    assert!(!f.claimed(&stuck));

    // The tree arrives by some other route. No registry write announces it;
    // the scheduled retry finds it.
    acquirer.unavailable.lock().unwrap().remove(&stuck);
    eventually(
        "stuck session recovers on its retry",
        Duration::from_secs(3),
        || f.claimed(&stuck),
    )
    .await;

    // Registered sessions are done: further writes start no attempts.
    let (stuck_locals, fresh_locals) = (acquirer.locals(&stuck), acquirer.locals(&fresh));
    for n in 60..70 {
        noise(&alpha, n).await;
        noise(&beta, n).await;
    }
    tokio::time::sleep(POLICY.cap * 2).await;
    assert_eq!(acquirer.locals(&stuck), stuck_locals);
    assert_eq!(acquirer.locals(&fresh), fresh_locals);

    shutdown.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("discovery stops on shutdown")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_exposure_is_never_registered_late() {
    let f = fixture().await;
    let alpha = f.agent("alpha").await;
    let session = f.exposed_session(&alpha).await;
    let acquirer = f.acquirer().await;
    acquirer.unavailable.lock().unwrap().insert(session.clone());
    let hold = Arc::new(Notify::new());
    *acquirer.hold_pull.lock().unwrap() = Some(hold.clone());
    let shutdown = Arc::new(Notify::new());
    let task = spawn(
        acquirer.clone(),
        vec![alpha.clone()],
        POLICY,
        shutdown.clone(),
    );

    tokio::time::timeout(Duration::from_secs(5), acquirer.pull_started.notified())
        .await
        .expect("first pull starts");
    // Revoke while the pull is in flight, then let the pull "succeed".
    assert!(
        alpha
            .adb
            .unexpose_session_from(&session, "matrix")
            .await
            .unwrap()
    );
    acquirer.unavailable.lock().unwrap().remove(&session);
    *acquirer.hold_pull.lock().unwrap() = None;
    hold.notify_one();

    tokio::time::sleep(POLICY.cap * 3).await;
    assert!(!f.claimed(&session), "a revoked exposure must not register");
    assert!(!f.server.is_watching_session(&session).await);
    assert_eq!(acquirer.pulls(&session), 1, "revocation cancels retries");

    shutdown.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_later_valid_exposure_is_not_hidden_by_an_unattached_agent() {
    let f = fixture().await;
    let a = f.agent("alpha").await;
    let b = f.agent("beta").await;
    let (first, attached) = if a.entry.db_id < b.entry.db_id {
        (a, b)
    } else {
        (b, a)
    };
    let (_, db) = f.registry.create_session(Some("exposed")).await.unwrap();
    let session = db.root_id().to_string();
    f.registry
        .attach_agent_to_session(&session, &attached.entry)
        .await
        .unwrap();
    first
        .adb
        .expose_session_on(&session, "matrix")
        .await
        .unwrap();
    let acquirer = f.acquirer().await;
    let shutdown = Arc::new(Notify::new());
    let task = spawn(
        acquirer.clone(),
        vec![first, attached.clone()],
        POLICY,
        shutdown.clone(),
    );

    // Observe a whole refusal/retry first, so beta cannot win the initial wake.
    eventually(
        "unattached agent tried twice",
        Duration::from_secs(5),
        || acquirer.locals(&session) >= 2,
    )
    .await;
    assert!(!f.claimed(&session));
    attached
        .adb
        .expose_session_on(&session, "matrix")
        .await
        .unwrap();
    eventually(
        "attached agent adopts session",
        Duration::from_secs(5),
        || f.claimed(&session),
    )
    .await;
    assert_eq!(acquirer.pulls(&session), 0, "the session is already local");

    shutdown.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_denial_fails_closed_without_retrying() {
    let f = fixture().await;
    let alpha = f.agent("alpha").await;
    let session = f.exposed_session(&alpha).await;
    let acquirer = f.acquirer().await;
    acquirer.unavailable.lock().unwrap().insert(session.clone());
    acquirer.denied.lock().unwrap().insert(session.clone());
    let shutdown = Arc::new(Notify::new());
    let task = spawn(
        acquirer.clone(),
        vec![alpha.clone()],
        POLICY,
        shutdown.clone(),
    );

    eventually("first pull", Duration::from_secs(5), || {
        acquirer.pulls(&session) == 1
    })
    .await;
    for n in 0..5 {
        noise(&alpha, n).await;
    }
    tokio::time::sleep(POLICY.cap * 3).await;
    assert_eq!(acquirer.pulls(&session), 1);
    assert!(!f.claimed(&session));

    shutdown.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_adopts_existing_and_immediately_written_exposures() {
    let f = fixture().await;
    let alpha = f.agent("alpha").await;
    startup_gap(&f, &alpha, alpha.clone()).await;
}

#[tokio::test]
async fn service_exposure_catches_up_then_observes_live_writes() {
    let f = fixture().await;
    let alpha = f.agent("alpha").await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("exposures.sock");
    let service = eidetica::service::ServiceServer::bind(f.instance.clone(), &socket)
        .await
        .unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(());
    let task = tokio::spawn(service.run(receiver));
    let client = eidetica::Instance::connect(format!("unix://{}", socket.display()))
        .await
        .unwrap();
    let account = client.login_account("test", "test-password").await.unwrap();
    let db = account
        .open_database_with_key(&alpha.entry.db_id, &alpha.entry.pubkey)
        .await
        .unwrap();
    let remote = Exposer {
        entry: alpha.entry.clone(),
        adb: crate::agent_db::AgentDb::from_database(db),
    };
    startup_gap(&f, &alpha, remote).await;
    drop((account, client, shutdown));
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

async fn startup_gap(f: &Fixture, alpha: &Exposer, watched_agent: Exposer) {
    // Exposed before discovery starts: the restart case.
    let existing = f.exposed_session(alpha).await;
    // Watched for transport by some other client, but nobody runs it.
    let (_c, watched_db) = f.registry.create_session(Some("watched")).await.unwrap();
    let watched = watched_db.root_id().to_string();
    f.registry
        .attach_agent_to_session(&watched, &alpha.entry)
        .await
        .unwrap();
    let secrets = crate::security::SecretStore::new(f.registry.chaz_peer().clone()).await;
    f.server
        .watch_session(
            &watched_db,
            crate::backends::BackendManager::new(&None, secrets),
            None,
            None,
        )
        .await
        .unwrap();
    alpha
        .adb
        .expose_session_on(&watched, "matrix")
        .await
        .unwrap();

    let acquirer = f.acquirer().await;
    let release = Arc::new(Notify::new());
    *acquirer.hold_initial_read.lock().unwrap() = Some(release.clone());
    let shutdown = Arc::new(Notify::new());
    let task = spawn(
        acquirer.clone(),
        vec![watched_agent],
        POLICY,
        shutdown.clone(),
    );
    // Land a write strictly after the initial projection and before the
    // callback exists. No later Agent DB write is needed to see it.
    tokio::time::timeout(Duration::from_secs(5), acquirer.initial_read.notified())
        .await
        .unwrap();
    let racing = f.exposed_session(alpha).await;
    release.notify_one();

    for id in [&existing, &watched, &racing] {
        eventually("exposed session registers", Duration::from_secs(5), || {
            f.claimed(id)
        })
        .await;
    }
    let live = f.exposed_session(alpha).await;
    eventually(
        "post-subscription exposure registers",
        Duration::from_secs(5),
        || f.claimed(&live),
    )
    .await;
    for id in [&existing, &watched, &racing, &live] {
        assert_eq!(acquirer.locals(id), 1, "{id} registered exactly once");
        assert_eq!(acquirer.pulls(id), 0);
    }

    shutdown.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_cancels_in_flight_acquisition() {
    let f = fixture().await;
    let alpha = f.agent("alpha").await;
    let session = f.exposed_session(&alpha).await;
    let acquirer = f.acquirer().await;
    acquirer.unavailable.lock().unwrap().insert(session.clone());
    // Never released: the pull would hang forever without cancellation.
    *acquirer.hold_pull.lock().unwrap() = Some(Arc::new(Notify::new()));
    let shutdown = Arc::new(Notify::new());
    let task = spawn(
        acquirer.clone(),
        vec![alpha.clone()],
        POLICY,
        shutdown.clone(),
    );
    tokio::time::timeout(Duration::from_secs(5), acquirer.pull_started.notified())
        .await
        .unwrap();

    shutdown.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("an in-flight pull must not hold shutdown")
        .unwrap();
    // Nothing restarts after shutdown.
    noise(&alpha, 0).await;
    tokio::time::sleep(POLICY.cap * 2).await;
    assert_eq!(acquirer.pulls(&session), 1);
    assert!(
        Arc::strong_count(&acquirer) == 1,
        "no task retains the acquirer"
    );
}
