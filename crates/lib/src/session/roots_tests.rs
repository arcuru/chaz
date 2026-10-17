//! Root-reference lookup, initialization and failure behaviour.

use super::roots::{self, REFERENCE_NAMESPACE, RootKind, RootSetup, RootsError};
use crate::test_support::counting_backend::{BackendProbe, CountingBackend};
use eidetica::backend::database::InMemory;
use eidetica::entry::ID;
use eidetica::store::DocStore;
use eidetica::{AccountSession, Database, Instance, NewAccount};

async fn instance() -> (Instance, AccountSession) {
    Instance::create_backend(Box::new(InMemory::new()), NewAccount::passwordless("chaz"))
        .await
        .unwrap()
}

async fn relogin(instance: &Instance) -> AccountSession {
    instance.login_account("chaz", None).await.unwrap()
}

async fn named(user: &mut AccountSession, name: &str) -> Database {
    let key = user.get_default_key().unwrap();
    let mut settings = eidetica::crdt::Doc::new();
    settings.set("name", name);
    user.create_database(settings, &key).await.unwrap()
}

/// Every tracked database carrying `name` (the old discovery rule).
async fn tracked_named(user: &AccountSession, name: &str) -> Vec<ID> {
    match user.find_database(name).await {
        Ok(found) => found.iter().map(|db| db.root_id().clone()).collect(),
        Err(_) => Vec::new(),
    }
}

async fn assert_no_roots_created(user: &AccountSession) {
    assert!(tracked_named(user, "chaz_group").await.is_empty());
    assert!(tracked_named(user, "chaz_peer").await.is_empty());
}

#[tokio::test]
async fn designated_writer_initializes_once_then_reopens_by_reference() {
    let (instance, mut user) = instance().await;
    let first = roots::open(&mut user, RootSetup::Initialize).await.unwrap();
    assert_eq!(
        roots::referenced(&user, RootKind::Group).await.unwrap(),
        Some(first.group.root_id().clone())
    );
    assert_eq!(
        roots::referenced(&user, RootKind::Peer).await.unwrap(),
        Some(first.peer.root_id().clone())
    );

    // Restart: a fresh login opens the same pair, even as a non-writer.
    let mut restarted = relogin(&instance).await;
    let again = roots::open(&mut restarted, RootSetup::RequireExisting)
        .await
        .unwrap();
    assert_eq!(again.group.root_id(), first.group.root_id());
    assert_eq!(again.peer.root_id(), first.peer.root_id());
    assert_eq!(tracked_named(&restarted, "chaz_group").await.len(), 1);
    assert_eq!(tracked_named(&restarted, "chaz_peer").await.len(), 1);
}

#[tokio::test]
async fn non_writer_reports_absent_roots_without_creating_them() {
    let (_instance, mut user) = instance().await;
    let error = roots::open(&mut user, RootSetup::RequireExisting)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&error, RootsError::NotInitialized { missing } if missing == &[RootKind::Group, RootKind::Peer]),
        "{error}"
    );
    assert_no_roots_created(&user).await;
}

#[tokio::test]
async fn references_survive_display_name_changes_and_restart() {
    let (instance, mut user) = instance().await;
    let first = roots::open(&mut user, RootSetup::Initialize).await.unwrap();
    for db in [&first.group, &first.peer] {
        let txn = db.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_name("renamed")
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
    // A decoy carrying the old name must not be selected either.
    let mut restarted = relogin(&instance).await;
    named(&mut restarted, "chaz_group").await;
    let again = roots::open(&mut restarted, RootSetup::Initialize)
        .await
        .unwrap();
    assert_eq!(again.group.root_id(), first.group.root_id());
    assert_eq!(again.peer.root_id(), first.peer.root_id());
    assert_eq!(again.group.get_name().await.unwrap(), "renamed");
}

#[tokio::test]
async fn malformed_reference_is_an_error_and_never_replaced() {
    let (_instance, mut user) = instance().await;
    let txn = user.account().database().new_transaction().await.unwrap();
    txn.get_store::<DocStore>(format!("refs:{REFERENCE_NAMESPACE}"))
        .await
        .unwrap()
        .set("group", "not-a-database-id")
        .await
        .unwrap();
    txn.commit().await.unwrap();

    let error = roots::open(&mut user, RootSetup::Initialize)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            RootsError::Unavailable {
                kind: RootKind::Group,
                ..
            }
        ),
        "{error}"
    );
    assert_no_roots_created(&user).await;
}

