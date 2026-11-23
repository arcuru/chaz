//! Synthetic read amplification controls; no models or production state.
use super::super::activity_backend::counting_eidetica;
use super::*;
use eidetica::store::Table;

async fn seed(user: &mut eidetica::AccountSession, history: usize) -> eidetica::Database {
    let key = user.get_default_key().unwrap();
    let db = user
        .create_database(eidetica::crdt::Doc::new(), &key)
        .await
        .unwrap();
    let txn = db.new_transaction().await.unwrap();
    let entries = txn
        .get_store::<Table<SessionEntry>>("entries")
        .await
        .unwrap();
    for n in 0..history {
        entries
            .set(
                format!("row-{n:05}"),
                SessionEntry {
                    sender: "user".into(),
                    content: format!("synthetic transcript row {n}"),
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                    metadata: None,
                    routing: None,
                },
            )
            .await
            .unwrap();
    }
    txn.commit().await.unwrap();
    db
}

#[tokio::test]
async fn hydration_one_transcript_per_rebuild() {
    let (_owner, mut user, reads) = counting_eidetica().await;
    let db = seed(&mut user, 1024).await;
    // Warm native materialization so resolutions are distinguished from raw I/O.
    load_session_snapshot("fixture", db.clone()).await.unwrap();
    let before = reads.tables();
    let raw = reads.count();
    let loaded = load_session_snapshot("fixture", db).await.unwrap();
    let after = reads.tables();
    let entries =
        after.get("entries").copied().unwrap_or(0) - before.get("entries").copied().unwrap_or(0);
    eprintln!(
        "HYDRATION session history=1024 transcript_resolutions={entries} raw_entry_gets={} before={before:?} after={after:?}",
        reads.count() - raw
    );
    assert_eq!(loaded.entries.len(), 1024);
    assert_eq!(
        entries, 1,
        "one rebuild must hydrate the transcript only once"
    );
}

#[tokio::test]
async fn hydration_component_observation_many_tabs() {
    let (_owner, mut user, reads) = counting_eidetica().await;
    let mut dbs = Vec::new();
    for _ in 0..16 {
        dbs.push(seed(&mut user, 1024).await);
    }
    for db in &dbs {
        load_session_snapshot("fixture", db.clone()).await.unwrap();
    }
    let before = reads.tables();
    let raw = reads.count();
    for db in &dbs {
        load_session_snapshot("fixture", db.clone()).await.unwrap();
    }
    let after = reads.tables();
    let delta: std::collections::BTreeMap<_, _> = after
        .into_iter()
        .map(|(key, n)| {
            let count = n - before.get(&key).copied().unwrap_or(0);
            (key, count)
        })
        .collect();
    eprintln!(
        "HYDRATION embedded tabs=16 history=1024 rebuilds=16 raw_entry_gets={} store_resolutions={delta:?}",
        reads.count() - raw
    );
    assert!(delta["entries"] >= 16, "detecting transcript control");
}

use super::tests::{resolved, tab};
use chaz_core::session::jobs::{JobState, StagedAgentDefinition};
use std::{sync::atomic::AtomicUsize, time::Duration};
use tokio::sync::{oneshot, watch};

