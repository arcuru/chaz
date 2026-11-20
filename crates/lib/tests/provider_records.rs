//! Native Provider record proof, not a Provider protocol implementation.
//! Disposable databases/keys; listeners are loopback HTTP and temporary Unix sockets.
//! Historical verification is distinct from current authority and concrete signer attribution.

use anyhow::Context;
use eidetica::{
    Database, Entry, Instance, NewAccount,
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
    store::DocStore,
    sync::{Address, Sync, transports::http::HttpTransport},
};
use tokio::sync::{mpsc, watch};

async fn publish(db: &Database, key: DatabaseKey, provider: &ID) -> eidetica::Result<ID> {
    let tx = db.clone().with_key(key).new_transaction().await?;
    let store = tx.get_store::<DocStore>("records").await?;
    // This label is data, not proof of who wrote it.
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

#[tokio::test]
async fn native_provider_service_parity() -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), proof()).await??;
    Ok(())
}

async fn proof() -> anyhow::Result<()> {
    let (owner, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("proof", "test-password"),
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
    for key in [&direct, &delegated] {
        tx.get_settings()?
            .set_auth_key(
                &key.public_key(),
                AuthKey::active(Some("operator"), Permission::Write(10)),
            )
            .await?;
    }
    tx.get_settings()?
        .set_auth_key(
            &reader.public_key(),
            AuthKey::active(None, Permission::Read),
        )
        .await?;
    tx.commit().await?;
    let provider_tips = provider.snapshot().await?.into_tips();
    let tx = session.new_transaction().await?;
    for key in [&direct, &forger] {
        tx.get_settings()?
            .set_auth_key(
                &key.public_key(),
                AuthKey::active(None, Permission::Write(10)),
            )
            .await?;
    }
    // Explicit Read grants are proof fixtures, not Provider membership policy.
    for key in [&reader, &target_only] {
        tx.get_settings()?
            .set_auth_key(&key.public_key(), AuthKey::active(None, Permission::Read))
            .await?;
    }
    tx.get_settings()?
        .add_delegated_tree(DelegatedTreeRef {
            permission_bounds: PermissionBounds {
                max: Permission::Write(10),
                min: None,
            },
            tree: TreeReference {
                root: provider.root_id().clone(),
                tips: provider_tips.clone(),
            },
        })
        .await?;
    tx.commit().await?;

    let direct_id = publish(
        &session,
        DatabaseKey::new(direct.clone()),
        provider.root_id(),
    )
    .await?;
    let forged_id = publish(&session, DatabaseKey::new(forger), provider.root_id()).await?;
    let identity = SigKey::Delegation {
        path: vec![DelegationStep {
            tree: provider.root_id().clone(),
            tips: provider_tips,
        }],
        hint: KeyHint::from_name("operator"),
    };
    let delegated_id = publish(
        &session,
        DatabaseKey::with_identity(delegated.clone(), identity.clone()),
        provider.root_id(),
    )
    .await?;

    user.admin()
        .await?
        .create_account(NewAccount::with_password("reader", "test-password"))
        .await?;
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join("proof.sock");
    let service = ServiceServer::bind(owner.clone(), &socket).await?;
    let (shutdown, receiver) = watch::channel(());
    let running = tokio::spawn(service.run(receiver));
    let remote = Instance::connect(format!("unix://{}", socket.display())).await?;
    let login = remote.login_account("reader", "test-password").await?;
    let admin_error = login.admin().await.expect_err("reader is not a host admin");
    assert!(
        admin_error.to_string().contains("[auth::PermissionDenied]"),
        "non-admin must be denied by authorization: {admin_error}"
    );
    assert_read_denied(
        Database::open(&remote, session.root_id())
            .await
            .unwrap_err(),
        session.root_id(),
    );
    // Register proof of possession for the per-database actors, not extra grants.
    for key in [&reader, &target_only, &delegated] {
        Database::create(&remote, key.clone(), Doc::new()).await?;
    }
    let remote_session = open_as(&remote, &session, &reader).await?;
    let remote_provider = open_as(&remote, &provider, &reader).await?;

    // The same server-verified Entry bytes reach both readers. No callback tips
    // or plain provider/sender field is used as the authorization boundary.
    let local_records = records(&session).await?;
    let remote_records = records(&remote_session).await?;
    assert_eq!(
        serde_json::to_value(&local_records)?,
        serde_json::to_value(&remote_records)?
    );
    assert_eq!(local_records.len(), 3);
    for (db, authority) in [(&session, &provider), (&remote_session, &remote_provider)] {
        let direct_entry = db.get_entry(&direct_id).await?;
        assert!(db.verify_entry_signature(&direct_id).await?);
        assert!(verify_entry_signature(&direct_entry, &direct.public_key()).is_ok());
        let provider_auth = authority.get_settings().await?.auth_snapshot().await?;
        assert!(
            provider_auth
                .get_key_by_pubkey(&direct.public_key())?
                .permissions()
                .can_write()
        );
        let forged = db.get_entry(&forged_id).await?;
        assert!(
            db.verify_entry_signature(&forged_id).await?,
            "forger is a valid session writer"
        );
        assert!(verify_entry_signature(&forged, &direct.public_key()).is_err());
        assert!(verify_entry_signature(&forged, &delegated.public_key()).is_err());
        let mut expected = vec![direct_id.clone(), delegated_id.clone()];
        expected.sort();
        assert_eq!(issued_records(db, &[&direct, &delegated]).await?, expected);
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
    let target = open_as(&remote, &session, &target_only).await?;
    assert!(target.verify_entry_signature(&direct_id).await?);
    let proof_error = target
        .verify_entry_signature(&delegated_id)
        .await
        .unwrap_err();
    assert_read_denied(proof_error, provider.root_id());
    assert_read_denied(
        open_as(&remote, &provider, &target_only).await.unwrap_err(),
        provider.root_id(),
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
        &remote,
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

    drop(shutdown);
    running.await??;
    Ok(())
}

fn assert_read_denied(error: eidetica::Error, tree: &ID) {
    assert_permission_denied(error, tree, "Read");
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

/// Exercise the native service's actual Write gate, not historical signed-entry
/// submission (which is verification-gated). Abort the empty private cache build;
/// no records are published and no Provider effects are executed.
async fn service_write_probe(
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
        NewAccount::with_password("source", "test-password"),
    )
    .await?;
    let admin = user.get_default_key()?;
    let provider = user.create_database(Doc::new(), &admin).await?;
    let session = user.create_database(Doc::new(), &admin).await?;
    let direct = PrivateKey::generate();
    let delegated = PrivateKey::generate();
    let reader = PrivateKey::generate();
    let tx = provider.new_transaction().await?;
    tx.get_settings()?
        .set_auth_key(
            &direct.public_key(),
            AuthKey::active(None, Permission::Write(10)),
        )
        .await?;
    tx.get_settings()?
        .set_auth_key(
            &reader.public_key(),
            AuthKey::active(None, Permission::Read),
        )
        .await?;
    tx.commit().await?;
    let provider_tips = provider.snapshot().await?.into_tips();
    let tx = session.new_transaction().await?;
    tx.get_settings()?
        .set_auth_key(
            &direct.public_key(),
            AuthKey::active(None, Permission::Write(10)),
        )
        .await?;
    tx.get_settings()?
        .set_auth_key(
            &reader.public_key(),
            AuthKey::active(None, Permission::Read),
        )
        .await?;
    tx.get_settings()?
        .add_delegated_tree(DelegatedTreeRef {
            permission_bounds: PermissionBounds {
                max: Permission::Write(10),
                min: None,
            },
            tree: TreeReference {
                root: provider.root_id().clone(),
                tips: provider_tips,
            },
        })
        .await?;
    tx.commit().await?;
    // Explicit fixture Read grants are not operator-only membership policy.
    session.share().await?;
    provider.share().await?;

    let (replica, account) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("replica", "test-password"),
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
    tx.get_settings()?
        .set_auth_key(
            &delegated.public_key(),
            AuthKey::active(None, Permission::Write(10)),
        )
        .await?;
    let missing_proof = tx.commit().await?;
    let identity = SigKey::Delegation {
        path: vec![DelegationStep {
            tree: provider.root_id().clone(),
            tips: provider.snapshot().await?.into_tips(),
        }],
        hint: KeyHint::from_pubkey(&delegated.public_key()),
    };
    assert!(replica.backend().get(&missing_proof).await.is_err());

    let dir = tempfile::tempdir()?;
    let socket = dir.path().join("replica.sock");
    let service = ServiceServer::bind(replica.clone(), &socket).await?;
    let (shutdown, receiver) = watch::channel(());
    let running = tokio::spawn(service.run(receiver));
    account
        .admin()
        .await?
        .create_account(NewAccount::with_password("observer", "test-password"))
        .await?;
    let remote = Instance::connect(format!("unix://{}", socket.display())).await?;
    let login = remote.login_account("observer", "test-password").await?;
    assert!(
        login
            .admin()
            .await
            .unwrap_err()
            .to_string()
            .contains("[auth::PermissionDenied]")
    );
    // Register possession; the session actor remains the explicit Read key.
    for key in [&reader, &direct, &delegated] {
        Database::create(&remote, key.clone(), Doc::new()).await?;
    }
    let service_session = open_as(&remote, &local, &reader)
        .await?
        .with_key(reader.clone());
    let cursor = local.snapshot().await?;
    let (local_tx, mut local_rx) = mpsc::unbounded_channel();
    let _local_watch = local
        .on_write_at_tips(cursor.clone(), move |event, _| {
            let _ = local_tx.send((event.previous_tips().clone(), event.post_tips().clone()));
            async { Ok(()) }
        })
        .await?;
    let (remote_tx, mut remote_rx) = mpsc::unbounded_channel();
    let _remote_watch = service_session
        .on_write_at_tips(cursor, move |event, _| {
            let _ = remote_tx.send((event.previous_tips().clone(), event.post_tips().clone()));
            async { Ok(()) }
        })
        .await
        .context("subscribe service with explicit reader")?;

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
    assert!(
        local_rx.try_recv().is_err(),
        "raw insertion must not trigger effects"
    );
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
        let raw_ids = db.ids_added(&before_local.0, &before_local.1).await?;
        assert!(
            raw_ids.contains(&invalid_id),
            "watch brackets include Failed entries"
        );
        assert!(
            raw_ids.contains(&delegated_id),
            "watch brackets include Unverified entries"
        );
        assert_eq!(
            issued_records(db, &[&direct, &delegated]).await?,
            vec![direct_id.clone()]
        );
    }
    // This is a *real* peer transfer of the missing Provider proof, not copying
    // entries or trusting the sender's verification labels.
    receiving
        .sync_with_peer_as(&address, Some(provider.root_id()), Some(&reader))
        .await
        .context("sync delayed Provider proof")?;
    assert!(replica.backend().get(&missing_proof).await.is_ok());
    let report = local.verify().await?;
    assert_eq!(report.verified, 1, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    let after_local = local_rx.recv().await.unwrap();
    let after_remote = remote_rx.recv().await.unwrap();
    assert_eq!(after_local, after_remote);
    assert_eq!(after_local.0, before_local.1);
    assert_eq!(after_local.0, after_local.1, "promotion adds no DAG IDs");
    let mut expected = vec![direct_id.clone(), delegated_id.clone()];
    expected.sort();
    for db in [&local, &service_session] {
        assert!(
            db.ids_added(&after_local.0, &after_local.1)
                .await?
                .is_empty()
        );
        assert_eq!(issued_records(db, &[&direct, &delegated]).await?, expected);
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
        &remote,
        remote.remote_connection().unwrap().clone(),
        session.root_id(),
        identity.clone(),
    )
    .await?;
    assert_eq!(acting.current_permission().await?, Permission::Write(10));
    assert!(!service_actor.snapshot().await?.is_empty());
    service_write_probe(&remote, &local, &identity).await?;
    let direct_identity = SigKey::from_pubkey(&direct.public_key());
    service_write_probe(&remote, &local, &direct_identity).await?;

    // A server-native Write gate detects a reduction even though Read and
    // historical authentication still work. No client-side permission RPC or
    // production observer bypass is introduced by this disposable cache probe.
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
    service_write_probe(&remote, &local, &identity).await?;
    receiving
        .sync_with_peer_as(&address, Some(provider.root_id()), Some(&reader))
        .await?;
    assert_permission_denied(
        service_write_probe(&remote, &local, &identity)
            .await
            .unwrap_err(),
        session.root_id(),
        "Write(0)",
    );
    assert_eq!(acting.current_permission().await?, Permission::Read);
    assert!(
        !service_actor.snapshot().await?.is_empty(),
        "Read survives downgrade"
    );
    assert!(local.verify_entry_signature(&delegated_id).await?);
    assert!(
        service_session
            .verify_entry_signature(&delegated_id)
            .await?
    );

    // The reader may still verify old issuance after an operator loses all
    // current Provider/session authority.
    let local_provider = Database::open(&replica, provider.root_id()).await?;
    let remote_provider = open_as(&remote, &local_provider, &reader).await?;
    let tx = provider.new_transaction().await?;
    for key in [&direct, &delegated] {
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
        for key in [&direct, &delegated] {
            assert!(!auth.get_key_by_pubkey(&key.public_key())?.is_active());
            assert_key_not_found(db.clone().with_key(key.clone()).current_permission().await);
        }
    }
    assert!(matches!(acting.current_permission().await,
        Err(eidetica::Error::Auth(ref error))
        if matches!(error.as_ref(), eidetica::auth::AuthError::SigningKeyMismatch { .. })));
    assert_read_denied(
        service_actor.snapshot().await.unwrap_err(),
        session.root_id(),
    );
    assert_permission_denied(
        service_write_probe(&remote, &local, &identity)
            .await
            .unwrap_err(),
        session.root_id(),
        "Write(0)",
    );
    // A separate direct session grant is not the same as Provider membership.
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
        assert!(db.verify_entry_signature(&direct_id).await?);
        assert!(db.verify_entry_signature(&delegated_id).await?);
        assert_eq!(
            issued_records(db, &[&direct, &delegated]).await?,
            expected,
            "historical issuance is not current eligibility"
        );
    }
    assert_permission_denied(
        service_write_probe(&remote, &local, &direct_identity)
            .await
            .unwrap_err(),
        session.root_id(),
        "Write(0)",
    );
    // Client-side delegated current_permission remains unsupported (covered
    // above), so the production observer guard stays. The service gate instead
    // checks current accepted authority natively on each actual request.

    drop(login);
    drop(shutdown);
    running.await??;
    serving.stop_server().await?;
    Ok(())
}

/// An inert effect-selection probe, not a Provider implementation: these are
/// known fixture operating keys. Native historical validation plus concrete-key
/// matching is applied only to entries reconciled from a fresh verified view.
/// Current Provider/session permission must be checked separately before effects.
async fn issued_records(db: &Database, operators: &[&PrivateKey]) -> eidetica::Result<Vec<ID>> {
    let mut accepted = Vec::new();
    for entry in records(db).await? {
        if db.verify_entry_signature(entry.id()).await?
            && operators
                .iter()
                .any(|key| verify_entry_signature(&entry, &key.public_key()).is_ok())
        {
            accepted.push(entry.id());
        }
    }
    accepted.sort();
    Ok(accepted)
}

/// Historical issuance stays valid, but retaining pre-revocation tips cannot
/// recover live eligibility after the verifier accepts the removal.
#[tokio::test]
async fn native_provider_current_revocation() -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), current_revocation()).await??;
    Ok(())
}