#[tokio::test]
async fn unavailable_or_unauthorized_target_is_an_error_and_never_replaced() {
    // Unavailable: the referenced database is not on this instance.
    let (_elsewhere, mut other) = instance().await;
    let missing = named(&mut other, "chaz_group").await.root_id().clone();
    let (instance, mut user) = instance().await;
    user.set_reference(REFERENCE_NAMESPACE, "group", &missing)
        .await
        .unwrap();
    let error = roots::open(&mut user, RootSetup::Initialize)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            RootsError::Unavailable {
                kind: RootKind::Group,
                ..
            }
        ),
        "{error}"
    );
    assert_no_roots_created(&user).await;

    // Unauthorized: present locally, but this login holds no key for it.
    user.admin()
        .await
        .unwrap()
        .create_account(NewAccount::passwordless("stranger"))
        .await
        .unwrap();
    let mut stranger = instance.login_account("stranger", None).await.unwrap();
    let foreign = named(&mut stranger, "chaz_peer").await.root_id().clone();
    let peer_name = RootKind::Peer.reference_name(&user);
    user.set_reference(REFERENCE_NAMESPACE, "group", &foreign)
        .await
        .unwrap();
    let error = roots::open(&mut user, RootSetup::Initialize)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            RootsError::Unavailable {
                kind: RootKind::Group,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(
        user.reference(REFERENCE_NAMESPACE, &peer_name)
            .await
            .unwrap(),
        None
    );
    assert_no_roots_created(&user).await;
}

#[tokio::test]
async fn legacy_named_roots_refuse_initialization_until_recorded() {
    let (_instance, mut user) = instance().await;
    let group = named(&mut user, "chaz_group").await.root_id().clone();
    let peer = named(&mut user, "chaz_peer").await.root_id().clone();

    let error = roots::open(&mut user, RootSetup::Initialize)
        .await
        .err()
        .unwrap();
    match &error {
        RootsError::Unreferenced { found } => {
            assert_eq!(
                found,
                &vec![
                    (RootKind::Group, vec![group.clone()]),
                    (RootKind::Peer, vec![peer.clone()])
                ]
            );
        }
        other => panic!("unexpected: {other}"),
    }
    assert!(
        error
            .to_string()
            .contains(&format!("`chaz roots --group {group} --peer {peer}`")),
        "{error}"
    );
    assert_eq!(
        tracked_named(&user, "chaz_group").await,
        vec![group.clone()]
    );

    roots::record(&user, RootKind::Group, &group).await.unwrap();
    roots::record(&user, RootKind::Peer, &peer).await.unwrap();
    let opened = roots::open(&mut user, RootSetup::Initialize).await.unwrap();
    assert_eq!(opened.group.root_id(), &group);
    assert_eq!(opened.peer.root_id(), &peer);
    assert_eq!(tracked_named(&user, "chaz_peer").await, vec![peer]);
}

#[tokio::test]
async fn partial_initialization_requires_explicit_repair_with_the_existing_id() {
    let (_instance, mut user) = instance().await;
    // A previous initializer created both but only recorded the group.
    let group = named(&mut user, "chaz_group").await.root_id().clone();
    let orphan = named(&mut user, "chaz_peer").await.root_id().clone();
    roots::record(&user, RootKind::Group, &group).await.unwrap();

    for _ in 0..2 {
        let error = roots::open(&mut user, RootSetup::Initialize)
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&error, RootsError::Unreferenced { found } if found == &vec![(RootKind::Peer, vec![orphan.clone()])]),
            "{error}"
        );
        assert!(error.to_string().contains(&orphan.to_string()));
    }
    assert_eq!(
        tracked_named(&user, "chaz_peer").await,
        vec![orphan.clone()]
    );

    roots::record(&user, RootKind::Peer, &orphan).await.unwrap();
    // Recording the same ID again is a no-op; a different one is refused.
    roots::record(&user, RootKind::Peer, &orphan).await.unwrap();
    let error = roots::record(&user, RootKind::Peer, &group)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            RootsError::AlreadyReferenced {
                kind: RootKind::Peer,
                ..
            }
        ),
        "{error}"
    );
    let opened = roots::open(&mut user, RootSetup::Initialize).await.unwrap();
    assert_eq!(opened.peer.root_id(), &orphan);
}