async fn next(rx: &mut mpsc::Receiver<Refresh>) -> Refresh {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn round(
    app: &mut App,
    id: &str,
    tx: &mpsc::Sender<Refresh>,
    rx: &mut mpsc::Receiver<Refresh>,
) -> bool {
    request(app, id, RefreshKind::Session, tx);
    let msg = next(rx).await;
    let unchanged = matches!(msg, Refresh::Unchanged { .. });
    assert_eq!(apply(app, msg, resolved).follow_up, None);
    unchanged
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydration_service_rpc_counts_and_unchanged_views() {
    use eidetica::service::{
        ServiceServer,
        protocol::{ServiceRequest, read_frame, write_frame},
    };
    let (owner, mut user, reads) = counting_eidetica().await;
    let mut dbs = Vec::new();
    for _ in 0..16 {
        dbs.push(seed(&mut user, 1024).await);
    }
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("owner.sock");
    let proxy_socket = dir.path().join("proxy.sock");
    let service = ServiceServer::bind(owner, &socket).await.unwrap();
    let (shutdown, receiver) = watch::channel(());
    let service_task = tokio::spawn(service.run(receiver));
    let listener = tokio::net::UnixListener::bind(&proxy_socket).unwrap();
    let rpcs = Arc::new(AtomicUsize::new(0));
    let proxy = tokio::spawn({
        let rpcs = rpcs.clone();
        async move {
            let (client, _) = listener.accept().await.unwrap();
            let service = tokio::net::UnixStream::connect(socket).await.unwrap();
            let (mut input, mut output) = client.into_split();
            let (mut replies, mut requests) = service.into_split();
            let upstream = async {
                while let Some(frame) = read_frame::<_, serde_json::Value>(&mut input)
                    .await
                    .unwrap()
                {
                    if let Ok(ServiceRequest::AuthenticatedDb(_)) =
                        serde_json::from_value(frame.clone())
                    {
                        rpcs.fetch_add(1, Ordering::SeqCst);
                    }
                    write_frame(&mut requests, &frame).await.unwrap();
                }
            };
            let downstream = async {
                let _ = tokio::io::copy(&mut replies, &mut output).await;
            };
            tokio::select! { _ = upstream => {}, _ = downstream => {} }
        }
    });
    let client = eidetica::Instance::connect(format!("unix://{}", proxy_socket.display()))
        .await
        .unwrap();
    let user = client.login_account("test", "test-password").await.unwrap();
    let mut app = App::hub(HashSet::new());
    for db in &dbs {
        let remote = user.open_database(db.root_id()).await.unwrap();
        app.push_tab(tab(&db.root_id().to_string(), remote));
    }
    // Warm the transcripts under test before auxiliary views can fill the
    // per-login cache budget. This counts warm reads, not cache capacity.
    for tab in &app.tabs {
        let db = &tab.session_db;
        let snapshot = db.snapshot().await.unwrap();
        let entries = Session::entries_at_snapshot(db, &snapshot).await.unwrap();
        assert_eq!(entries.len(), 1024);
    }
    let (tx, mut rx) = mpsc::channel(64);
    let ids: Vec<_> = app.tabs.iter().map(|t| t.session_db_id.clone()).collect();
    let before = rpcs.load(Ordering::SeqCst);
    let tables = reads.tables();
    for id in &ids {
        assert!(!round(&mut app, id, &tx, &mut rx).await);
    }
    let entries = reads.tables()["entries"] - tables["entries"];
    eprintln!(
        "HYDRATION service tabs=16 history=1024 candidate_rebuild_db_rpcs={} transcript_resolutions={entries}",
        rpcs.load(Ordering::SeqCst) - before
    );
    assert_eq!(entries, 16);
    let before = rpcs.load(Ordering::SeqCst);
    let tables = reads.tables();
    for _ in 0..10 {
        for id in &ids {
            assert!(round(&mut app, id, &tx, &mut rx).await);
        }
    }
    let after = reads.tables();
    eprintln!(
        "HYDRATION service tabs=16 history=1024 unchanged_rounds=10 db_rpcs={} transcript_resolutions={} telemetry_resolutions={}",
        rpcs.load(Ordering::SeqCst) - before,
        after["entries"] - tables["entries"],
        after["call_telemetry"] - tables["call_telemetry"]
    );
    assert!(
        rpcs.load(Ordering::SeqCst) > before,
        "native permission/snapshot probes are deliberate, not cached authority"
    );
    for store in [
        "entries",
        "call_telemetry",
        "turn_transcript",
        "turn_attempts",
        "turn_activity",
    ] {
        assert_eq!(after[store], tables[store], "unchanged {store}");
    }
    drop(app);
    proxy.abort();
    let _ = proxy.await;
    drop(shutdown);
    service_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn hydration_callbacks_during_load_remote_hints_and_teardown() {
    use eidetica::service::ServiceServer;
    for service_mode in [false, true] {
        let (owner, mut user, reads) = counting_eidetica().await;
        let key = user.add_private_key(Some("view-member")).await.unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let session = Session::try_new(
            chaz_core::types::ConversationId("fixture".into()),
            db.clone(),
        )
        .await
        .unwrap();
        session
            .update_meta(|m| m.name = Some("before-subscribe".into()))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("owner.sock");
        let (shutdown, receiver) = watch::channel(());
        let service_task = if service_mode {
            let service = ServiceServer::bind(owner.clone(), &socket).await.unwrap();
            Some(tokio::spawn(service.run(receiver)))
        } else {
            None
        };
        let client = if service_mode {
            Some(
                eidetica::Instance::connect(format!("unix://{}", socket.display()))
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let observed = if let Some(client) = &client {
            let user = client.login_account("test", "test-password").await.unwrap();
            let key = user.find_keys_by_display_name("view-member")[0].clone();
            user.open_database_with_key(db.root_id(), &key)
                .await
                .unwrap()
        } else {
            db.clone()
        };
        let id = db.root_id().to_string();
        let (notify, mut events) = mpsc::unbounded_channel();
        let subscription = observed
            .on_write(move |latest, _| {
                let _ = notify.send(latest.clone());
                std::future::ready(Ok(()))
            })
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap(),
            db.snapshot().await.unwrap()
        );
        let mut app = App::new(HashSet::new(), tab(&id, observed.clone()));
        let (tx, mut rx) = mpsc::channel(64);
        assert!(!round(&mut app, &id, &tx, &mut rx).await);
        assert_eq!(
            app.active().session_name.as_deref(),
            Some("before-subscribe"),
            "subscribe-before-read catch-up"
        );
        let (release, gate) = oneshot::channel();
        let (loaded, ready) = oneshot::channel();
        let slow = observed.clone();
        let sid = id.clone();
        request_with(
            &mut app,
            &id,
            RefreshKind::Session,
            &tx,
            move |ticket| async move {
                let tips = slow.snapshot().await.unwrap();
                let session = Session::try_new_at(
                    chaz_core::types::ConversationId("slow".into()),
                    slow.clone(),
                    &tips,
                )
                .await
                .unwrap();
                loaded.send(()).unwrap();
                gate.await.unwrap();
                let snapshot = snapshot_from_session(&session, &slow, tips)
                    .await
                    .map(Box::new)
                    .map_err(|e| e.to_string());
                Refresh::Session {
                    session_db_id: sid,
                    ticket,
                    snapshot,
                }
            },
        );
        ready.await.unwrap();
        for n in 0..8 {
            // The second handle uses authenticated service writes in service mode.
            chaz_core::session::update_meta_on_db(if n % 2 == 0 { &db } else { &observed }, |m| {
                m.name = Some(format!("during-{n}"))
            })
            .await
            .unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                db.snapshot().await.unwrap()
            );
            request(&mut app, &id, RefreshKind::Session, &tx);
        }
        release.send(()).unwrap();
        let applied = apply(&mut app, next(&mut rx).await, resolved);
        assert_eq!(applied.follow_up, Some((id.clone(), RefreshKind::Session)));
        assert_eq!(
            app.active().session_name.as_deref(),
            Some("before-subscribe"),
            "a later meta write leaked into the pinned partial load"
        );
        assert!(rx.try_recv().is_err(), "burst spawned an unbounded read");
        assert!(!round(&mut app, &id, &tx, &mut rx).await);
        assert_eq!(app.active().session_name.as_deref(), Some("during-7"));
        let before = reads.tables();
        for _ in 0..10 {
            assert!(round(&mut app, &id, &tx, &mut rx).await);
        }
        assert_eq!(
            reads.tables()["entries"],
            before["entries"],
            "duplicate hints cannot rehydrate an unchanged transcript"
        );
        assert_eq!(app.active().session_name.as_deref(), Some("during-7"));
        while events.try_recv().is_ok() {}
        drop(subscription);
        session
            .update_meta(|m| m.name = Some("after-teardown".into()))
            .await
            .unwrap();
        assert!(
            matches!(
                tokio::time::timeout(Duration::from_millis(40), events.recv()).await,
                Ok(None) | Err(_)
            ),
            "dropped subscription forwarded another notification"
        );
        drop(app);
        drop(client);
        drop(shutdown);
        if let Some(task) = service_task {
            task.await.unwrap().unwrap();
        }
    }
}

async fn started_job(db: &eidetica::Database) -> (Session, chaz_core::session::TurnAttempt) {
    use chaz_core::session::EntryType;
    let mut session = Session::try_new(
        chaz_core::types::ConversationId(db.root_id().to_string()),
        db.clone(),
    )
    .await
    .unwrap();
    let directive = SessionEntry {
        sender: "parent".into(),
        content: "synthetic job".into(),
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Directive,
        metadata: None,
        routing: None,
    };
    session.add_entry(directive).await.unwrap();
    let request = session.entries_with_ids().last().unwrap().0.clone();
    let definition = StagedAgentDefinition {
        target: "fixture".into(),
        task: "synthetic job".into(),
        executor_pubkey: "fixture".into(),
        call_depth: 0,
        max_call_depth: 1,
        allowed_tools: vec![],
        tool_ceilings: Default::default(),
        capability_ceiling: Default::default(),
    };
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<eidetica::store::DocStore>("job_acceptance").await.unwrap().set_string("v1",serde_json::json!({"parent_id":"fixture","definition":definition,"directive_id":request.as_str()}).to_string()).await.unwrap();
    txn.commit().await.unwrap();
    let attempt = session.start_turn_attempt(request).await.unwrap();
    (session, attempt)
}

#[tokio::test]
async fn hydration_job_ticks_skip_transcript_and_telemetry_but_age_and_observe_changes() {
    let (_owner, mut user, reads) = counting_eidetica().await;
    let db = seed(&mut user, 1024).await;
    let (mut session, attempt) = started_job(&db).await;
    let id = db.root_id().to_string();
    let mut job_tab = tab(&id, db.clone());
    job_tab.job = Some(jobs::JobTab {
        state: None,
        details: false,
    });
    let mut app = App::new(HashSet::new(), job_tab);
    let (tx, mut rx) = mpsc::channel(64);
    assert!(!round(&mut app, &id, &tx, &mut rx).await);
    assert!(matches!(
        app.active().job.as_ref().unwrap().state,
        Some(JobState::StartedUnknown {
            activity_recent: true,
            ..
        })
    ));
    let before = reads.tables();
    assert!(round(&mut app, &id, &tx, &mut rx).await);
    for _ in 0..10 {
        request_tick(&mut app, &tx);
        let msg = next(&mut rx).await;
        assert!(matches!(
            msg,
            Refresh::Unchanged {
                kind: RefreshKind::Job,
                ..
            }
        ));
        assert_eq!(
            apply(&mut app, msg, resolved).follow_up,
            None,
            "tick must request only one job check"
        );
    }
    let after = reads.tables();
    for store in [
        "entries",
        "call_telemetry",
        "turn_transcript",
        "turn_attempts",
        "turn_activity",
    ] {
        assert_eq!(
            after[store], before[store],
            "unchanged job tick read {store}"
        );
    }
    assert!(age_steering(
        &mut app,
        attempt.started_at + chrono::Duration::seconds(45)
    ));
    assert_eq!(app.active().active_turns, 0);
    assert!(matches!(
        app.active().job.as_ref().unwrap().state,
        Some(JobState::StartedUnknown {
            activity_recent: false,
            ..
        })
    ));
    assert_eq!(reads.tables(), after, "expiry performs no storage I/O");
    // A real change still performs exactly one transcript hydration.
    session
        .update_meta(|m| m.name = Some("renamed job".into()))
        .await
        .unwrap();
    let before = reads.tables();
    assert!(!round(&mut app, &id, &tx, &mut rx).await);
    assert_eq!(reads.tables()["entries"] - before["entries"], 1);
    assert_eq!(app.active().session_name.as_deref(), Some("renamed job"));
    session.complete_turn_attempt(&attempt, None).await.unwrap();
    assert!(!round(&mut app, &id, &tx, &mut rx).await);
    assert!(matches!(
        app.active().job.as_ref().unwrap().state,
        Some(JobState::Succeeded { .. })
    ));
}

#[tokio::test]
async fn hydration_failures_never_cover_partial_views_and_success_retries() {
    use eidetica::store::DocStore;
    let (_owner, mut user, reads) = counting_eidetica().await;
    let db = seed(&mut user, 1024).await;
    let id = db.root_id().to_string();
    let mut app = App::new(HashSet::new(), tab(&id, db.clone()));
    let (tx, mut rx) = mpsc::channel(16);
    round(&mut app, &id, &tx, &mut rx).await;
    assert!(app.active().live.covered.is_some());
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<DocStore>("meta")
        .await
        .unwrap()
        .set_string("agents", "malformed-json")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    request(&mut app, &id, RefreshKind::Session, &tx);
    apply(&mut app, next(&mut rx).await, resolved);
    assert!(app.active().live.covered.is_none());
    assert!(app.active().live.failed(RefreshKind::Session));
    assert_eq!(
        app.active().entries.len(),
        1024,
        "failed read discarded last good transcript"
    );
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<DocStore>("meta")
        .await
        .unwrap()
        .set_string("agents", "[]")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(!round(&mut app, &id, &tx, &mut rx).await);
    assert!(app.active().live.error.is_none());
    // A partial activity result cannot certify its successful transcript as fresh.
    let mut snapshot = load_session_snapshot(&id, db.clone()).await.unwrap();
    snapshot.active_turns = Err("controlled partial activity failure".into());
    let sid = id.clone();
    request_with(
        &mut app,
        &id,
        RefreshKind::Session,
        &tx,
        move |ticket| async move {
            Refresh::Session {
                session_db_id: sid,
                ticket,
                snapshot: Ok(Box::new(snapshot)),
            }
        },
    );
    apply(&mut app, next(&mut rx).await, resolved);
    assert!(app.active().live.covered.is_none());
    let before = reads.tables();
    assert!(!round(&mut app, &id, &tx, &mut rx).await);
    assert_eq!(
        reads.tables()["entries"] - before["entries"],
        1,
        "failed partial view suppressed retry"
    );
    assert!(app.active().live.error.is_none());
}

#[tokio::test]
async fn hydration_equal_frontier_is_not_cached_authority() {
    use eidetica::{
        auth::{AuthKey, SigKey, types::Permission},
        service::ServiceServer,
    };
    for service_mode in [false, true] {
        let (owner, mut user, _reads) = counting_eidetica().await;
        let db = seed(&mut user, 8).await;
        user.admin()
            .await
            .unwrap()
            .create_account(eidetica::NewAccount::with_password(
                "reader",
                "test-password",
            ))
            .await
            .unwrap();
        let mut reader = owner
            .login_account("reader", "test-password")
            .await
            .unwrap();
        let key = reader.add_private_key(Some("view-reader")).await.unwrap();
        let txn = db.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(&key, AuthKey::active(Some("reader"), Permission::Read))
            .await
            .unwrap();
        txn.commit().await.unwrap();
        reader
            .map_key(&key, db.root_id(), SigKey::from_pubkey(&key))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("owner.sock");
        let (shutdown, receiver) = watch::channel(());
        let service = if service_mode {
            let server = ServiceServer::bind(owner.clone(), &socket).await.unwrap();
            Some(tokio::spawn(server.run(receiver)))
        } else {
            None
        };
        let client = if service_mode {
            Some(
                eidetica::Instance::connect(format!("unix://{}", socket.display()))
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let observed = if let Some(client) = &client {
            let reader = client
                .login_account("reader", "test-password")
                .await
                .unwrap();
            let key = reader.find_keys_by_display_name("view-reader")[0].clone();
            reader
                .open_database_with_key(db.root_id(), &key)
                .await
                .unwrap()
        } else {
            reader
                .open_database_with_key(db.root_id(), &key)
                .await
                .unwrap()
        };
        assert_eq!(
            observed.current_permission().await.unwrap(),
            Permission::Read
        );
        let id = db.root_id().to_string();
        let mut app = App::new(HashSet::new(), tab(&id, db.clone()));
        let (tx, mut rx) = mpsc::channel(8);
        round(&mut app, &id, &tx, &mut rx).await;
        assert_eq!(
            app.active().live.covered.as_ref(),
            Some(&observed.snapshot().await.unwrap())
        );
        // A previously displayed view, same root and same frontier, grants nothing.
        app.active_mut().session_db = observed;
        request(&mut app, &id, RefreshKind::Session, &tx);
        let msg = next(&mut rx).await;
        assert!(
            matches!(&msg,Refresh::Session{snapshot:Err(error),..} if error.contains("Write")),
            "same frontier bypassed current native permission check"
        );
        apply(&mut app, msg, resolved);
        assert!(app.active().live.covered.is_none());
        assert_eq!(
            app.active().entries.len(),
            8,
            "last-good display must remain clearly stale"
        );
        assert!(app.active().live.error.is_some());
        app.active_mut().session_db = db;
        assert!(!round(&mut app, &id, &tx, &mut rx).await);
        assert!(app.active().live.error.is_none());
        drop(app);
        drop(client);
        drop(shutdown);
        if let Some(task) = service {
            task.await.unwrap().unwrap();
        }
    }
}

async fn install_watch(app: &mut App, tx: &presentation::Sender) {
    app.active_mut().live.observation = Some(
        jobs::observe(&app.active().session_db, tx.clone())
            .await
            .unwrap(),
    );
}

async fn apply_watch(reads: &mut presentation::Reads, app: &mut App) {
    let loaded = reads.join_next().await.unwrap().unwrap();
    assert!(presentation::apply(app, loaded, resolved).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydration_acknowledged_snapshot_feed() {
    for connected in [false, true] {
        tokio::time::timeout(Duration::from_secs(15), acknowledged_round(connected))
            .await
            .expect("snapshot/commit/GUI wait cycle");
    }
}

async fn acknowledged_round(connected: bool) {
    let (owner, mut user, _) = counting_eidetica().await;
    let db = seed(&mut user, 3).await;
    let other = seed(&mut user, 1).await;
    let initial = db.snapshot().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("watch.sock");
    let (shutdown, receiver) = watch::channel(());
    let service = if connected {
        let service = eidetica::service::ServiceServer::bind(owner.clone(), &socket)
            .await
            .unwrap();
        Some(tokio::spawn(service.run(receiver)))
    } else {
        None
    };
    let client = if connected {
        Some(
            eidetica::Instance::connect(format!("unix://{}", socket.display()))
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let (observed, independent) = if let Some(client) = &client {
        let login = client.login_account("test", "test-password").await.unwrap();
        (
            login.open_database(db.root_id()).await.unwrap(),
            login.open_database(other.root_id()).await.unwrap(),
        )
    } else {
        (db.clone(), other.clone())
    };
    let id = db.root_id().to_string();
    let mut app = App::new(HashSet::new(), tab(&id, observed.clone()));
    let mut elsewhere = App::new(
        HashSet::new(),
        tab(&other.root_id().to_string(), independent),
    );
    let (tx, mut inbox) = presentation::channel();
    let (other_tx, mut other_inbox) = presentation::channel();
    let mut reads = presentation::Reads::default();
    install_watch(&mut app, &tx).await;
    install_watch(&mut elsewhere, &other_tx).await;
    reads.start_initial(&mut elsewhere);
    apply_watch(&mut reads, &mut elsewhere).await;

    // Hold initial hydration until *after* a write burst. Neither same-tree
    // commits nor a different tree on the same transport wait for the GUI.
    for name in ["one", "two", "three"] {
        chaz_core::session::update_meta_on_db(&db, |m| m.name = Some(name.into()))
            .await
            .unwrap();
    }
    let outputs = std::collections::BTreeMap::from([(
        "fixture".into(),
        chaz_core::extension::ExtensionOutput {
            status: [("phase".into(), "new status".into())].into(),
        },
    )]);
    chaz_core::extension::write_extension_outputs(&db, &outputs)
        .await
        .unwrap();
    chaz_core::session::update_meta_on_db(&other, |m| m.name = Some("independent".into()))
        .await
        .unwrap();
    reads.start(&mut elsewhere, other_inbox.recv().await.unwrap());
    apply_watch(&mut reads, &mut elsewhere).await;
    assert_eq!(
        elsewhere.active().session_name.as_deref(),
        Some("independent")
    );

    reads.start_initial(&mut app);
    let loaded = reads.join_next().await.unwrap().unwrap();
    assert!(matches!(
        inbox.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(presentation::apply(&mut app, loaded, resolved).is_some());
    assert_eq!(app.active().live.covered.as_ref(), Some(&initial));
    assert_eq!(app.active().entries.len(), 3);
    assert_eq!(app.active().session_name, None);
    assert!(app.active().live.status_segments.is_empty());

    let latest = db.snapshot().await.unwrap();
    loop {
        reads.start(&mut app, inbox.recv().await.unwrap());
        apply_watch(&mut reads, &mut app).await;
        if app.active().live.covered.as_ref() == Some(&latest) {
            break;
        }
        assert!(
            connected,
            "local mailbox must retain only the latest pending snapshot"
        );
    }
    assert_eq!(app.active().session_name.as_deref(), Some("three"));
    assert_eq!(app.active().live.status_segments, ["new status"]);
    app.close_tab_at(0);
    app.push_tab(tab(&id, observed.clone()));
    install_watch(&mut app, &tx).await;
    reads.start_initial(&mut app);
    apply_watch(&mut reads, &mut app).await;
    assert_eq!(app.active().session_name.as_deref(), Some("three"));
    drop(app);
    drop(elsewhere);
    drop(client);
    drop(shutdown);
    if let Some(service) = service {
        service.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydration_retirement_finishes_started_verification() {
    tokio::time::timeout(Duration::from_secs(10), async {
        use eidetica::{
            Database, auth::crypto::PrivateKey, backend::VerificationStatus, store::DocStore,
        };
        let (source, _, _) = counting_eidetica().await;
        let signer = PrivateKey::generate();
        let writer = Database::create(&source, signer.clone(), Default::default())
            .await
            .unwrap();
        let (owner, _, reads) = counting_eidetica().await;
        owner
            .put_entry(
                writer.root_id(),
                VerificationStatus::Unverified,
                writer.get_entry(writer.root_id()).await.unwrap(),
                eidetica::WriteSource::Remote,
            )
            .await
            .unwrap();
        let db = Database::open(&owner, writer.root_id())
            .await
            .unwrap()
            .with_key(signer);
        let id = db.root_id().to_string();
        let (tx, _old_rx) = presentation::channel();
        let (surviving, mut live_rx) = presentation::channel();
        let mut app = App::new(HashSet::new(), tab(&id, db.clone()));
        let mut other = App::new(HashSet::new(), tab(&id, db.clone()));
        let mut owned = presentation::Reads::default();
        install_watch(&mut app, &tx).await;
        install_watch(&mut other, &surviving).await;
        owned.start_initial(&mut other);
        apply_watch(&mut owned, &mut other).await;

        let txn = writer.new_transaction().await.unwrap();
        txn.get_store::<DocStore>("meta")
            .await
            .unwrap()
            .set_string("name", "promoted")
            .await
            .unwrap();
        let entry = txn.commit().await.unwrap();
        db.insert_raw(writer.get_entry(&entry).await.unwrap())
            .await
            .unwrap();
        let (promoted, release) = reads.pause_after_promotion(entry.clone());
        // Reading the initial historical view rechecks *live* native Write
        // authority. That check verifies the newer signed entry. Its lifetime
        // is the owned load, not the registration waiting for GUI handling.
        owned.start_initial(&mut app);
        promoted.await.unwrap();
        for c in "still typing".chars() {
            input::handle_chat_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        assert_eq!(app.input, "still typing");
        app.close_tab_at(0);
        release.send(()).unwrap();
        let loaded = owned.join_next().await.unwrap().unwrap();
        // Reopen the same DB before the retired result can touch the GUI.
        app.push_tab(tab(&id, db.clone()));
        install_watch(&mut app, &tx).await;
        assert!(presentation::apply(&mut app, loaded, resolved).is_none());
        assert_eq!(app.active().session_name, None);
        // No retry, new read or later write can heal a missed publication.
        // The surviving registration must receive the promotion itself.
        let live = live_rx.recv().await.unwrap();
        assert_eq!(live.tips.tips(), &[entry]);
        owned.start(&mut other, live);
        apply_watch(&mut owned, &mut other).await;
        assert_eq!(other.active().session_name.as_deref(), Some("promoted"));
    })
    .await
    .expect("retiring one observer lost another observer's promotion");
}
