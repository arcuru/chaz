use super::*;
use crate::{
    runtime::ToolCallRequest,
    session::{EntryType, SessionEntry},
    types::ConversationId,
};
use eidetica::{
    Instance, NewAccount, PrivateKey,
    backend::database::{InMemory, Sqlite},
    sync::{Address, Sync, transports::http::HttpTransport},
};
use std::sync::atomic::{AtomicUsize, Ordering};

const PASSWORD: &str = "fixture-only";

struct Fixture {
    admin: Database,
    requester: PrivateKey,
    fulfiller: PrivateKey,
    binding: Binding,
    journal: std::path::PathBuf,
}

impl Fixture {
    async fn new(dir: &Path, user: &mut AccountSession) -> Result<Self> {
        let admin = allocate_root(user, &user.get_default_key()?).await?;
        let requester = PrivateKey::generate();
        let fulfiller = PrivateKey::generate();
        let journal = dir.join("provider-journal");
        let binding = Binding::provision(
            &journal,
            admin.root_id().clone(),
            Resource {
                transport: "fixture".into(),
                login: "provider-login".into(),
                channel: "one-resource".into(),
                operation: "read_marker".into(),
            },
            requester.public_key(),
            fulfiller.public_key(),
        )?;
        Ok(Self {
            admin,
            requester,
            fulfiller,
            binding,
            journal,
        })
    }

    fn requester(&self) -> Database {
        self.admin.clone().with_key(self.requester.clone())
    }
    fn fulfiller(&self) -> Database {
        self.admin.clone().with_key(self.fulfiller.clone())
    }

    async fn pending(&self) -> Result<(WorkerCallSite, Snapshot)> {
        let db = self.requester();
        let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        let request_id = session
            .add_entry(SessionEntry {
                sender: "operator".into(),
                content: "fixture input".into(),
                timestamp: Utc::now(),
                entry_type: EntryType::Message,
                metadata: None,
                routing: None,
            })
            .await?;
        let attempt = session.start_turn_attempt(request_id.clone()).await?;
        let context = db.snapshot().await?;
        let site = WorkerCallSite {
            request_id,
            attempt_id: attempt.attempt_id,
            execution_id: Some("exact-branch".into()),
            agent: "fixture".into(),
            model_sequence: 0,
            call_index: 0,
            call_id: "one-call".into(),
        };
        session
            .append_turn_transcript(
                TurnTranscriptRecord {
                    request_id: site.request_id.clone(),
                    attempt_id: site.attempt_id.clone(),
                    sequence: 0,
                    timestamp: Utc::now(),
                    identity: Some(TurnCallIdentity {
                        agent: site.agent.clone(),
                        requested_model: "no-model-transport".into(),
                        execution_id: site.execution_id.clone(),
                    }),
                    message: TurnTranscriptMessage::ModelResponse {
                        model_sequence: 0,
                        content: None,
                        terminal: false,
                        metadata: None,
                        provider_extra: Default::default(),
                        tool_calls: vec![ToolCallRequest {
                            id: site.call_id.clone(),
                            name: self.binding.resource.operation.clone(),
                            arguments: "{}".into(),
                        }],
                    },
                },
                Vec::new(),
            )
            .await?;
        Ok((site, context))
    }
}

async fn grant(db: &Database, pk: &PublicKey, permission: Permission) -> Result<()> {
    let txn = db.new_transaction().await?;
    txn.get_settings()?
        .set_auth_key(pk, AuthKey::active(None, permission))
        .await?;
    txn.commit().await?;
    Ok(())
}

async fn publish(db: &Database, record: Record) -> Result<ID> {
    let txn = db.new_transaction().await?;
    write_record(&txn, &record).await?;
    Ok(txn.commit().await?)
}

async fn listen(sync: &Sync) -> Result<Address> {
    sync.register_transport("http", HttpTransport::builder().bind("127.0.0.1:0"))
        .await?;
    sync.accept_connections().await?;
    Ok(Address::http(sync.get_server_address().await?))
}

#[test]
fn fixed_call_cross_peer_reopens_exact_wait_without_reissuing_effect() -> Result<()> {
    // Debug Eidetica's native HTTP bootstrap is deeper than libtest's stack
    // (it overflows while polling the reply's oneshot). Scope extra stack to
    // this fixture rather than changing the process-wide test configuration.
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(45), round_trip()).await?
                })
        })?
        .join()
        .expect("fixed call peer fixture panicked")
}

