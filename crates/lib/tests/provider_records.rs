//! Native Provider record proof, not a Provider protocol implementation.
//! Disposable databases/keys; listeners are loopback HTTP and temporary Unix sockets.
//! Historical verification is distinct from current authority and concrete signer attribution.

use anyhow::Context;
use eidetica::{
    Database, Entry, Instance, NewAccount, PublicKey,
    account::AccountSession,
    auth::{
        crypto::{PrivateKey, verify_entry_signature},
        types::{
            AuthKey, DelegatedTreeRef, DelegationStep, KeyHint, Permission, PermissionBounds,
            SigKey, TreeReference,
        },
        validation::AuthValidator,
    },
    backend::{
        CacheScope, ProjectionDescriptor, StoreStateLifecycle, StoreStateRequest,
        VerificationStatus, database::InMemory,
    },
    crdt::Doc,
    database::DatabaseKey,
    entry::ID,
    service::{ServiceServer, protocol::ReadScope},
    store::{DocStore, SettingsStore},
    sync::{Address, Sync, transports::http::HttpTransport},
};
use tokio::sync::{mpsc, watch};

const PASSWORD: &str = "test-password";

async fn grant(
    settings: &SettingsStore,
    keys: &[&PrivateKey],
    name: Option<&str>,
    permission: Permission,
) -> eidetica::Result<()> {
    for key in keys {
        settings
            .set_auth_key(&key.public_key(), AuthKey::active(name, permission))
            .await?;
    }
    Ok(())
}

async fn grant_session_access(
    session: &Database,
    provider: &Database,
    writers: &[&PrivateKey],
    readers: &[&PrivateKey],
) -> eidetica::Result<DelegationStep> {
    let tree = provider.root_id().clone();
    let tips = provider.snapshot().await?.into_tips();
    let tx = session.new_transaction().await?;
    let settings = tx.get_settings()?;
    grant(&settings, writers, None, Permission::Write(10)).await?;
    grant(&settings, readers, None, Permission::Read).await?;
    settings
        .add_delegated_tree(DelegatedTreeRef {
            permission_bounds: PermissionBounds {
                max: Permission::Write(10),
                min: None,
            },
            tree: TreeReference {
                root: tree.clone(),
                tips: tips.clone(),
            },
        })
        .await?;
    tx.commit().await?;
    Ok(DelegationStep { tree, tips })
}

struct ServiceFixture {
    client: Instance,
    shutdown: watch::Sender<()>,
    task: tokio::task::JoinHandle<eidetica::Result<()>>,
    _dir: tempfile::TempDir,
}

impl ServiceFixture {
    async fn start(
        host: &Instance,
        owner: &AccountSession,
        keys: &[&PrivateKey],
    ) -> anyhow::Result<Self> {
        owner
            .admin()
            .await?
            .create_account(NewAccount::with_password("observer", PASSWORD))
            .await?;
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("service.sock");
        let server = ServiceServer::bind(host.clone(), &socket).await?;
        let (shutdown, receiver) = watch::channel(());
        let task = tokio::spawn(server.run(receiver));
        let client = Instance::connect(format!("unix://{}", socket.display())).await?;
        let login = client.login_account("observer", PASSWORD).await?;
        let error = login
            .admin()
            .await
            .expect_err("observer is not a host admin");
        assert!(
            error.to_string().contains("[auth::PermissionDenied]"),
            "{error}"
        );
        // Register proof of possession without granting access to either database.
        for &key in keys {
            Database::create(&client, key.clone(), Doc::new()).await?;
        }
        Ok(Self {
            client,
            shutdown,
            task,
            _dir: dir,
        })
    }

    async fn stop(self) -> anyhow::Result<()> {
        drop(self.shutdown);
        self.task.await??;
        Ok(())
    }
}

