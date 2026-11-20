use super::*;
use crate::extension::test_doubles::make_session_db;
use serde_json::json;
use std::sync::Arc;

const TEST_FIELD: SettingField = SettingField {
    key: "flag",
    label: "Flag",
    help: "An agent-only preference.",
    kind: SettingKind::Boolean,
    targets: SettingTargets::Agent,
};

struct DeclaredSettings(&'static [SettingField], Value);

impl Extension for DeclaredSettings {
    fn name(&self) -> &'static str {
        "declared"
    }

    fn supported_hooks(&self) -> &[crate::extension::HookKind] {
        &[]
    }

    fn setting_fields(&self) -> &'static [SettingField] {
        self.0
    }

    fn default_settings(&self) -> Value {
        self.1.clone()
    }
}

#[tokio::test]
async fn declared_settings_validate_before_both_install_paths_register_anything() {
    const EMPTY_KEY: &[SettingField] = &[SettingField {
        key: "",
        ..TEST_FIELD
    }];
    const INVALID_BOUNDS: &[SettingField] = &[SettingField {
        kind: SettingKind::Unsigned { min: 2, max: 1 },
        ..TEST_FIELD
    }];

    for commands_only in [true, false] {
        let mut hub = ExtensionHub::new();
        for (fields, defaults, expected) in [
            (EMPTY_KEY, json!({"": false}), "invalid setting key"),
            (
                &[TEST_FIELD, TEST_FIELD],
                json!({"flag": false}),
                "duplicate setting key",
            ),
            (&[TEST_FIELD], json!({}), "missing default"),
            (&[TEST_FIELD], json!(false), "must be an object"),
            (&[TEST_FIELD], json!({"flag": 0}), "invalid default"),
            (
                INVALID_BOUNDS,
                json!({"flag": 1}),
                "invalid unsigned bounds",
            ),
        ] {
            let extensions: Vec<Arc<dyn Extension>> = vec![
                Arc::new(crate::extensions::session_naming::SessionNaming),
                Arc::new(DeclaredSettings(fields, defaults)),
            ];
            let result = if commands_only {
                hub.install_commands(extensions).await
            } else {
                hub.install_all(extensions).await
            };
            let error = result.unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert!(hub.extension_names().is_empty());
            assert!(hub.registered_setting_fields("session_naming").is_none());
        }
    }
}