async fn round_trip() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("executor.db").display()
    );
    let (left, mut owner) = Instance::create_backend(
        Box::new(Sqlite::connect(&url).await?),
        NewAccount::with_password("left", PASSWORD),
    )
    .await?;
    let fx = Fixture::new(dir.path(), &mut owner).await?;
    let forger = PrivateKey::generate();
    let forger_pk = forger.public_key();
    grant(&fx.admin, &forger_pk, Permission::Write(4)).await?;
    let unrelated = PrivateKey::generate().public_key();
    grant(&fx.admin, &unrelated, Permission::Read).await?;
    install(&fx.admin, &fx.binding).await?;
    let requester_key = owner
        .import_private_key(fx.requester.clone(), Some("executor"))
        .await?;
    owner
        .map_key(
            &requester_key,
            fx.admin.root_id(),
            SigKey::from_pubkey(&requester_key),
        )
        .await?;
    assert_eq!(
        fx.admin
            .get_settings()
            .await?
            .get_auth_key(&unrelated)
            .await?
            .permissions(),
        &Permission::Read
    );
    fx.admin.share().await?;

    let txn = fx
        .admin
        .clone()
        .with_key(forger.clone())
        .new_transaction()
        .await?;
    let mut wrong = fx.binding.clone();
    wrong.resource.channel = "wrong-resource".into();
    txn.get_store::<DocStore>(SETTINGS)
        .await?
        .set_string(POLICY, serde_json::to_string(&wrong)?)
        .await?;
    let denied = txn.commit().await.unwrap_err();
    assert!(matches!(denied, eidetica::Error::Transaction(ref error)
        if matches!(error.as_ref(), eidetica::transaction::TransactionError::EntryValidationFailed)));
    assert!(
        FixedCall::open_bound(fx.requester(), wrong.clone())
            .await
            .is_err()
    );
    assert!(
        FixedCall::open_bound(
            fx.admin.clone().with_key(forger.clone()),
            fx.binding.clone()
        )
        .await
        .is_err()
    );

    let (call, context) = fx.pending().await?;
    let deadline = Utc::now() + TimeDelta::seconds(30);
    let mut a = FixedCall::open(&owner, fx.binding.clone(), &requester_key).await?;
    let original_records = pending_records(&a.observe().await?.txn, &call).await?;
    let wrong_target = Request {
        binding: wrong,
        call: call.clone(),
        arguments: "{}".into(),
        context: context.clone(),
        transcript_digest: digest(&original_records)?,
        issued_at: Utc::now(),
        deadline,
    };
    let wrong_request = publish(&fx.requester(), Record::Request(Box::new(wrong_target))).await?;
    assert!(fx.admin.verify_entry_signature(&wrong_request).await?);
    assert_eq!(
        FixedCall::open_bound(fx.fulfiller(), fx.binding.clone())
            .await?
            .fulfill(&fx.journal, |_| async {
                panic!("wrong target reached the provider")
            })
            .await?,
        None
    );
    let id = a
        .issue(call.clone(), "{}".into(), context.clone(), deadline)
        .await?;
    assert!(
        a.issue(call.clone(), "{}".into(), context, deadline)
            .await
            .is_err()
    );
    let (_, issued) = a.observe().await?.request.unwrap();
    let writer = fx.admin.clone().with_key(forger);
    let forged_request = publish(&writer, Record::Request(Box::new(issued))).await?;
    let forged_answer = publish(
        &writer,
        Record::Answer {
            request: id.clone(),
            answer: Answer::Reply("forged".into()),
        },
    )
    .await?;
    assert!(fx.admin.verify_entry_signature(forged_request).await?);
    assert!(fx.admin.verify_entry_signature(forged_answer).await?);
    assert_eq!(a.resume(&call).await?, Wait::Pending { deadline });
    let mut different = call.clone();
    different.execution_id = Some("another-branch".into());
    assert!(a.resume(&different).await.is_err());

    let (right, mut provider) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("right", PASSWORD),
    )
    .await?;
    let left_sync = Sync::new(left.clone()).await?;
    let address = listen(&left_sync).await?;
    let session_id = fx.admin.root_id().clone();
    let acquisition = Sync::new(right.clone()).await?;
    acquisition
        .register_transport("http", HttpTransport::builder())
        .await?;
    acquisition
        .sync_with_peer_for_bootstrap_with_key(
            &address,
            &session_id,
            &fx.fulfiller,
            "fixture-provider",
            Permission::Write(10),
        )
        .await
        .context("acquire exact request on provider peer")?;
    let provider_key = provider
        .import_private_key(fx.fulfiller.clone(), Some("provider"))
        .await?;
    provider
        .track_database(
            session_id.clone(),
            &provider_key,
            eidetica::account::SyncSettings::default(),
        )
        .await?;
    let right_db = provider
        .open_database_with_key(&session_id, &provider_key)
        .await?;
    right_db.share().await?;
    let right_sync = Sync::new(right.clone()).await?;
    let right_address = listen(&right_sync).await?;
    let b = FixedCall::open(&provider, fx.binding.clone(), &provider_key).await?;
    publish(
        &right_db,
        Record::Answer {
            request: wrong_request,
            answer: Answer::Reply("wrong request".into()),
        },
    )
    .await?;
    publish(
        &right_db,
        Record::Answer {
            request: id.clone(),
            answer: Answer::Reply("x".repeat(MAX_RESULT_BYTES + 1)),
        },
    )
    .await?;

    // A's actor and SQLite backend stop. The provider has one offline start;
    // neither a shared account/key nor a second context/transport is needed.
    left_sync.stop_server().await?;
    let Fixture {
        admin,
        requester,
        fulfiller,
        binding,
        journal,
    } = fx;
    drop((a, admin, fulfiller, writer, left_sync, owner, left));
    let invoked = AtomicUsize::new(0);
    let marker = "marker-only-the-provider-can-return";
    let marker_file = dir.path().join("provider-marker");
    std::fs::write(&marker_file, marker)?;
    let count = &invoked;
    assert_eq!(
        b.fulfill(&journal, |args| async move {
            assert_eq!(args, "{}");
            count.fetch_add(1, Ordering::SeqCst);
            Answer::Reply(std::fs::read_to_string(marker_file).unwrap())
        })
        .await?,
        Some(Answer::Reply(marker.into()))
    );
    assert_eq!(
        b.fulfill(&journal, |_| async {
            panic!("duplicate hint must not execute")
        })
        .await?,
        None
    );
    assert_eq!(invoked.load(Ordering::SeqCst), 1);

    let left = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match Instance::connect(&url).await {
                Ok(reopened) => break reopened,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
    })
    .await?;
    let owner = left.login_account("left", PASSWORD).await?;
    let mut a = FixedCall::open(&owner, binding, &requester_key).await?;
    let reopened = Session::new(ConversationId(session_id.to_string()), a.db.clone()).await;
    let interrupted = reopened.turn_requests(|_| false, &HashSet::new()).await?;
    let [original] = interrupted.as_slice() else {
        bail!("expected one interrupted turn")
    };
    let TurnRequestState::Interrupted { attempt_id } = &original.state else {
        bail!("expected the original interrupted attempt")
    };
    let restored = a
        .pending_call(&original.id, attempt_id)
        .await?
        .context("missing exact durable wait")?;
    assert_eq!(restored, call);
    assert_eq!(a.resume(&restored).await?, Wait::Pending { deadline });
    let left_sync = Sync::new(left.clone()).await?;
    left_sync
        .register_transport("http", HttpTransport::builder())
        .await?;
    left_sync
        .sync_with_peer_as(&right_address, Some(&session_id), Some(&requester))
        .await
        .context("reconcile missed provider result after SQLite reopen")?;
    assert_eq!(
        a.resume(&restored).await?,
        Wait::Paired(Answer::Reply(marker.into()))
    );
    assert_eq!(a.resume(&restored).await?, Wait::Settled);
    assert!(a.pending_call(&original.id, attempt_id).await?.is_none());
    assert_eq!(
        a.observe().await?.request.unwrap().0,
        id,
        "no reissued request"
    );
    let reopened = Session::new(ConversationId(session_id.to_string()), a.db.clone()).await;
    assert_eq!(
        reopened.entries().len(),
        1,
        "no Message/Directive or outbox entry imported"
    );
    let transcript = reopened.turn_transcript(&call.attempt_id).await?;
    assert_eq!(transcript.len(), 2);
    assert!(matches!(&transcript[1].message,
        TurnTranscriptMessage::ToolResult { call_id, output, outcome: ToolResultOutcome::Success, .. }
        if *call_id == call.call_id && output == marker));
    assert_eq!(
        transcript[1].identity.as_ref().unwrap().execution_id,
        call.execution_id
    );
    right_sync.stop_server().await?;
    Ok(())
}

