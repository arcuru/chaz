//! Native historical signature regression, not a Provider protocol implementation.
//! All databases/keys are disposable; the only listener is a temporary Unix socket.
//! Historical verification is distinct from current authority and concrete signer attribution.

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
    backend::database::InMemory,
    crdt::Doc,
    database::DatabaseKey,
    entry::ID,
    service::{ServiceServer, protocol::ReadScope},
    store::DocStore,
};
use tokio::sync::watch;

async fn publish(db: &Database, key: DatabaseKey, provider: &ID) -> eidetica::Result<ID> {
    let tx = db.clone().with_key(key).new_transaction().await?;
    let store = tx.get_store::<DocStore>("records").await?;
    // This label is data, not proof of who wrote it.
    store.set("provider", provider.to_string()).await?;
    tx.commit().await
}

async fn records(db: &Database) -> eidetica::Result<Vec<Entry>> {
    let frontier = db.snapshot().await?;
    db.get_store_entries("records", frontier.tips(), ReadScope::Verified)
        .await
}

#[tokio::test]
async fn native_provider_service_parity() -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), proof()).await??;
    Ok(())
}

async fn proof() -> anyhow::Result<()> {
    let (owner, mut user) =
        Instance::create_backend(Box::new(InMemory::new()), NewAccount::passwordless("proof"))
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
        .create_account(NewAccount::passwordless("reader"))
        .await?;
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join("proof.sock");
    let service = ServiceServer::bind(owner.clone(), &socket).await?;
    let (shutdown, receiver) = watch::channel(());
    let running = tokio::spawn(service.run(receiver));
    let remote = Instance::connect(format!("unix://{}", socket.display())).await?;
    let login = remote.login_account("reader", None).await?;
    assert!(login.admin().await.is_err(), "reader is not a host admin");
    assert!(
        Database::open(&remote, session.root_id()).await.is_err(),
        "login alone grants no session access"
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
    }
    println!(
        "PROVED: verified-frontier direct/service entry parity, direct-key Provider association, valid-writer forged-label rejection"
    );

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
    println!(
        "PROVED: direct native delegated identity resolves and authenticates the Provider operating key"
    );

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
    assert!(
        proof_error.to_string().contains("not permitted for Read"),
        "session Read does not grant delegated-proof Read: {proof_error}"
    );
    assert!(open_as(&remote, &provider, &target_only).await.is_err());
    assert!(
        remote_session
            .clone()
            .with_key(PrivateKey::generate())
            .verify_entry_signature(&delegated_id)
            .await?,
        "attached key must not replace the service handle's bound reader"
    );
    assert!(
        remote
            .remote_connection()
            .unwrap()
            .verify_entry_signature(
                session.root_id().clone(),
                SigKey::from_pubkey(&PrivateKey::generate().public_key()),
                delegated_id.clone()
            )
            .await
            .is_err(),
        "unregistered actor is denied"
    );
    println!(
        "PROVED: service historical delegated verification with non-admin Read actor; per-database proof Read and Write denials"
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
    println!(
        "CONFIRMED: delegated service current_permission unsupported; observer guard retained"
    );

    drop(shutdown);
    running.await??;
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