async fn current_revocation() -> anyhow::Result<()> {
    let (instance, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("current", "test-password"),
    )
    .await?;
    let admin = user.get_default_key()?;
    let provider = user.create_database(Doc::new(), &admin).await?;
    let session = user.create_database(Doc::new(), &admin).await?;
    let operator = PrivateKey::generate();
    let tx = provider.new_transaction().await?;
    tx.get_settings()?
        .set_auth_key(
            &operator.public_key(),
            AuthKey::active(None, Permission::Write(10)),
        )
        .await?;
    tx.commit().await?;
    let tips = provider.snapshot().await?.into_tips();
    let tx = session.new_transaction().await?;
    tx.get_settings()?
        .add_delegated_tree(DelegatedTreeRef {
            permission_bounds: PermissionBounds {
                max: Permission::Write(10),
                min: None,
            },
            tree: TreeReference {
                root: provider.root_id().clone(),
                tips: tips.clone(),
            },
        })
        .await?;
    tx.commit().await?;
    let identity = SigKey::Delegation {
        path: vec![DelegationStep {
            tree: provider.root_id().clone(),
            tips,
        }],
        hint: KeyHint::from_pubkey(&operator.public_key()),
    };
    let key = DatabaseKey::with_identity(operator.clone(), identity);
    let acting = session.clone().with_key(key.clone());
    assert_eq!(acting.current_permission().await?, Permission::Write(10));
    assert!(
        Database::can_access(
            &instance,
            session.root_id(),
            &operator.public_key(),
            &Permission::Write(10)
        )
        .await?
    );
    let issued = publish(&session, key, provider.root_id()).await?;
    let tx = provider.new_transaction().await?;
    tx.get_settings()?
        .revoke_auth_key(&operator.public_key())
        .await?;
    tx.commit().await?;
    assert!(
        !provider
            .get_settings()
            .await?
            .auth_snapshot()
            .await?
            .get_key_by_pubkey(&operator.public_key())?
            .is_active()
    );
    assert_key_not_found(
        provider
            .with_key(operator.clone())
            .current_permission()
            .await,
    );
    // Native one-hop pubkey discovery is an available local current-state
    // alternative, checked alongside the explicit retained identity.
    assert!(
        !Database::can_access(
            &instance,
            session.root_id(),
            &operator.public_key(),
            &Permission::Write(10)
        )
        .await?
    );
    assert!(
        session.verify_entry_signature(&issued).await?,
        "historical issuance remains valid"
    );
    // No partition: the same local instance has accepted the revocation above.
    let current = acting.current_permission().await;
    assert!(
        matches!(current, Err(eidetica::Error::Auth(_))),
        "retained tips must not recover live authority after accepted revocation: {current:?}"
    );
    drop(instance);
    Ok(())
}

fn assert_key_not_found(result: eidetica::Result<Permission>) {
    assert!(
        matches!(result, Err(eidetica::Error::Auth(ref error))
            if matches!(error.as_ref(), eidetica::auth::AuthError::KeyNotFound { .. })),
        "expected native current-key denial, not a transport failure: {result:?}"
    );
}