#[tokio::test]
async fn fixed_call_retains_unknown_after_durable_start_and_refuses_lost_journal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (_instance, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("test", PASSWORD),
    )
    .await?;
    let fx = Fixture::new(dir.path(), &mut user).await?;
    install(&fx.admin, &fx.binding).await?;
    let (call, context) = fx.pending().await?;
    let mut a = FixedCall::open_bound(fx.requester(), fx.binding.clone()).await?;
    let b = FixedCall::open_bound(fx.fulfiller(), fx.binding.clone()).await?;
    let id = a
        .issue(
            call.clone(),
            "{}".into(),
            context,
            Utc::now() + TimeDelta::seconds(30),
        )
        .await?;
    // Simulate interruption at the concrete durable start boundary, with no
    // completion in either the journal or the shared session.
    {
        let mut journal = journal::Journal::open(&fx.journal, &fx.binding)?;
        journal.start(&id)?;
    }
    assert!(
        b.fulfill(&dir.path().join("missing"), |_| async {
            panic!("lost journal must not authorize replay")
        })
        .await
        .is_err()
    );
    assert_eq!(
        b.fulfill(&fx.journal, |_| async {
            panic!("ambiguous start must never run again")
        })
        .await?,
        Some(Answer::Unknown)
    );
    assert_eq!(a.resume(&call).await?, Wait::Paired(Answer::Unknown));
    Ok(())
}