async fn provider_operators(
    provider: &Database,
    keys: [&PrivateKey; 2],
) -> eidetica::Result<[PublicKey; 2]> {
    let operators = keys.map(PrivateKey::public_key);
    let auth = provider.get_settings().await?.auth_snapshot().await?;
    for key in &operators {
        let grant = auth.get_key_by_pubkey(key)?;
        assert!(grant.is_active() && grant.permissions().can_write());
    }
    Ok(operators)
}

async fn publish(db: &Database, key: DatabaseKey, provider: &ID) -> eidetica::Result<ID> {
    let tx = db.clone().with_key(key).new_transaction().await?;
    let store = tx.get_store::<DocStore>("records").await?;
    // These labels are data, not proof of who wrote the entry.
    store.set("provider", provider.to_string()).await?;
    store.set("sender", provider.to_string()).await?;
    tx.commit().await
}

async fn records(db: &Database) -> eidetica::Result<Vec<Entry>> {
    // Resolve the store frontier from one verified main frontier, including
    // after settings-only commits. ReadScope alone does not filter raw tips.
    let context = db
        .transaction_context(&["records".into()], ReadScope::Verified)
        .await?;
    let tips: Vec<_> = context.subtree_parents["records"]
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    db.get_store_entries("records", &tips, ReadScope::Verified)
        .await
}

fn record_ids(entries: &[Entry]) -> Vec<ID> {
    let mut ids: Vec<_> = entries.iter().map(Entry::id).collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn native_provider_service_parity() -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), service_parity()).await??;
    Ok(())
}