#[tokio::test]
async fn record_refuses_targets_this_login_cannot_open() {
    let (_elsewhere, mut other) = instance().await;
    let missing = named(&mut other, "chaz_group").await.root_id().clone();
    let (_instance, user) = instance().await;
    assert!(
        roots::record(&user, RootKind::Group, &missing)
            .await
            .is_err()
    );
    assert_eq!(
        roots::referenced(&user, RootKind::Group).await.unwrap(),
        None
    );
}

/// In-process initializers on one login serialize: exactly one pair is
/// created. A waiter whose session predates that creation may not yet hold
/// the new key mapping; it then reports the present reference as unusable
/// rather than creating another root, and a restart opens the pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_in_process_initializers_create_one_pair() {
    let (instance, _owner) = instance().await;
    let mut sessions = Vec::new();
    for _ in 0..4 {
        sessions.push(relogin(&instance).await);
    }
    let handles: Vec<_> = sessions
        .into_iter()
        .map(|mut user| {
            tokio::spawn(async move {
                let roots = roots::open(&mut user, RootSetup::Initialize).await;
                roots.map(|r| (r.group.root_id().clone(), r.peer.root_id().clone()))
            })
        })
        .collect();
    let mut pairs = Vec::new();
    for handle in handles {
        match handle.await.unwrap() {
            Ok(pair) => pairs.push(pair),
            Err(RootsError::Unavailable { .. }) => {}
            Err(other) => panic!("initializer failed unexpectedly: {other}"),
        }
    }
    pairs.dedup();
    assert_eq!(
        pairs.len(),
        1,
        "initializers chose different roots: {pairs:?}"
    );
    let mut user = relogin(&instance).await;
    assert_eq!(
        tracked_named(&user, "chaz_group").await,
        vec![pairs[0].0.clone()]
    );
    assert_eq!(
        tracked_named(&user, "chaz_peer").await,
        vec![pairs[0].1.clone()]
    );
    let restarted = roots::open(&mut user, RootSetup::RequireExisting)
        .await
        .unwrap();
    assert_eq!(restarted.group.root_id(), &pairs[0].0);
}

#[tokio::test]
async fn another_installations_peer_reference_never_selects_this_peer() {
    let (instance, mut user) = instance().await;
    let name = RootKind::Peer.reference_name(&user);
    assert_eq!(name, format!("peer-{}", user.login_id()));
    assert_ne!(user.login_id(), user.account().id().to_string());

    // The Account carries a peer pointer written for another installation
    // (another login record) of the same Account.
    let theirs = named(&mut user, "their-peer").await.root_id().clone();
    user.set_reference(REFERENCE_NAMESPACE, "peer-another-installation", &theirs)
        .await
        .unwrap();
    let ours = roots::open(&mut user, RootSetup::Initialize).await.unwrap();
    assert_ne!(ours.peer.root_id(), &theirs);

    // A second login on this instance is a separate installation identity.
    user.admin()
        .await
        .unwrap()
        .create_account(NewAccount::passwordless("second"))
        .await
        .unwrap();
    let second = instance.login_account("second", None).await.unwrap();
    assert_ne!(RootKind::Peer.reference_name(&second), name);
    assert_eq!(
        user.reference(REFERENCE_NAMESPACE, "peer-another-installation")
            .await
            .unwrap(),
        Some(theirs)
    );
}

