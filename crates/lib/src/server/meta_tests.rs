//! Per-pass metadata reads and control writes that race a pass.

use super::*;
use eidetica::service::ServiceServer;
use eidetica::service::protocol::{DatabaseOp, ServiceRequest, read_frame, write_frame};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, watch};

#[tokio::test]
async fn heartbeat_pass_reads_meta_once_and_next_callback_observes_late_stop() {
    tokio::time::timeout(
        ASYNC_TEST_TIMEOUT,
        Box::pin(async {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("owner.sock");
            let proxy_socket = dir.path().join("counter.sock");
            let (owner, local_server, local_registry) = server_fixture().await;
            local_server.shutdown().await;
            // No adoption or model calls: install a durable running attempt and
            // its real cancellation signal, as spawn_agent_task does.
            let (sid, db) = local_registry
                .create_session(Some("spawn:meta-test"))
                .await
                .unwrap();
            let mut session = Session::new(sid.clone(), db.clone()).await;
            for i in 0..50 {
                session
                    .add_entry(SessionEntry {
                        sender: "user".into(),
                        content: format!("turn {i}"),
                        timestamp: Utc::now(),
                        entry_type: EntryType::Message,
                        metadata: None,
                        routing: None,
                    })
                    .await
                    .unwrap();
                let request = session.latest_entry_with_id().unwrap().0.clone();
                let attempt = session.start_turn_attempt(request).await.unwrap();
                session
                    .add_entry(SessionEntry {
                        sender: "default".into(),
                        content: String::new(),
                        timestamp: Utc::now(),
                        entry_type: EntryType::Ack,
                        metadata: None,
                        routing: None,
                    })
                    .await
                    .unwrap();
                if i < 49 {
                    session
                        .complete_turn_attempt(
                            &attempt,
                            Some(SessionEntry {
                                sender: "default".into(),
                                content: "done".into(),
                                timestamp: Utc::now(),
                                entry_type: EntryType::Message,
                                metadata: None,
                                routing: None,
                            }),
                        )
                        .await
                        .unwrap();
                }
            }
            let attempt = session
                .attempts_for_test()
                .await
                .into_iter()
                .find(|a| a.completed_at.is_none())
                .unwrap();
            let service = ServiceServer::bind(owner, &socket).await.unwrap();
            let (shutdown, receiver) = watch::channel(());
            let service_task = tokio::spawn(service.run(receiver));
            let listener = UnixListener::bind(&proxy_socket).unwrap();
            let counting = Arc::new(AtomicBool::new(false));
            let rpcs = Arc::new(AtomicUsize::new(0));
            let meta_reads = Arc::new(AtomicUsize::new(0));
            let gate = Arc::new(AtomicBool::new(false));
            let paused = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let proxy = ActivityHeartbeat(tokio::spawn({
                let counting = counting.clone();
                let rpcs = rpcs.clone();
                let meta_reads = meta_reads.clone();
                let gate = gate.clone();
                let paused = paused.clone();
                let release = release.clone();
                let id = sid.0.clone();
                async move {
                    let (client, _) = listener.accept().await.unwrap();
                    let upstream = UnixStream::connect(socket).await.unwrap();
                    let (mut client_read, mut client_write) = client.into_split();
                    let (mut upstream_read, mut upstream_write) = upstream.into_split();
                    let requests = async {
                        let mut reading_pass_meta = false;
                        while let Some(frame) = read_frame::<_, serde_json::Value>(&mut client_read)
                            .await
                            .unwrap()
                        {
                            if counting.load(Ordering::Acquire) {
                                rpcs.fetch_add(1, Ordering::SeqCst);
                            }
                            if let Ok(ServiceRequest::AuthenticatedDb(request)) =
                                serde_json::from_value(frame.clone())
                                && request.root_id.to_string() == id
                                && let DatabaseOp::ResolveStoreState { request } = request.op
                            {
                                // SessionMeta is read between transcript hydration and
                                // commands. Marker reads before it and turn-baseline
                                // reads after it also use meta, but are separate controls.
                                if request.store == "entries" {
                                    reading_pass_meta = true;
                                }
                                if request.store == "meta"
                                    && reading_pass_meta
                                    && counting.load(Ordering::Acquire)
                                {
                                    meta_reads.fetch_add(1, Ordering::SeqCst);
                                }
                                if request.store == "session_commands" {
                                    reading_pass_meta = false;
                                    if gate.swap(false, Ordering::AcqRel) {
                                        paused.notify_one();
                                        release.notified().await;
                                    }
                                }
                            }
                            write_frame(&mut upstream_write, &frame).await.unwrap();
                        }
                    };
                    let responses = tokio::io::copy(&mut upstream_read, &mut client_write);
                    let (_, result) = tokio::join!(requests, responses);
                    result.unwrap();
                }
            }));
            let settings = crate::config::EideticaConfig {
                connection: format!("unix://{}", proxy_socket.display()),
                login: crate::config::EideticaLoginConfig {
                    username: "test".into(),
                    password: None,
                    passwordless: true,
                },
                sync: None,
            };
            let connection =
                crate::instance::connect_with(&settings, crate::config::ExecutionRole::Executor)
                    .await
                    .unwrap();
            let registry = Arc::new(
                crate::session::SessionRegistry::new(
                    connection.instance,
                    connection.user,
                    local_server.agents_arc(),
                )
                .await
                .unwrap(),
            );
            let (_, server, _) = server_fixture_from_registry(registry.clone()).await;
            // Remove unrelated watcher work from the measured direct pass.
            server.shutdown().await;
            let cancel = runtime::TurnCancel::new();
            server.turns.lock().await.insert(
                sid.0.clone(),
                RunningTurn {
                    attempt: attempt.clone(),
                    cancel: cancel.clone(),
                },
            );
            server
                .live_attempts
                .lock()
                .await
                .insert(attempt.attempt_id.clone());
            server.processing.lock().await.insert(sid.0.clone());
            // Materialize stores once, as earlier callback passes do in a
            // running session; cold derived-store construction is not heartbeat cost.
            server.process_session(&sid.0).await.unwrap();
            Session::renew_turn_activity(&db, &attempt.attempt_id)
                .await
                .unwrap();
            counting.store(true, Ordering::Release);
            assert!(server.process_session(&sid.0).await.unwrap().is_none());
            counting.store(false, Ordering::Release);
            let total = rpcs.load(Ordering::SeqCst);
            let opens = meta_reads.load(Ordering::SeqCst);
            eprintln!("heartbeat pass: {total} RPCs, {opens} SessionMeta store resolutions");
            assert_eq!(opens, 1, "one get_all, not repeated per-key meta reads");
            assert!(total <= 110, "heartbeat pass used {total} RPCs");
            assert!(!cancel.is_cancelled());

            // A fresh executor supplies the ordinary callback-driven processing loop.
            let (_, executor, _) = server_fixture_from_registry(registry.clone()).await;
            executor.turns.lock().await.insert(
                sid.0.clone(),
                RunningTurn {
                    attempt: attempt.clone(),
                    cancel: cancel.clone(),
                },
            );
            executor
                .live_attempts
                .lock()
                .await
                .insert(attempt.attempt_id.clone());
            executor.processing.lock().await.insert(sid.0.clone());
            let (_, remote_db) = registry.open_session(&sid.0).await.unwrap();
            executor
                .watch_session(&remote_db, executor.default_backend.clone(), None, None)
                .await
                .unwrap();
            gate.store(true, Ordering::Release);
            Session::renew_turn_activity(&db, &attempt.attempt_id)
                .await
                .unwrap();
            paused.notified().await;
            assert!(session.request_stop("user").await.unwrap());
            assert!(
                !cancel.is_cancelled(),
                "the gated pass has an older transcript"
            );
            release.notify_one();
            // No explicit wake/process_session call: the stop's own write callback
            // must cause the next pass to cancel the exact running attempt.
            cancel.cancelled().await;
            assert!(cancel.is_cancelled());
            executor.shutdown().await;
            let _ = shutdown.send(());
            service_task.await.unwrap().unwrap();
            drop(proxy);
        }),
    )
    .await
    .expect("metadata regression must finish");
}