#[tokio::test]
async fn declared_settings_command_catalog_is_readable_and_edits_check_target() {
    let mut hub = ExtensionHub::new();
    hub.install_commands(vec![
        Arc::new(crate::extensions::session_naming::SessionNaming),
        Arc::new(crate::extensions::pruner::ContextPruner),
        Arc::new(DeclaredSettings(
            &[TEST_FIELD],
            json!({"flag": false, "internal": 42}),
        )),
    ])
    .await
    .unwrap();
    let naming = hub.registered_setting_fields("session_naming").unwrap();
    assert_eq!(naming.fields().len(), 2);
    assert_eq!(naming.default_value("enabled"), Some(&json!(false)));
    assert_eq!(naming.default_value("model"), Some(&Value::Null));
    assert!(naming.field("attempted").is_none());
    let pruner = hub.registered_setting_fields("context_prune").unwrap();
    assert_eq!(pruner.default_value("min_bytes"), Some(&json!(16384)));
    assert!(pruner.field("enabled").is_none());
    assert!(hub.registered_setting_fields("unknown").is_none());
    let declared = hub.registered_setting_fields("declared").unwrap();
    assert!(declared.default_value("internal").is_none());

    let (_inst, db) = make_session_db().await;
    let id = db.root_id().to_string();
    let before = db.snapshot().await.unwrap();
    for (target, extension, key, expected) in [
        ("stale", "session_naming", "enabled", "no longer matches"),
        (&id, "declared", "flag", "does not allow session overrides"),
        (&id, "declared", "internal", "no declared field"),
    ] {
        for edit in [SettingEdit::Set(json!(true)), SettingEdit::Reset] {
            let error = hub
                .edit_session_setting(target, &db, extension, key, edit)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }
    assert_eq!(db.snapshot().await.unwrap(), before);
}

async fn legacy(db: &Database, ext: &str, text: &str) {
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<DocStore>(RETAINED_STORE)
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
    assert_eq!(migrate_settings(&db).await.unwrap(), 2);
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
        write_settings(
            &db,
            "memory",
            json!({"left":0,"right":0,"enabled":false,"nested":{"a":0,"b":0},"same":0}),
        )
        .await
        .unwrap();
        let snapshot = db.snapshot().await.unwrap();
        let a = db.new_transaction_at(&snapshot).await.unwrap();
        let b = db.clone().new_transaction_at(&snapshot).await.unwrap();
        update_settings_in(&a, "memory", true, true, |v| {
            v["left"] = json!(1);
            v["nested"]["a"] = json!(1);
            v["same"] = json!(7);
            Ok(())
        })
        .await
        .unwrap();
        update_settings_in(&b, "memory", true, true, |v| {
            v["right"] = json!(2);
            v["nested"]["b"] = json!(2);
            v["same"] = json!(7);
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
            json!({"left":1,"right":2,"enabled":false,"nested":{"a":1,"b":2},"same":7})
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
    bad.set(NUMBER, true);
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
        r#"{"left":0,"right":0,"same":0,"enabled":false,"nested":{"a":0,"b":0},"list":[1,2,3]}"#,
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
        v["nested"]["a"] = json!(1);
        v["list"] = json!([4]);
        Ok(())
    })
    .await
    .unwrap();
    update_settings(&pdb, "memory", true, |v| {
        v["right"] = json!(2);
        v["same"] = json!("peer");
        v["nested"]["b"] = json!(2);
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
    assert_eq!(local["nested"], json!({"a":1,"b":2}));
    assert_eq!(local["list"], json!([4]));
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

#[tokio::test]
async fn native_settings_use_native_types_with_only_pinned_api_exceptions() {
    let (_inst, db) = make_session_db().await;
    let value = json!({
        "enabled":false,"limit":8,"nothing":null,"text":"{not JSON}",
        "nested":{"name":"native","a.b":1,"":null,"雪":true,"~array":"literal"},
        "items":[false,{"x":2},null],"huge":u64::MAX,"fraction":1.25,"zero":-0.0,
        "a.b":3,"a":{"b":4},"~number":"not a wrapper"
    });
    write_settings(&db, "literal.extension", value.clone())
        .await
        .unwrap();
    write_settings(&db, "literal", json!({"extension":"separate"}))
        .await
        .unwrap();
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
        .await
        .unwrap();
    let raw = store.get(field_key("literal.extension")).await.unwrap();
    let doc = raw.as_doc().unwrap();
    assert_eq!(doc.get("enabled"), Some(&NativeValue::Bool(false)));
    assert_eq!(doc.get("limit"), Some(&NativeValue::Int(8)));
    assert_eq!(doc.get("nothing"), Some(&NativeValue::Null));
    assert_eq!(
        doc.get("text"),
        Some(&NativeValue::Text("{not JSON}".into()))
    );
    assert_eq!(
        doc.get("nested.name"),
        Some(&NativeValue::Text("native".into()))
    );
    assert_eq!(
        doc.get("nested").unwrap().as_doc().unwrap().get(""),
        Some(&NativeValue::Null)
    );
    assert_eq!(
        doc.get("items")
            .unwrap()
            .as_doc()
            .unwrap()
            .get(ARRAY)
            .unwrap()
            .as_list()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        doc.get("fraction").unwrap().as_doc().unwrap().get(NUMBER),
        Some(&NativeValue::Text("1.25".into()))
    );
    assert_eq!(decode(&raw).unwrap(), value);
    assert_eq!(
        read_settings_checked(&db, "literal.extension")
            .await
            .unwrap(),
        value
    );
    assert_eq!(
        read_settings_checked(&db, "literal").await.unwrap(),
        json!({"extension":"separate"})
    );
    let (_other, other_db) = make_session_db().await;
    assert_eq!(
        read_settings_checked(&other_db, "literal.extension")
            .await
            .unwrap(),
        json!({})
    );

    // The draft predecessor's format is only an explicit conversion input.
    let txn = db.new_transaction().await.unwrap();
    let mut hybrid = Doc::atomic();
    hybrid.set("k656e61626c6564", "false");
    hybrid.set("k6e6573746564", r#"{"array":[1.5,null]}"#);
    txn.get_store::<DocStore>(RETAINED_STORE)
        .await
        .unwrap()
        .set("hybrid", hybrid)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(migrate_settings(&db).await.unwrap(), 1);
    let txn = db.new_transaction().await.unwrap();
    let native = txn
        .get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
        .await
        .unwrap()
        .get("hybrid")
        .await
        .unwrap();
    assert_eq!(
        native.as_doc().unwrap().get("enabled"),
        Some(&NativeValue::Bool(false))
    );
    assert_eq!(
        decode(&native).unwrap(),
        json!({"enabled":false,"nested":{"array":[1.5,null]}})
    );
}

#[tokio::test]
async fn native_settings_field_assignment_replaces_nested_objects_and_lists() {
    let (_inst, db) = make_session_db().await;
    write_settings(
        &db,
        "memory",
        json!({"nested":{"old":1,"keep":2},"list":[1,2,3],"unrelated":false}),
    )
    .await
    .unwrap();
    patch_settings(
        &db,
        "memory",
        true,
        json!({"nested":{"new":3},"list":[4]})
            .as_object()
            .unwrap()
            .clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"nested":{"new":3},"list":[4],"unrelated":false})
    );
    patch_settings(
        &db,
        "memory",
        true,
        json!({"nested":{},"list":[]}).as_object().unwrap().clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"nested":{},"list":[],"unrelated":false})
    );
    // Deletion-only nested deltas must not be discarded as native is_empty().
    update_settings(&db, "memory", true, |v| {
        v["nested"] = json!({"remove":true,"retain":null});
        Ok(())
    })
    .await
    .unwrap();
    update_settings(&db, "memory", true, |v| {
        v["nested"].as_object_mut().unwrap().remove("remove");
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap()["nested"],
        json!({"retain":null})
    );
    write_settings(&db, "memory", json!({"only":true}))
        .await
        .unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"only":true})
    );
    let before = db.snapshot().await.unwrap();
    update_settings(&db, "memory", true, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(db.snapshot().await.unwrap(), before);
}