async fn service_parity() -> anyhow::Result<()> {
    let (owner, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("proof", PASSWORD),
    )
    .await?;
    let admin = user.get_default_key()?;
    let provider = user.create_database(Doc::new(), &admin).await?;
    let session = user.create_database(Doc::new(), &admin).await?;
    let direct = PrivateKey::generate();
    let delegated = PrivateKey::generate();
    let forger = PrivateKey::generate();
    let reader = PrivateKey::generate();
    let target_only = PrivateKey::generate();

    let tx = provider.new_transaction().await?;
    let settings = tx.get_settings()?;
    grant(
        &settings,
        &[&direct, &delegated],
        Some("operator"),
        Permission::Write(10),
    )
    .await?;
    grant(&settings, &[&reader], None, Permission::Read).await?;
    tx.commit().await?;
    let step = grant_session_access(
        &session,
        &provider,
        &[&direct, &forger],
        &[&reader, &target_only],
    )
    .await?;

    let direct_id = publish(
        &session,
        DatabaseKey::new(direct.clone()),
        provider.root_id(),
    )
    .await?;
    let forged_id = publish(
        &session,
        DatabaseKey::new(forger.clone()),
        provider.root_id(),
    )
    .await?;
    let identity = SigKey::Delegation {
        path: vec![step],
        hint: KeyHint::from_name("operator"),
    };
    let delegated_id = publish(
        &session,
        DatabaseKey::with_identity(delegated.clone(), identity.clone()),
        provider.root_id(),
    )
    .await?;

    let service =
        ServiceFixture::start(&owner, &user, &[&reader, &target_only, &delegated]).await?;
    let remote = &service.client;
    assert_permission_denied(
        Database::open(remote, session.root_id()).await.unwrap_err(),
        session.root_id(),
        "Read",
    );
    let remote_session = open_as(remote, &session, &reader).await?;
    let remote_provider = open_as(remote, &provider, &reader).await?;

    // The same server-verified Entry bytes reach both readers. No callback tips
    // or plain provider/sender field is used as the authorization boundary.
    let local_records = records(&session).await?;
    assert_eq!(local_records, records(&remote_session).await?);
    let mut expected = vec![direct_id.clone(), delegated_id.clone()];
    expected.sort();
    let mut all = expected.clone();
    all.push(forged_id.clone());
    all.sort();
    assert_eq!(record_ids(&local_records), all);
    for (db, authority) in [(&session, &provider), (&remote_session, &remote_provider)] {
        let operators = provider_operators(authority, [&direct, &delegated]).await?;
        let forged = db.get_entry(&forged_id).await?;
        assert!(
            db.verify_entry_signature(&forged_id).await?,
            "forger is a valid session writer"
        );
        assert!(verify_entry_signature(&forged, &forger.public_key()).is_ok());
        let claims: Doc = serde_json::from_slice(forged.data("records")?)?;
        let claimed_provider = provider.root_id().to_string();
        for field in ["provider", "sender"] {
            assert_eq!(
                claims.get_as::<&str>(field),
                Some(claimed_provider.as_str())
            );
        }
        assert_eq!(
            issued_records(db, &operators).await?,
            expected,
            "a valid session writer is not necessarily a Provider operator"
        );
    }

    // Native delegation resolves to an actual operating key, not a trusted hint.
    let entry = session.get_entry(&delegated_id).await?;
    assert!(session.verify_entry_signature(&delegated_id).await?);
    let auth = session.get_settings().await?.auth_snapshot().await?;
    let candidates = AuthValidator::new()
        .resolve_sig_key(&entry.auth().key, &auth, Some(&owner))
        .await?;
    assert_eq!(candidates.len(), 2, "shared hint is not a concrete signer");
    let signers: Vec<_> = candidates
        .iter()
        .filter(|candidate| verify_entry_signature(&entry, &candidate.public_key).is_ok())
        .collect();
    assert_eq!(signers.len(), 1);
    assert_eq!(signers[0].public_key, delegated.public_key());
    assert!(signers[0].effective_permission.can_write());

    // The historical verdict runs natively on the service using the bound Read
    // actor. Neither a shared name nor a provider label is signer attribution.
    assert!(remote_session.verify_entry_signature(&delegated_id).await?);
    let remote_entry = remote_session.get_entry(&delegated_id).await?;
    assert!(verify_entry_signature(&remote_entry, &delegated.public_key()).is_ok());
    assert!(verify_entry_signature(&remote_entry, &direct.public_key()).is_err());
    let remote_reader = remote_session.clone().with_key(reader.clone());
    assert_eq!(remote_reader.current_permission().await?, Permission::Read);
    let write_error = publish(&remote_reader, DatabaseKey::new(reader), provider.root_id())
        .await
        .unwrap_err();
    assert!(
        matches!(write_error, eidetica::Error::Transaction(ref error)
        if matches!(error.as_ref(), eidetica::transaction::TransactionError::EntryValidationFailed)),
        "Read is not Write: {write_error}"
    );
    let target = open_as(remote, &session, &target_only).await?;
    assert!(target.verify_entry_signature(&direct_id).await?);
    let proof_error = target
        .verify_entry_signature(&delegated_id)
        .await
        .unwrap_err();
    assert_permission_denied(proof_error, provider.root_id(), "Read");
    assert_permission_denied(
        open_as(remote, &provider, &target_only).await.unwrap_err(),
        provider.root_id(),
        "Read",
    );
    assert!(
        remote_session
            .clone()
            .with_key(PrivateKey::generate())
            .verify_entry_signature(&delegated_id)
            .await?,
        "attached key must not replace the service handle's bound reader"
    );
    let unregistered_error = remote
        .remote_connection()
        .unwrap()
        .verify_entry_signature(
            session.root_id().clone(),
            SigKey::from_pubkey(&PrivateKey::generate().public_key()),
            delegated_id.clone(),
        )
        .await
        .unwrap_err();
    assert!(
        unregistered_error
            .to_string()
            .contains("[auth::SigningKeyMismatch]")
            && unregistered_error
                .to_string()
                .contains("not in the session keyset"),
        "unregistered actor must fail proof of possession: {unregistered_error}"
    );

    // Historical service verification does not add a remote current-permission
    // resolver. Keep Chaz's observer guard until that distinct API is supported.
    let local_delegate = session.clone().with_key(DatabaseKey::with_identity(
        delegated.clone(),
        identity.clone(),
    ));
    assert_eq!(
        chaz_core::session::jobs::require_observer_write(&local_delegate).await?,
        Permission::Write(10)
    );
    // A service acting identity carries the concrete registered public key;
    // the historical entry above deliberately uses an ambiguous name instead.
    let SigKey::Delegation { path, .. } = identity else {
        unreachable!()
    };
    let query_identity = SigKey::Delegation {
        path,
        hint: KeyHint::from_pubkey(&delegated.public_key()),
    };
    let remote_delegate = Database::open_remote(
        remote,
        remote.remote_connection().unwrap().clone(),
        session.root_id(),
        query_identity.clone(),
    )
    .await?
    .with_key(DatabaseKey::with_identity(delegated, query_identity));
    let error = remote_delegate.current_permission().await.unwrap_err();
    assert!(
        matches!(error, eidetica::Error::Auth(ref error)
        if matches!(error.as_ref(), eidetica::auth::AuthError::InvalidAuthConfiguration { reason }
            if reason.contains("Operation not supported: local backend engine on remote instance"))),
        "{error}"
    );
    assert!(
        chaz_core::session::jobs::require_observer_write(&remote_delegate)
            .await
            .unwrap_err()
            .to_string()
            .contains("native permission query is unsupported")
    );

    service.stop().await
}

