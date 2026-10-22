//! Synthetic native materialization / service RPC observations, not a production benchmark.
use super::*;
use crate::test_support::counting_backend::counting_eidetica;
use eidetica::service::{
    ServiceServer,
    protocol::{ServiceRequest, read_frame, write_frame},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;
use tokio::sync::watch;

async fn seed(user: &mut eidetica::AccountSession, history: usize) -> Database {
    let key = user.get_default_key().unwrap();
    let db = user
        .create_database(eidetica::crdt::Doc::new(), &key)
        .await
        .unwrap();
    let tx = db.new_transaction().await.unwrap();
    let attempts = tx
        .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
        .await
        .unwrap();
    let activity = tx
        .get_store::<Table<DateTime<Utc>>>(TURN_ACTIVITY_STORE)
        .await
        .unwrap();
    for n in 0..history {
        let id = format!("old-{n}");
        attempts
            .set(
                &id,
                TurnAttempt {
                    attempt_id: id.clone(),
                    request_id: TurnRequestId::parse(&id),
                    started_at: Utc::now() - chrono::Duration::hours(1),
                    completed_at: Some(Utc::now()),
                    status: TurnAttemptStatus::Completed,
                    generation: 1,
                },
            )
            .await
            .unwrap();
        activity.set(&id, Utc::now()).await.unwrap();
    }
    tx.commit().await.unwrap();
    db
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_polling_component_baseline() {
    for history in [0, 1024] {
        for active in [false, true] {
            let (owner, mut user, reads) = counting_eidetica().await;
            let mut dbs = Vec::new();
            for _ in 0..16 {
                let db = seed(&mut user, history).await;
                if active {
                    let session =
                        Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
                    session
                        .start_turn_attempt(TurnRequestId::parse("active"))
                        .await
                        .unwrap();
                }
                dbs.push(db);
            }
            for db in &dbs {
                Session::active_turn_attempts(db).await.unwrap();
            }
            let values: Vec<_> = {
                let mut values = Vec::new();
                for db in &dbs {
                    values.push(Session::turn_activity(db).await.unwrap());
                }
                values
            };
            let before = (reads.count(), reads.activity_tables());
            let started = Instant::now();
            for _ in 0..10 {
                for db in &dbs {
                    assert_eq!(
                        Session::active_turn_attempts(db).await.unwrap().len(),
                        usize::from(active)
                    );
                }
            }
            eprintln!(
                "ACTIVITY baseline embedded sessions=16 history={history} active={active} ticks=10 raw_entry_gets={} activity_table_resolutions={} elapsed_us={}",
                reads.count() - before.0,
                reads.activity_tables() - before.1,
                started.elapsed().as_micros()
            );
            assert!(
                reads.activity_tables() > before.1,
                "positive table-read control"
            );
            let before = (reads.count(), reads.activity_tables());
            let started = Instant::now();
            for _ in 0..10 {
                for value in &values {
                    assert_eq!(value.active_at(Utc::now()).count(), usize::from(active));
                }
            }
            eprintln!(
                "ACTIVITY candidate embedded sessions=16 history={history} active={active} ticks=10 raw_entry_gets={} activity_table_resolutions={} elapsed_us={}",
                reads.count() - before.0,
                reads.activity_tables() - before.1,
                started.elapsed().as_micros()
            );
            assert_eq!((reads.count(), reads.activity_tables()), before);

            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("owner.sock");
            let proxy_socket = dir.path().join("proxy.sock");
            let service = ServiceServer::bind(owner.clone(), &socket).await.unwrap();
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
            let remote_user = client.login_account("test", None).await.unwrap();
            let mut remote = Vec::new();
            for db in &dbs {
                remote.push(remote_user.open_database(db.root_id()).await.unwrap());
            }
            for db in &remote {
                Session::active_turn_attempts(db).await.unwrap();
            }
            let before = (rpcs.load(Ordering::SeqCst), reads.activity_tables());
            let started = Instant::now();
            for _ in 0..10 {
                for db in &remote {
                    assert_eq!(
                        Session::active_turn_attempts(db).await.unwrap().len(),
                        usize::from(active)
                    );
                }
            }
            eprintln!(
                "ACTIVITY baseline service sessions=16 history={history} active={active} ticks=10 authenticated_db_rpcs={} owner_activity_table_resolutions={} elapsed_us={}",
                rpcs.load(Ordering::SeqCst) - before.0,
                reads.activity_tables() - before.1,
                started.elapsed().as_micros()
            );
            assert!(
                rpcs.load(Ordering::SeqCst) > before.0,
                "positive RPC control"
            );
            let mut values = Vec::new();
            for db in &remote {
                values.push(Session::turn_activity(db).await.unwrap());
            }
            let before = (rpcs.load(Ordering::SeqCst), reads.activity_tables());
            let started = Instant::now();
            for _ in 0..10 {
                for value in &values {
                    assert_eq!(value.active_at(Utc::now()).count(), usize::from(active));
                }
            }
            eprintln!(
                "ACTIVITY candidate service sessions=16 history={history} active={active} ticks=10 authenticated_db_rpcs={} owner_activity_table_resolutions={} elapsed_us={}",
                rpcs.load(Ordering::SeqCst) - before.0,
                reads.activity_tables() - before.1,
                started.elapsed().as_micros()
            );
            assert_eq!(
                (rpcs.load(Ordering::SeqCst), reads.activity_tables()),
                before
            );
            proxy.abort();
            let _ = proxy.await;
            drop(shutdown);
            service_task.await.unwrap().unwrap();
        }
    }
}

#[tokio::test]
async fn activity_subscription_initial_read_and_remote_completion_use_native_visibility() {
    use tokio::sync::mpsc;
    for service_mode in [false, true] {
        let (owner, mut user, _) = counting_eidetica().await;
        // Not the account's default identity: all reads/subscriptions must use
        // the database handle's member key, including through the service.
        let key = user.add_private_key(Some("activity-member")).await.unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        let cursor = db.snapshot().await.unwrap();
        let old = session
            .start_turn_attempt(TurnRequestId::parse("before-subscribe"))
            .await
            .unwrap();
        session.complete_turn_attempt(&old, None).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("service.sock");
        let (shutdown, receiver) = watch::channel(());
        let service_task = if service_mode {
            let service = ServiceServer::bind(owner.clone(), &socket).await.unwrap();
            Some(tokio::spawn(service.run(receiver)))
        } else {
            None
        };
        let remote = if service_mode {
            Some(
                eidetica::Instance::connect(format!("unix://{}", socket.display()))
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let observed = if let Some(remote) = &remote {
            let remote_user = remote.login_account("test", None).await.unwrap();
            let key = remote_user.find_keys_by_display_name("activity-member")[0].clone();
            remote_user
                .open_database_with_key(db.root_id(), &key)
                .await
                .unwrap()
        } else {
            db.clone()
        };
        let (events, mut received) = mpsc::unbounded_channel();
        let subscription = observed
            .on_write_at_tips(cursor, move |event, _| {
                let events = events.clone();
                let source = event.source();
                async move {
                    let _ = events.send(source);
                    Ok(())
                }
            })
            .await
            .unwrap();
        // Native registration isn't eager catch-up. Reading AFTER it sees the
        // completed turn even with no subsequent write to trigger a callback.
        assert_eq!(
            Session::turn_activity(&observed)
                .await
                .unwrap()
                .active_at(Utc::now())
                .count(),
            0
        );
        let attempt = session
            .start_turn_attempt(TurnRequestId::parse("during-read"))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        let captured = Session::turn_activity(&observed).await.unwrap();
        assert_eq!(captured.active_at(Utc::now()).count(), 1);
        // Start and heartbeat are also delivered through the native Remote
        // verification path, not just local commits or a supplied source flag.
        for heartbeat in [false, true] {
            if heartbeat {
                Session::renew_turn_activity(&db, &attempt.attempt_id)
                    .await
                    .unwrap();
            }
            crate::test_support::replay_tips_as_remote(&owner, &db).await;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if received.recv().await.unwrap() == eidetica::instance::WriteSource::Remote {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(
                Session::turn_activity(&observed)
                    .await
                    .unwrap()
                    .active_at(Utc::now())
                    .count(),
                1
            );
        }
        // Both transactions branch from the same native view. Commit the
        // completion first, then the competing heartbeat, as late sync can.
        let completion = db.new_transaction().await.unwrap();
        let heartbeat = db.new_transaction().await.unwrap();
        completion
            .get_store::<Table<TurnAttempt>>(TURN_ATTEMPTS_STORE)
            .await
            .unwrap()
            .set(
                &attempt.attempt_id,
                TurnAttempt {
                    status: TurnAttemptStatus::Completed,
                    completed_at: Some(Utc::now()),
                    ..attempt.clone()
                },
            )
            .await
            .unwrap();
        heartbeat
            .get_store::<Table<DateTime<Utc>>>(TURN_ACTIVITY_STORE)
            .await
            .unwrap()
            .set(&attempt.attempt_id, Utc::now())
            .await
            .unwrap();
        completion.commit().await.unwrap();
        heartbeat.commit().await.unwrap();
        crate::test_support::replay_tips_as_remote(&owner, &db).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if received.recv().await.unwrap() == eidetica::instance::WriteSource::Remote {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            Session::turn_activity(&observed)
                .await
                .unwrap()
                .active_at(Utc::now())
                .count(),
            0
        );
        assert_eq!(
            captured.active_at(Utc::now()).count(),
            1,
            "captured evidence is immutable"
        );
        drop(subscription);
        while received.try_recv().is_ok() {}
        Session::renew_turn_activity(&db, "after-teardown")
            .await
            .unwrap();
        assert!(
            received.recv().await.is_none(),
            "dropped observer retained its callback sender"
        );
        drop(remote);
        drop(shutdown);
        if let Some(task) = service_task {
            task.await.unwrap().unwrap();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn activity_exact_boundary_and_future_heartbeat_are_not_discarded_at_load() {
    let now = Utc::now();
    let attempt = TurnAttempt {
        attempt_id: "clock".into(),
        request_id: TurnRequestId::parse("clock"),
        started_at: now,
        generation: 1,
        status: TurnAttemptStatus::Started,
        completed_at: None,
    };
    let value = TurnActivity::from_rows(vec![attempt.clone()], &HashMap::new());
    assert_eq!(value.active_at(now).count(), 1);
    assert_eq!(
        value
            .active_at(now + TURN_ACTIVITY_TIMEOUT - chrono::Duration::nanoseconds(1))
            .count(),
        1
    );
    assert_eq!(value.active_at(now + TURN_ACTIVITY_TIMEOUT).count(), 0);
    let future = now + chrono::Duration::seconds(3);
    let heartbeat = HashMap::from([(attempt.attempt_id.clone(), future)]);
    let value = TurnActivity::from_rows(vec![attempt.clone()], &heartbeat);
    assert_eq!(value.active_at(now).count(), 0);
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    assert_eq!(value.active_at(future).count(), 1);
    let terminal = TurnAttempt {
        status: TurnAttemptStatus::Completed,
        ..attempt.clone()
    };
    assert_eq!(
        TurnActivity::from_rows(vec![attempt, terminal], &heartbeat)
            .active_at(future)
            .count(),
        0
    );
}