#[tokio::test]
async fn fixed_call_service_distinguishes_session_write_from_metadata_read_and_old_signatures()
-> Result<()> {
    use eidetica::service::ServiceServer;
    let dir = tempfile::tempdir()?;
    let (instance, mut admin) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("admin", PASSWORD),
    )
    .await?;
    let fx = Fixture::new(dir.path(), &mut admin).await?;
    install(&fx.admin, &fx.binding).await?;
    let reader = PrivateKey::generate();
    let metadata = admin
        .create_database(Doc::new(), &admin.get_default_key()?)
        .await?;
    grant(&metadata, &reader.public_key(), Permission::Read).await?;
    admin
        .admin()
        .await?
        .create_account(NewAccount::with_password("client", PASSWORD))
        .await?;
    let socket = dir.path().join("proof.sock");
    let service = ServiceServer::bind(instance.clone(), &socket).await?;
    let (shutdown, receiver) = tokio::sync::watch::channel(());
    let task = tokio::spawn(service.run(receiver));
    let client = Instance::connect(format!("unix://{}", socket.display())).await?;
    let mut user = client.login_account("client", PASSWORD).await?;
    let reader = user
        .import_private_key(reader, Some("metadata-reader"))
        .await?;
    for root in [metadata.root_id(), fx.admin.root_id()] {
        user.map_key(&reader, root, SigKey::from_pubkey(&reader))
            .await?;
    }
    let read = user
        .open_database_with_key(metadata.root_id(), &reader)
        .await?;
    assert_eq!(read.current_permission().await?, Permission::Read);
    let denied = user
        .open_database_with_key(fx.admin.root_id(), &reader)
        .await
        .unwrap_err();
    assert!(
        denied.to_string().contains("[auth::PermissionDenied]"),
        "{denied}"
    );
    assert!(
        denied.to_string().contains(&fx.admin.root_id().to_string()),
        "{denied}"
    );

    let requester = user
        .import_private_key(fx.requester.clone(), Some("executor"))
        .await?;
    user.map_key(
        &requester,
        fx.admin.root_id(),
        SigKey::from_pubkey(&requester),
    )
    .await?;
    let service_db = user
        .open_database_with_key(fx.admin.root_id(), &requester)
        .await?;
    assert!(service_db.current_permission().await?.can_write());
    let mut a = FixedCall::open(&user, fx.binding.clone(), &requester).await?;
    let (call, context) = fx.pending().await?;
    let deadline = Utc::now() + TimeDelta::seconds(30);
    let id = a
        .issue(call.clone(), "{}".into(), context, deadline)
        .await?;
    assert_eq!(a.resume(&call).await?, Wait::Pending { deadline });
    let b = FixedCall::open_bound(fx.fulfiller(), fx.binding.clone()).await?;
    // The provider stopped after persisting the screened result but before
    // publishing it. Reconciliation must use the saved bytes, not the adapter.
    {
        let mut journal = journal::Journal::open(&fx.journal, &fx.binding)?;
        journal.start(&id)?;
        journal.save(&id, &Answer::Reply("screened".into()))?;
    }
    assert_eq!(
        b.fulfill(&fx.journal, |_| async {
            panic!("saved result must not repeat the effect")
        })
        .await?,
        Some(Answer::Reply("screened".into()))
    );
    assert_eq!(
        a.observe().await?.answer,
        Some(Answer::Reply("screened".into()))
    );

    // The requester still has native Write and the historical request can
    // still verify over the service. Neither proves the other operator's Write.
    grant(&fx.admin, &fx.fulfiller.public_key(), Permission::Read).await?;
    assert!(service_db.verify_entry_signature(id).await?);
    assert!(service_db.current_permission().await?.can_write());
    let error = a.resume(&call).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("fixed operator lacks current direct Write"),
        "{error}"
    );
    assert!(
        b.fulfill(&fx.journal, |_| async {
            panic!("Read must not authorize an effect")
        })
        .await
        .is_err()
    );
    let session = Session::new(ConversationId(fx.binding.session.to_string()), service_db).await;
    assert_eq!(session.turn_transcript(&call.attempt_id).await?.len(), 1);
    assert!(
        install(&fx.admin, &fx.binding).await.is_err(),
        "recovery cannot restore lost Write"
    );
    drop(shutdown);
    task.await??;
    Ok(())
}

