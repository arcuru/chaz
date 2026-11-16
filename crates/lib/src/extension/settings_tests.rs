use super::*;
use crate::extension::test_doubles::make_session_db;
use serde_json::json;

async fn legacy(db: &Database, ext: &str, text: &str) {
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
        .await
        .unwrap()
        .set_string(ext, text)
        .await
        .unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn native_settings_retained_conversion_is_explicit_atomic_and_lossless() {
    let (_inst, db) = make_session_db().await;
    let value =
        json!({"enabled":false,"a.b":{"nested":1.25},"":null,"雪":[18446744073709551615u64]});
    legacy(&db, "memory", &value.to_string()).await;
    assert_eq!(read_settings_checked(&db, "memory").await.unwrap(), value);
    let before = db.snapshot().await.unwrap();
    assert!(
        update_settings(&db, "memory", true, |v| {
            v["new"] = json!(1);
            Ok(())
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("offline conversion")
    );
    assert_eq!(db.snapshot().await.unwrap(), before);
    legacy(&db, "broken", "{bad").await;
    let before = db.snapshot().await.unwrap();
    assert!(migrate_settings(&db).await.is_err());
    assert_eq!(
        db.snapshot().await.unwrap(),
        before,
        "no partially converted objects"
    );
    legacy(&db, "broken", "null").await;
    assert_eq!(migrate_settings(&db).await.unwrap(), 1);
    assert_eq!(migrate_settings(&db).await.unwrap(), 0);
    assert_eq!(read_settings_checked(&db, "memory").await.unwrap(), value);
    assert_eq!(
        read_settings_checked(&db, "broken").await.unwrap(),
        Value::Null
    );
    // Old get_string readers fail on this new object, never claimed supported.
    let txn = db.new_transaction().await.unwrap();
    assert!(
        txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
            .await
            .unwrap()
            .get_string("memory")
            .await
            .is_err()
    );
    update_settings(&db, "memory", true, |v| {
        v["a.b"] = json!({"only":2});
        v.as_object_mut().unwrap().remove("");
        Ok(())
    })
    .await
    .unwrap();
    let got = read_settings_checked(&db, "memory").await.unwrap();
    assert_eq!(got["a.b"], json!({"only":2}));
    assert!(got.get("").is_none());
    assert_eq!(got["雪"], value["雪"]);
    // A removal without any accompanying set is still a real native delta.
    update_settings(&db, "memory", true, |v| {
        v.as_object_mut().unwrap().remove("enabled");
        Ok(())
    })
    .await
    .unwrap();
    let removed = read_settings_checked(&db, "memory").await.unwrap();
    assert!(removed.get("enabled").is_none());
    assert_eq!(removed["雪"], value["雪"]);
    write_settings(&db, "memory", json!({})).await.unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({})
    );
    for value in [
        json!(null),
        json!([1, 2]),
        json!(1.25),
        json!(18446744073709551615u64),
    ] {
        write_settings(&db, "memory", value.clone()).await.unwrap();
        assert_eq!(read_settings_checked(&db, "memory").await.unwrap(), value);
    }
}

#[tokio::test]
async fn native_settings_independent_deltas_survive_both_commit_orders() {
    for reverse in [false, true] {
        let (_inst, db) = make_session_db().await;
        write_settings(&db, "memory", json!({"left":0,"right":0,"enabled":false}))
            .await
            .unwrap();
        let snapshot = db.snapshot().await.unwrap();
        let a = db.new_transaction_at(&snapshot).await.unwrap();
        let b = db.clone().new_transaction_at(&snapshot).await.unwrap();
        update_settings_in(&a, "memory", true, |v| {
            v["left"] = json!(1);
            Ok(())
        })
        .await
        .unwrap();
        update_settings_in(&b, "memory", true, |v| {
            v["right"] = json!(2);
            Ok(())
        })
        .await
        .unwrap();
        if reverse {
            b.commit().await.unwrap();
            a.commit().await.unwrap();
        } else {
            a.commit().await.unwrap();
            b.commit().await.unwrap();
        }
        assert_eq!(
            read_settings_checked(&db, "memory").await.unwrap(),
            json!({"left":1,"right":2,"enabled":false})
        );
    }
}

#[tokio::test]
async fn native_settings_checked_corruption_and_read_only_writes() {
    let (_instance, user, db) = crate::session::test_helpers::test_session_db().await;
    legacy(&db, "memory", "{bad").await;
    assert!(read_settings_checked(&db, "memory").await.is_err());
    assert_eq!(read_settings(&db, "memory").await, json!({}));
    let before = db.snapshot().await.unwrap();
    update_settings(&db, "memory", false, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(
        db.snapshot().await.unwrap(),
        before,
        "a lenient no-op must not repair corruption"
    );
    assert!(
        update_settings(&db, "memory", true, |_| Ok(()))
            .await
            .is_err()
    );
    update_settings(&db, "memory", false, |v| {
        v["enabled"] = json!(false);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"enabled":false})
    );
    let txn = db.new_transaction().await.unwrap();
    let mut bad = Doc::new();
    bad.set(field_key("oops"), 123);
    txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
        .await
        .unwrap()
        .set_node("memory", bad)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(read_settings_checked(&db, "memory").await.is_err());
    update_settings(&db, "memory", false, |v| {
        v["fixed"] = json!(true);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"fixed":true})
    );
    legacy(&db, "retained", r#"{"enabled":false}"#).await;
    let key = user.get_default_key().unwrap();
    let txn = db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_auth_key(
            &key,
            eidetica::auth::types::AuthKey::active(None, eidetica::auth::Permission::Read),
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let reopened = user.open_database(db.root_id()).await.unwrap();
    let before = reopened.snapshot().await.unwrap();
    assert!(migrate_settings(&reopened).await.is_err());
    assert_eq!(reopened.snapshot().await.unwrap(), before);
    assert_eq!(
        read_settings_checked(&reopened, "retained").await.unwrap(),
        json!({"enabled":false})
    );
    assert!(
        write_settings(&reopened, "memory", json!({"bad":1}))
            .await
            .is_err()
    );
    assert!(
        update_settings(&reopened, "memory", true, |v| {
            v["bad"] = json!(1);
            Ok(())
        })
        .await
        .is_err()
    );
    assert_eq!(
        read_settings_checked(&reopened, "memory").await.unwrap(),
        json!({"fixed":true})
    );
}

#[tokio::test]
async fn native_settings_peer_sync_converges_independent_and_same_field_writes() {
    use eidetica::{
        Instance, NewAccount,
        auth::{Permission, types::AuthKey},
        backend::database::InMemory,
        sync::{Address, transports::http::HttpTransport},
    };
    let (owner, owner_user, db) = crate::session::test_helpers::test_session_db().await;
    owner.enable_sync().await.unwrap();
    let txn = db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_global_auth_key(AuthKey::active(None, Permission::Write(0)))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    legacy(
        &db,
        "memory",
        r#"{"left":0,"right":0,"same":0,"enabled":false}"#,
    )
    .await;
    migrate_settings(&db).await.unwrap();
    db.share().await.unwrap();
    let sync = owner.sync().unwrap();
    sync.register_transport("http", HttpTransport::builder().bind("127.0.0.1:0"))
        .await
        .unwrap();
    sync.accept_connections().await.unwrap();
    let address = Address::http(sync.get_server_address_for("http").await.unwrap());
    let (peer, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::with_password("peer", "test-password"),
    )
    .await
    .unwrap();
    peer.enable_sync().await.unwrap();
    let key = user.get_default_key().unwrap();
    let signing = user.get_signing_key(&key).unwrap();
    let ps = peer.sync().unwrap();
    ps.register_transport("http", HttpTransport::builder())
        .await
        .unwrap();
    ps.sync_with_peer_for_bootstrap_with_key(
        &address,
        db.root_id(),
        &signing,
        &key.to_string(),
        Permission::Write(10),
    )
    .await
    .unwrap();
    ps.flush().await.unwrap();
    let (sigkey, _) = Database::find_sigkeys(&peer, db.root_id(), &key)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    user.map_key(&key, db.root_id(), sigkey).await.unwrap();
    let pdb = user.open_database(db.root_id()).await.unwrap();
    assert_eq!(
        read_settings_checked(&pdb, "memory").await.unwrap()["enabled"],
        false
    );
    update_settings(&db, "memory", true, |v| {
        v["left"] = json!(1);
        v["same"] = json!("owner");
        Ok(())
    })
    .await
    .unwrap();
    update_settings(&pdb, "memory", true, |v| {
        v["right"] = json!(2);
        v["same"] = json!("peer");
        Ok(())
    })
    .await
    .unwrap();
    for _ in 0..2 {
        ps.sync_with_peer(&address, Some(db.root_id()))
            .await
            .unwrap();
        ps.flush().await.unwrap();
    }
    let local = read_settings_checked(&db, "memory").await.unwrap();
    let remote = read_settings_checked(&pdb, "memory").await.unwrap();
    assert_eq!(local, remote);
    assert_eq!(local["left"], 1);
    assert_eq!(local["right"], 2);
    assert_eq!(local["enabled"], false);
    assert!([json!("owner"), json!("peer")].contains(&local["same"]));
    // Reopening separate authorized handles reconstructs from synchronized history.
    let reopened = owner_user.open_database(db.root_id()).await.unwrap();
    assert_eq!(
        read_settings_checked(&reopened, "memory").await.unwrap(),
        local
    );
    drop(pdb);
    let reopened = user.open_database(db.root_id()).await.unwrap();
    assert_eq!(
        read_settings_checked(&reopened, "memory").await.unwrap(),
        local
    );
}

#[tokio::test]
async fn native_settings_sqlite_reopen_preserves_converted_values_and_deltas() {
    use eidetica::{Instance, NewAccount, backend::database::Sqlite};
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("settings.db").display()
    );
    let (instance, mut user) = Instance::create_backend(
        Box::new(Sqlite::connect(&url).await.unwrap()),
        NewAccount::with_password("settings", "test-password"),
    )
    .await
    .unwrap();
    let key = user.get_default_key().unwrap();
    let db = user.create_database(Doc::new(), &key).await.unwrap();
    let id = db.root_id().clone();
    legacy(&db, "memory", r#"{"enabled":false,"left":0,"right":0}"#).await;
    migrate_settings(&db).await.unwrap();
    update_settings(&db, "memory", true, |v| {
        v["left"] = json!(1.5);
        Ok(())
    })
    .await
    .unwrap();
    drop(db);
    drop(user);
    drop(instance);
    let instance = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            match Instance::connect(&url).await {
                Ok(instance) => break instance,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("SQLite instance was not released");
    let user = instance
        .login_account("settings", "test-password")
        .await
        .unwrap();
    let db = user.open_database(&id).await.unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"enabled":false,"left":1.5,"right":0})
    );
    update_settings(&db, "memory", true, |v| {
        v["right"] = json!(2);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"enabled":false,"left":1.5,"right":2})
    );
}