fn assert_permission_denied(error: eidetica::Error, tree: &ID, required: &str) {
    // Service auth errors retain their discriminant in an I/O envelope.
    let message = error.to_string();
    assert!(
        message.contains("[auth::PermissionDenied]")
            && message.contains(&tree.to_string())
            && message.contains(&format!("not permitted for {required}")),
        "expected {required} denial on {tree}: {error}"
    );
}

/// Exercise Read authorization for private derived-cache staging. The service
/// scopes this request to the login. Abort without publishing any cache state.
async fn stage_private_cache(
    remote: &Instance,
    db: &Database,
    identity: &SigKey,
) -> eidetica::Result<()> {
    let connection = remote.remote_connection().unwrap();
    let token = connection
        .begin_store_state_staging(
            identity.clone(),
            StoreStateRequest {
                database: db.root_id().clone(),
                store: "records".into(),
                lifecycle: StoreStateLifecycle::Derived,
                scope: CacheScope::Shared,
                projection: ProjectionDescriptor {
                    name: "provider-proof".into(),
                    version: 1,
                },
                source_key: b"disposable-proof".to_vec(),
            },
        )
        .await?;
    connection
        .abort_store_state(db.root_id().clone(), identity.clone(), token)
        .await
        .expect("authorized staging must be cleaned up");
    Ok(())
}

async fn open_as(remote: &Instance, db: &Database, key: &PrivateKey) -> eidetica::Result<Database> {
    Database::open_remote(
        remote,
        remote.remote_connection().unwrap().clone(),
        db.root_id(),
        SigKey::from_pubkey(&key.public_key()),
    )
    .await
}

#[tokio::test]
async fn native_provider_peer_reconciliation() -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), peer_reconciliation()).await??;
    Ok(())
}