/// The lookup touches only the Account and the two selected root trees,
/// however many unrelated databases the login tracks, and its reads of those
/// trees stay constant. The only growing operation is Eidetica re-checking
/// the verification status of the Account database's own history, which
/// gains entries as databases are tracked. The retired name search, by
/// contrast, opens every tracked database.
#[tokio::test]
async fn root_lookup_is_independent_of_unrelated_tracked_databases() {
    const ACCOUNT_HISTORY: &str = "get_verification_status";
    let probe = BackendProbe::default();
    let (instance, mut user) = Instance::create_backend(
        Box::new(CountingBackend::new(InMemory::new(), probe.clone())),
        NewAccount::passwordless("chaz"),
    )
    .await
    .unwrap();
    let initialized = roots::open(&mut user, RootSetup::Initialize).await.unwrap();
    let expected_trees: std::collections::BTreeSet<ID> = [
        user.account().id().clone(),
        initialized.group.root_id().clone(),
        initialized.peer.root_id().clone(),
    ]
    .into();

    let mut unrelated = Vec::new();
    let mut baseline = None;
    let mut measurements = Vec::new();
    for target in [0usize, 10, 60] {
        while unrelated.len() < target {
            let n = unrelated.len();
            unrelated.push(
                named(&mut user, &format!("unrelated-{n}"))
                    .await
                    .root_id()
                    .clone(),
            );
        }
        let mut restarted = relogin(&instance).await;
        // Warm derived store state so each window measures a steady read.
        roots::open(&mut restarted, RootSetup::RequireExisting)
            .await
            .unwrap();

        probe.start();
        let opened = roots::open(&mut restarted, RootSetup::RequireExisting)
            .await
            .unwrap();
        let (lookup_ops, trees) = probe.stop();
        let mut methods = probe.methods();
        assert_eq!(opened.group.root_id(), initialized.group.root_id());
        assert_eq!(opened.peer.root_id(), initialized.peer.root_id());
        assert_eq!(trees, expected_trees, "root lookup touched another tree");
        let history = methods.remove(ACCOUNT_HISTORY).unwrap_or_default();
        match &baseline {
            None => baseline = Some(methods),
            Some(baseline) => assert_eq!(&methods, baseline, "lookup reads changed"),
        }

        probe.start();
        let _ = restarted.find_database("chaz_group").await;
        let (scan_ops, scan_trees) = probe.stop();
        assert!(unrelated.iter().all(|id| scan_trees.contains(id)));
        measurements.push((target, lookup_ops, history, scan_ops));
    }
    eprintln!(
        "root lookup (unrelated DBs, lookup ops, of which account-history checks, retired name-scan ops): {measurements:?}; constant lookup reads: {baseline:?}"
    );
    assert!(measurements[2].3 > measurements[0].3, "{measurements:?}");
}

/// Embedded and service connections resolve the same references: a service
/// client opens the roots its owner initialized and never creates them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_client_opens_owner_initialized_roots() {
    use eidetica::service::ServiceServer;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("service.sock");
    let (owner, _) = instance().await;
    let server = ServiceServer::bind(owner.clone(), &socket).await.unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(());
    let task = tokio::spawn(server.run(receiver));
    let url = format!("unix://{}", socket.display());

    let client = Instance::connect(&url).await.unwrap();
    let mut early = client.login_account("chaz", None).await.unwrap();
    assert_eq!(
        RootSetup::for_connection(&client, false),
        RootSetup::RequireExisting
    );
    assert!(matches!(
        roots::open(&mut early, RootSetup::RequireExisting).await,
        Err(RootsError::NotInitialized { .. })
    ));

    let mut local = relogin(&owner).await;
    assert_eq!(
        RootSetup::for_connection(&owner, false),
        RootSetup::Initialize
    );
    let initialized = roots::open(&mut local, RootSetup::Initialize)
        .await
        .unwrap();

    let mut late = client.login_account("chaz", None).await.unwrap();
    let opened = roots::open(&mut late, RootSetup::RequireExisting)
        .await
        .unwrap();
    assert_eq!(opened.group.root_id(), initialized.group.root_id());
    assert_eq!(opened.peer.root_id(), initialized.peer.root_id());
    assert_eq!(late.login_id(), local.login_id());

    drop((early, late, client));
    shutdown.send(()).unwrap();
    task.await.unwrap().unwrap();
}