#[tokio::test]
async fn native_settings_nested_creation_preserves_independent_children() {
    for reverse in [false, true] {
        let (_inst, db) = make_session_db().await;
        let snapshot = db.snapshot().await.unwrap();
        let a = db.new_transaction_at(&snapshot).await.unwrap();
        let b = db.new_transaction_at(&snapshot).await.unwrap();
        update_settings_in(&a, "memory", true, true, |v| {
            v["nested"]["left"] = json!(1);
            Ok(())
        })
        .await
        .unwrap();
        update_settings_in(&b, "memory", true, true, |v| {
            v["nested"]["right"] = json!(2);
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
            json!({"nested":{"left":1,"right":2}})
        );
    }
}

#[tokio::test]
async fn native_settings_backend_missing_resource_is_not_a_missing_key() {
    let (_inst, mut user, faults) =
        crate::test_support::counting_backend::counting_eidetica().await;
    let key = user.get_default_key().unwrap();
    let db = user.create_database(Doc::new(), &key).await.unwrap();
    legacy(&db, "memory", r#"{"enabled":true}"#).await;
    write_settings(&db, "memory", json!({"enabled":false}))
        .await
        .unwrap();
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"enabled":false})
    );
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
        .await
        .unwrap();
    faults.fail_store_reads(Some(EXTENSION_SETTINGS_STORE));
    let error = store.get("memory").await.unwrap_err();
    assert!(matches!(error, eidetica::Error::Backend(_)) && error.is_not_found());
    assert!(
        read_settings_checked(&db, "memory").await.is_err(),
        "backend failure must not expose the stale retained enabled:true"
    );
    assert_eq!(read_settings(&db, "memory").await, json!({}));
    assert!(
        update_settings(&db, "memory", true, |v| {
            v["enabled"] = json!(true);
            Ok(())
        })
        .await
        .is_err()
    );
    faults.fail_store_reads(None);
    assert_eq!(
        read_settings_checked(&db, "memory").await.unwrap(),
        json!({"enabled":false})
    );
}