#[tokio::test]
async fn fixed_call_refuses_descendant_install_and_does_not_act_on_cancellation() -> Result<()> {
    use eidetica::auth::types::{DelegatedTreeRef, PermissionBounds, TreeReference};
    let dir = tempfile::tempdir()?;
    let (_instance, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("test", PASSWORD),
    )
    .await?;
    let fx = Fixture::new(dir.path(), &mut user).await?;
    install(&fx.admin, &fx.binding).await?;
    let child = user
        .create_database(Doc::new(), &user.get_default_key()?)
        .await?;
    let txn = child.new_transaction().await?;
    txn.get_settings()?
        .add_delegated_tree(DelegatedTreeRef {
            tree: TreeReference {
                root: fx.admin.root_id().clone(),
                tips: fx.admin.snapshot().await?.into_tips(),
            },
            permission_bounds: PermissionBounds {
                max: Permission::Admin(0),
                min: None,
            },
        })
        .await?;
    // Even a mutable _settings copy of the marker is not a root issuance.
    txn.get_store::<DocStore>(SETTINGS)
        .await?
        .set(ROOT_MARKER, true)
        .await?;
    txn.commit().await?;
    let child_binding = Binding {
        session: child.root_id().clone(),
        ..fx.binding.clone()
    };
    let error = install(&child, &child_binding).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not a provisioned fixed-call root"),
        "{error}"
    );

    let (call, context) = fx.pending().await?;
    let mut a = FixedCall::open_bound(fx.requester(), fx.binding.clone()).await?;
    let b = FixedCall::open_bound(fx.fulfiller(), fx.binding.clone()).await?;
    a.issue(
        call.clone(),
        "{}".into(),
        context,
        Utc::now() + TimeDelta::seconds(30),
    )
    .await?;
    a.cancel(&call).await?;
    assert_eq!(
        b.fulfill(&fx.journal, |_| async {
            panic!("cancelled call must not act")
        })
        .await?,
        None
    );
    assert_eq!(a.resume(&call).await?, Wait::Cancelled);
    assert_eq!(a.resume(&call).await?, Wait::Settled);
    Ok(())
}