async fn peer_reconciliation() -> anyhow::Result<()> {
    let (source, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("source", PASSWORD),
    )
    .await?;
    let admin = user.get_default_key()?;
    let provider = user.create_database(Doc::new(), &admin).await?;
    let session = user.create_database(Doc::new(), &admin).await?;
    let direct = PrivateKey::generate();
    let delegated = PrivateKey::generate();
    let reader = PrivateKey::generate();
    let operator_keys = [&direct, &delegated];
    let tx = provider.new_transaction().await?;
    let settings = tx.get_settings()?;
    grant(&settings, &[&direct], None, Permission::Write(10)).await?;
    grant(&settings, &[&reader], None, Permission::Read).await?;
    tx.commit().await?;
    grant_session_access(&session, &provider, &[&direct], &[&reader]).await?;
    // Explicit fixture Read grants are not operator-only membership policy.
    session.share().await?;
    provider.share().await?;

    let (replica, account) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("replica", PASSWORD),
    )
    .await?;
    let serving = Sync::new(source.clone()).await?;
    serving
        .register_transport("http", HttpTransport::builder().bind("127.0.0.1:0"))
        .await?;
    serving.accept_connections().await?;
    let address = Address::http(serving.get_server_address().await?);
    let receiving = Sync::new(replica.clone()).await?;
    receiving
        .register_transport("http", HttpTransport::builder())
        .await?;
    // Native explicit dependency-first acquisition; no discovery/scheduler layer.
    receiving
        .sync_with_peer_for_bootstrap_with_key(
            &address,
            provider.root_id(),
            &reader,
            "reader",
            Permission::Read,
        )
        .await
        .context("bootstrap initial Provider authority")?;
    receiving
        .sync_with_peer_for_bootstrap_with_key(
            &address,
            session.root_id(),
            &reader,
            "reader",
            Permission::Read,
        )
        .await
        .context("bootstrap session after initial Provider authority")?;
    let local = Database::open(&replica, session.root_id()).await?;
    assert!(records(&local).await?.is_empty());
    // A later operator grant has not reached the replica. Its signed bytes,
    // not just a validation label, will be acquired in the delayed-proof step.
    let tx = provider.new_transaction().await?;
    grant(
        &tx.get_settings()?,
        &[&delegated],
        None,
        Permission::Write(10),
    )
    .await?;
    let missing_proof = tx.commit().await?;
    let operators = provider_operators(&provider, operator_keys).await?;
    let identity = SigKey::Delegation {
        path: vec![DelegationStep {
            tree: provider.root_id().clone(),
            tips: provider.snapshot().await?.into_tips(),
        }],
        hint: KeyHint::from_pubkey(&delegated.public_key()),
    };
    assert!(
        replica
            .backend()
            .get(&missing_proof)
            .await
            .unwrap_err()
            .is_not_found()
    );

    let service =
        ServiceFixture::start(&replica, &account, &[&reader, &direct, &delegated]).await?;
    let remote = &service.client;
    let service_session = open_as(remote, &local, &reader).await?;
    let initial = local.snapshot().await?;
    let (local_tx, mut local_rx) = mpsc::unbounded_channel();
    let _local_watch = local
        .on_write(move |latest, _| {
            let _ = local_tx.send(latest.clone());
            async { Ok(()) }
        })
        .await?;
    let (remote_tx, mut remote_rx) = mpsc::unbounded_channel();
    let _remote_watch = service_session
        .on_write(move |latest, _| {
            let _ = remote_tx.send(latest.clone());
            async { Ok(()) }
        })
        .await
        .context("subscribe service with explicit reader")?;
    assert_eq!(local_rx.recv().await.unwrap(), initial);
    assert_eq!(remote_rx.recv().await.unwrap(), initial);

    let direct_id = publish(
        &session,
        DatabaseKey::new(direct.clone()),
        provider.root_id(),
    )
    .await?;
    let delegated_id = publish(
        &session,
        DatabaseKey::with_identity(delegated.clone(), identity.clone()),
        provider.root_id(),
    )
    .await?;
    // An actually invalid signature, not a manually asserted Failed status.
    let invalid = session
        .get_entry(&direct_id)
        .await?
        .with_auth(|auth| auth.signature = None);
    let invalid_id = local.insert_raw(invalid).await?;
    let engine = replica.backend().local_engine().unwrap();
    assert_eq!(
        engine.get_verification_status(&invalid_id).await?,
        VerificationStatus::Unverified
    );
    for db in [&local, &service_session] {
        assert!(issued_records(db, &operators).await?.is_empty());
    }
    receiving
        .sync_with_peer_as(&address, Some(session.root_id()), Some(&reader))
        .await?;
    assert_eq!(
        engine.get_verification_status(&invalid_id).await?,
        VerificationStatus::Failed
    );
    assert_eq!(
        engine.get_verification_status(&direct_id).await?,
        VerificationStatus::Verified
    );
    assert_eq!(
        engine.get_verification_status(&delegated_id).await?,
        VerificationStatus::Unverified
    );
    let before_local = local_rx.recv().await.unwrap();
    let before_remote = remote_rx.recv().await.unwrap();
    assert_eq!(before_local, before_remote);
    for db in [&local, &service_session] {
        let added = db.ids_added(&initial, &before_local).await?;
        assert!(added.contains(&direct_id));
        assert!(
            !added.contains(&invalid_id),
            "watch excludes Failed entries"
        );
        assert!(
            !added.contains(&delegated_id),
            "watch excludes Unverified entries"
        );
        assert_eq!(
            record_ids(&records(db).await?),
            vec![direct_id.clone()],
            "verified records exclude the Failed and Unverified entries"
        );
        assert_eq!(
            issued_records(db, &operators).await?,
            vec![direct_id.clone()]
        );
    }
    // Transfer the missing proof over the peer transport and confirm that
    // acquiring it alone does not promote the session entry.
    receiving
        .sync_with_peer_as(&address, Some(provider.root_id()), Some(&reader))
        .await
        .context("sync delayed Provider proof")?;
    assert_eq!(
        replica.backend().get(&missing_proof).await?.id(),
        missing_proof
    );
    assert_eq!(
        engine.get_verification_status(&delegated_id).await?,
        VerificationStatus::Unverified
    );
    // Reconcile only after this explicit pass; a snapshot read can itself retry verification.
    let report = local.verify().await?;
    assert_eq!(report.verified, 1, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    let after_local = local_rx.recv().await.unwrap();
    let after_remote = remote_rx.recv().await.unwrap();
    assert_eq!(after_local, after_remote);
    let mut expected = vec![direct_id.clone(), delegated_id.clone()];
    expected.sort();
    let local_provider = Database::open(&replica, provider.root_id()).await?;
    let remote_provider = open_as(remote, &local_provider, &reader).await?;
    for (db, authority) in [
        (&local, &local_provider),
        (&service_session, &remote_provider),
    ] {
        assert_eq!(
            provider_operators(authority, operator_keys).await?,
            operators
        );
        let added = db.ids_added(&before_local, &after_local).await?;
        assert!(
            added.contains(&delegated_id),
            "promotion changes the Verified view"
        );
        assert!(!added.contains(&invalid_id));
        assert_eq!(record_ids(&records(db).await?), expected);
        assert_eq!(issued_records(db, &operators).await?, expected);
        assert!(!db.verify_entry_signature(&invalid_id).await?);
    }
    assert_eq!(records(&local).await?, records(&service_session).await?);

    // Retain exactly the identity that signed the entry, including its old
    // Provider tips, across current-authority reductions.
    let acting = local.clone().with_key(DatabaseKey::with_identity(
        delegated.clone(),
        identity.clone(),
    ));
    let service_actor = Database::open_remote(
        remote,
        remote.remote_connection().unwrap().clone(),
        session.root_id(),
        identity.clone(),
    )
    .await?;
    assert_eq!(acting.current_permission().await?, Permission::Write(10));
    assert!(!service_actor.snapshot().await?.is_empty());
    stage_private_cache(remote, &local, &identity).await?;
    let direct_identity = SigKey::from_pubkey(&direct.public_key());

    // Check the downgrade on the local replica. Private cache staging is not
    // a Write probe: both the service read and staging must still work.
    let tx = provider.new_transaction().await?;
    tx.get_settings()?
        .set_auth_key(
            &delegated.public_key(),
            AuthKey::active(None, Permission::Read),
        )
        .await?;
    tx.commit().await?;
    assert_eq!(
        acting.current_permission().await?,
        Permission::Write(10),
        "the unsynchronized replica is honestly stale"
    );
    receiving
        .sync_with_peer_as(&address, Some(provider.root_id()), Some(&reader))
        .await?;
    assert_eq!(acting.current_permission().await?, Permission::Read);
    assert!(
        !service_actor.snapshot().await?.is_empty(),
        "Read survives downgrade"
    );
    stage_private_cache(remote, &local, &identity).await?;

    // The reader may still verify old issuance after an operator loses all
    // current Provider/session authority.
    let tx = provider.new_transaction().await?;
    for key in operator_keys {
        tx.get_settings()?
            .revoke_auth_key(&key.public_key())
            .await?;
    }
    tx.commit().await?;
    // Until this sync the replica has an honestly stale positive observation.
    assert_eq!(
        local_provider
            .clone()
            .with_key(direct.clone())
            .current_permission()
            .await?,
        Permission::Write(10)
    );
    receiving
        .sync_with_peer_as(&address, Some(provider.root_id()), Some(&reader))
        .await?;
    for db in [&local_provider, &remote_provider] {
        let auth = db.get_settings().await?.auth_snapshot().await?;
        for key in operator_keys {
            assert!(!auth.get_key_by_pubkey(&key.public_key())?.is_active());
            assert_key_not_found(db.clone().with_key(key.clone()).current_permission().await);
        }
    }
    assert!(matches!(acting.current_permission().await,
        Err(eidetica::Error::Auth(ref error))
        if matches!(error.as_ref(), eidetica::auth::AuthError::SigningKeyMismatch { .. })));
    assert_permission_denied(
        service_actor.snapshot().await.unwrap_err(),
        session.root_id(),
        "Read",
    );
    // A separate direct session grant is not the same as Provider membership.
    assert_eq!(
        local
            .clone()
            .with_key(direct.clone())
            .current_permission()
            .await?,
        Permission::Write(10)
    );
    stage_private_cache(remote, &local, &direct_identity).await?;
    let tx = session.new_transaction().await?;
    tx.get_settings()?
        .revoke_auth_key(&direct.public_key())
        .await?;
    tx.commit().await?;
    receiving
        .sync_with_peer_as(&address, Some(session.root_id()), Some(&reader))
        .await?;
    for db in [&local, &service_session] {
        assert_key_not_found(
            db.clone()
                .with_key(direct.clone())
                .current_permission()
                .await,
        );
        assert_eq!(
            issued_records(db, &operators).await?,
            expected,
            "historical issuance is not current eligibility"
        );
    }
    assert_permission_denied(
        stage_private_cache(remote, &local, &direct_identity)
            .await
            .unwrap_err(),
        session.root_id(),
        "Read",
    );
    // Client-side delegated current_permission remains unsupported (covered
    // above), so the production observer guard stays. The staging request
    // exercises the service's current Read gate independently.

    service.stop().await?;
    serving.stop_server().await?;
    Ok(())
}

/// An inert effect-selection probe, not a Provider implementation: these are
/// known fixture operating keys. Native historical validation plus concrete-key
/// matching is applied only to entries reconciled from a fresh verified view.
/// Current Provider/session permission must be checked separately before effects.
async fn issued_records(db: &Database, operators: &[PublicKey]) -> eidetica::Result<Vec<ID>> {
    let mut accepted = Vec::new();
    for entry in records(db).await? {
        if db.verify_entry_signature(entry.id()).await?
            && operators
                .iter()
                .any(|key| verify_entry_signature(&entry, key).is_ok())
        {
            accepted.push(entry.id());
        }
    }
    accepted.sort();
    Ok(accepted)
}

fn assert_key_not_found(result: eidetica::Result<Permission>) {
    assert!(
        matches!(result, Err(eidetica::Error::Auth(ref error))
            if matches!(error.as_ref(), eidetica::auth::AuthError::KeyNotFound { .. })),
        "expected native current-key denial, not a transport failure: {result:?}"
    );
}