#[tokio::test]
async fn process_session_acknowledges_ambient_once_per_matching_generation() {
    use crate::session::{
        AmbientParticipation, ambient_participation_receipt, bind_transport, update_meta_on_db,
    };
    let (_owner, server, registry) = server_fixture().await;
    server.shutdown().await;
    let (sid, db) = registry
        .create_session(Some("spawn:ambient-meta-test"))
        .await
        .unwrap();
    bind_transport(&db, "matrix", "@bot:example", "!room:example")
        .await
        .unwrap();
    let mut policy = AmbientParticipation::new("@bot:example", "!other:example");
    update_meta_on_db(&db, |meta| {
        meta.ambient_participation = Some(policy.clone())
    })
    .await
    .unwrap();
    server.process_session(&sid.0).await.unwrap();
    assert_eq!(ambient_participation_receipt(&db).await.unwrap(), None);
    policy.channel = "!room:example".into();
    for _ in 0..2 {
        policy.generation = uuid::Uuid::new_v4().to_string();
        update_meta_on_db(&db, |meta| {
            meta.ambient_participation = Some(policy.clone())
        })
        .await
        .unwrap();
        let before = db.snapshot().await.unwrap();
        server.process_session(&sid.0).await.unwrap();
        let acknowledged = db.snapshot().await.unwrap();
        assert_ne!(before, acknowledged, "new generation must write a receipt");
        assert_eq!(
            ambient_participation_receipt(&db).await.unwrap().as_deref(),
            Some(policy.generation.as_str())
        );
        server.process_session(&sid.0).await.unwrap();
        assert_eq!(
            db.snapshot().await.unwrap(),
            acknowledged,
            "same generation must not write again"
        );
    }
}
