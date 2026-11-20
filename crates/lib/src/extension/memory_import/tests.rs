use super::*;
use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
use crate::test_support::fresh_eidetica;

fn scope() -> MemoryImportScope {
    MemoryImportScope {
        agent_name: "archivist".into(),
        expected_agent_display_name: "Archivist".into(),
        bank_name: "source-mirror".into(),
        owner: "note-import-v1".into(),
        state_store: "note_import".into(),
        row_prefix: "note:v1:".into(),
    }
}
fn snapshot() -> MemoryImportSnapshot {
    MemoryImportSnapshot {
        source: "notes/lighthouse.md".into(),
        hash: "original".into(),
        rows: vec![MemoryImportRow {
            id: "note:v1:0000".into(),
            key: "notes/lighthouse.md > Lamp".into(),
            value: "Untrusted excerpt: amberlighthouse beacon".into(),
            tags: vec![scope().owner, "sha256:original".into()],
        }],
    }
}
async fn agent(user: &mut AccountSession) -> (AgentDb, PublicKey) {
    create_agent_db(
        user,
        "archivist",
        &AgentDbConfig::default(),
        &AgentMeta {
            display_name: Some("Archivist".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}
async fn rows(bank: &MemoryBankDb) -> BTreeMap<String, MemoryEntry> {
    bank.database()
        .new_transaction()
        .await
        .unwrap()
        .get_store::<Table<MemoryEntry>>(MEMORY_STORE)
        .await
        .unwrap()
        .search(|_: &MemoryEntry| true)
        .await
        .unwrap()
        .into_iter()
        .collect()
}
async fn set_row(bank: &MemoryBankDb, id: &str, tags: Vec<String>) {
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_store::<Table<MemoryEntry>>(MEMORY_STORE)
        .await
        .unwrap()
        .set(
            id,
            MemoryEntry {
                key: "manual".into(),
                value: "manual entry preserved".into(),
                timestamp: chrono::Utc::now(),
                tags,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
async fn bank(user: &AccountSession) -> MemoryBankDb {
    MemoryBankDb::from_database(
        named_databases(user, "memory:source-mirror")
            .await
            .unwrap()
            .pop()
            .unwrap()
            .0,
    )
}

#[tokio::test]
async fn native_snapshot_lifecycle_and_write_boundaries() {
    let (instance, mut user) = fresh_eidetica().await;
    let (agent, reader) = agent(&mut user).await;
    let outsider = user.add_private_key(Some("outsider")).await.unwrap();
    let mut endpoint = NativeMemoryImport::bind(&mut user, scope()).await.unwrap();
    let id = endpoint.bank_id().to_string();
    let bank = bank(endpoint.user).await;
    let original = snapshot();
    assert_eq!(
        endpoint.replace_snapshot(&original).await.unwrap(),
        MemoryImportChanges {
            inserted: 1,
            state_changed: true,
            ..Default::default()
        }
    );
    let before = bank.database().snapshot().await.unwrap();
    let timestamp = rows(&bank).await["note:v1:0000"].timestamp;
    assert_eq!(
        endpoint.replace_snapshot(&original).await.unwrap(),
        MemoryImportChanges::default()
    );
    assert_eq!(
        before,
        bank.database().snapshot().await.unwrap(),
        "no-op must not commit"
    );
    let mut renamed = original.clone();
    renamed.source = "notes/renamed.md".into();
    assert_eq!(
        endpoint.replace_snapshot(&renamed).await.unwrap(),
        MemoryImportChanges {
            state_changed: true,
            ..Default::default()
        }
    );
    assert_eq!(rows(&bank).await["note:v1:0000"].timestamp, timestamp);
    set_row(&bank, "manual", vec![]).await; // positive owner-write control
    let mut edit = original.clone();
    edit.hash = "edited".into();
    edit.rows[0].value = "Untrusted excerpt: violetlighthouse beacon".into();
    assert_eq!(endpoint.replace_snapshot(&edit).await.unwrap().updated, 1);
    assert_eq!(rows(&bank).await.len(), 2);
    // Validate all intended rows before any commit, even if earlier ones change.
    let before = bank.database().snapshot().await.unwrap();
    let mut invalid = edit.clone();
    invalid.rows[0].value = "must not persist".into();
    invalid.rows.push(MemoryImportRow {
        id: "manual".into(),
        ..original.rows[0].clone()
    });
    assert!(endpoint.replace_snapshot(&invalid).await.is_err());
    assert_eq!(before, bank.database().snapshot().await.unwrap());
    invalid = edit.clone();
    invalid.rows.push(edit.rows[0].clone());
    assert!(endpoint.replace_snapshot(&invalid).await.is_err());
    set_row(&bank, "note:v1:collision", vec!["someone-else".into()]).await;
    let before = bank.database().snapshot().await.unwrap();
    assert!(
        endpoint
            .replace_snapshot(&original)
            .await
            .unwrap_err()
            .to_string()
            .contains("ownership conflict")
    );
    assert_eq!(before, bank.database().snapshot().await.unwrap());
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_store::<Table<MemoryEntry>>(MEMORY_STORE)
        .await
        .unwrap()
        .delete("note:v1:collision")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Explicit removal preserves manual rows and stores source/hash atomically.
    let removed = MemoryImportSnapshot {
        rows: vec![],
        hash: "removed".into(),
        ..original.clone()
    };
    assert_eq!(
        endpoint.replace_snapshot(&removed).await.unwrap().removed,
        1
    );
    assert_eq!(
        rows(&bank).await.keys().cloned().collect::<Vec<_>>(),
        vec!["manual"]
    );
    let tx = bank.database().new_transaction().await.unwrap();
    let state = tx
        .get_store::<DocStore>(&scope().state_store)
        .await
        .unwrap();
    assert_eq!(state.get_string("hash").await.unwrap(), "removed");
    assert_eq!(state.get_string("source").await.unwrap(), original.source);
    endpoint.replace_snapshot(&original).await.unwrap();
    assert!(rows(&bank).await["note:v1:0000"].timestamp >= timestamp);
    drop(endpoint);
    assert_eq!(
        NativeMemoryImport::bind(&mut user, scope())
            .await
            .unwrap()
            .bank_id(),
        id
    );
    assert_eq!(agent.list_memory_banks().await.unwrap().len(), 1);
    // Native authorization, not just a cached permission label.
    for key in [&reader, &outsider] {
        user.map_key(
            key,
            &bank.id(),
            eidetica::auth::types::SigKey::from_pubkey(key),
        )
        .await
        .unwrap();
        let opened = user.open_database_with_key(&bank.id(), key).await;
        if key == &reader {
            let tx = opened
                .as_ref()
                .expect("Read key must open bank")
                .new_transaction()
                .await
                .unwrap();
            let table = tx
                .get_store::<Table<MemoryEntry>>(MEMORY_STORE)
                .await
                .unwrap();
            assert!(
                !table
                    .search(|_: &MemoryEntry| true)
                    .await
                    .unwrap()
                    .is_empty(),
                "Read key must read imported rows"
            );
        }
        let attempted = async {
            let db = opened?;
            let tx = db.new_transaction().await?;
            let table = tx.get_store::<Table<MemoryEntry>>(MEMORY_STORE).await?;
            table
                .set("forbidden", rows(&bank).await["manual"].clone())
                .await?;
            tx.commit().await
        }
        .await;
        assert!(attempted.is_err(), "non-writer committed a bank mutation");
    }
    assert!(!rows(&bank).await.contains_key("forbidden"));
    // Invoke the existing model-facing tool using normal upstream fixture setup.
    use crate::{
        agent::AgentRegistry,
        hosted_index::build_from_user,
        session::SessionRegistry,
        tool::{Tool, ToolRegistry},
        tools::Remember,
    };
    use std::sync::Arc;
    let (agents, _, _) = build_from_user(&user).await.unwrap();
    let registry = Arc::new(
        SessionRegistry::new(
            instance,
            user,
            Arc::new(AgentRegistry::with_default_agent()),
        )
        .await
        .unwrap(),
    );
    let (session_id, db) = registry
        .create_session(Some("native-import-fixture"))
        .await
        .unwrap();
    let session = Arc::new(tokio::sync::Mutex::new(
        crate::session::Session::new(session_id, db).await,
    ));
    let mut ctx = crate::test_support::tool_context(session, Arc::new(ToolRegistry::new()));
    ctx.agent_name = "archivist".into();
    let remember = Remember::new(registry, agents, None);
    remember
        .execute(
            serde_json::json!({"key":"control", "value":"own-memory write"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        remember
            .execute(
                serde_json::json!({"bank":"source-mirror", "key":"forbidden", "value":"denied"}),
                &ctx
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("Read-only")
    );
    println!(
        "PASS native import: bootstrap/rebind, initial/no-op/edit/removal, atomic validation, collision/manual preservation, native Read/ungranted denial + owner positive, model own-write + imported Read denial"
    );
}

#[tokio::test]
async fn compatible_legacy_layout_and_revocation_refusal() {
    let (_instance, mut user) = fresh_eidetica().await;
    let (agent, reader) = agent(&mut user).await;
    // Existing layout, no migration/recreation: same stores, IDs and owner-first tags.
    let (bank, _) = create_memory_bank(&mut user, &scope().bank_name, &MemoryBankMeta::default())
        .await
        .unwrap();
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_store::<DocStore>(&scope().state_store)
        .await
        .unwrap()
        .set_string("owner", &scope().owner)
        .await
        .unwrap();
    let state = tx
        .get_store::<DocStore>(&scope().state_store)
        .await
        .unwrap();
    state.set_string("hash", &snapshot().hash).await.unwrap();
    state
        .set_string("source", &snapshot().source)
        .await
        .unwrap();
    let row = snapshot().rows.remove(0);
    tx.get_store::<Table<MemoryEntry>>(MEMORY_STORE)
        .await
        .unwrap()
        .set(
            &row.id,
            MemoryEntry {
                key: row.key,
                value: row.value,
                tags: row.tags,
                timestamp: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut endpoint = NativeMemoryImport::bind(&mut user, scope()).await.unwrap();
    assert_eq!(endpoint.bank_id(), bank.id().to_string());
    assert_eq!(
        endpoint.replace_snapshot(&snapshot()).await.unwrap(),
        MemoryImportChanges::default()
    );
    assert_eq!(
        rows(&bank).await["note:v1:0000"].tags,
        snapshot().rows[0].tags
    );
    let reference = agent
        .find_memory_bank(&scope().bank_name)
        .await
        .unwrap()
        .unwrap();
    agent.detach_memory_bank(&scope().bank_name).await.unwrap();
    assert!(
        endpoint
            .replace_snapshot(&snapshot())
            .await
            .unwrap_err()
            .to_string()
            .contains("detached/revoked")
    );
    drop(endpoint);
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    assert!(
        agent
            .find_memory_bank(&scope().bank_name)
            .await
            .unwrap()
            .is_none()
    );
    agent.attach_memory_bank(reference.clone()).await.unwrap(); // explicit fixture restoration
    let mut endpoint = NativeMemoryImport::bind(&mut user, scope()).await.unwrap();
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_settings()
        .unwrap()
        .revoke_auth_key(&reader)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        endpoint
            .replace_snapshot(&snapshot())
            .await
            .unwrap_err()
            .to_string()
            .contains("active Read")
    );
    drop(endpoint);
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_settings()
        .unwrap()
        .set_auth_key(&reader, AuthKey::active(None, Permission::Write(0)))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_settings()
        .unwrap()
        .set_auth_key(&reader, AuthKey::active(None, Permission::Read))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    agent
        .attach_memory_bank(MemoryBankRef {
            permission: BankPermission::Write,
            ..reference.clone()
        })
        .await
        .unwrap();
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    agent
        .attach_memory_bank(MemoryBankRef {
            db_id: agent.id().to_string(),
            ..reference.clone()
        })
        .await
        .unwrap();
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    agent.attach_memory_bank(reference).await.unwrap();
    let mut wrong = scope();
    wrong.owner = "other-importer".into();
    assert!(NativeMemoryImport::bind(&mut user, wrong).await.is_err());
    let extra = user
        .add_private_key(Some("unexpected-reader"))
        .await
        .unwrap();
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_settings()
        .unwrap()
        .set_auth_key(&extra, AuthKey::active(None, Permission::Read))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_settings()
        .unwrap()
        .revoke_auth_key(&extra)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Account-unsynced is a separate, current check, even for an already-bound handle.
    let mut endpoint = NativeMemoryImport::bind(&mut user, scope()).await.unwrap();
    bank.database().set_shared(true).await.unwrap();
    assert!(
        endpoint
            .replace_snapshot(&snapshot())
            .await
            .unwrap_err()
            .to_string()
            .contains("sharing enabled")
    );
    drop(endpoint);
    bank.database().set_shared(false).await.unwrap(); // fixture only, never endpoint repair
    let tx = bank.database().new_transaction().await.unwrap();
    tx.get_settings()
        .unwrap()
        .set_global_auth_key(AuthKey::active(None, Permission::Read))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    println!(
        "PASS legacy layout reuse and wrong owner/detached/revoked/wider grant/reference/unexpected ACL/sharing/global access refusal"
    );
}

#[tokio::test]
async fn missing_ambiguous_identity_and_orphaned_setup_refuse() {
    let (_instance, mut user) = fresh_eidetica().await;
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    assert!(
        named_databases(&user, "memory:source-mirror")
            .await
            .unwrap()
            .is_empty()
    );
    agent(&mut user).await;
    let mut wrong = scope();
    wrong.state_store = MEMORY_STORE.into();
    assert!(NativeMemoryImport::bind(&mut user, wrong).await.is_err());
    let mut wrong = scope();
    wrong.row_prefix.clear();
    assert!(NativeMemoryImport::bind(&mut user, wrong).await.is_err());
    let mut wrong = scope();
    wrong.expected_agent_display_name = "Wrong".into();
    assert!(NativeMemoryImport::bind(&mut user, wrong).await.is_err());
    user.add_private_key(Some("memory:source-mirror"))
        .await
        .unwrap();
    assert!(
        NativeMemoryImport::bind(&mut user, scope())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("orphaned")
    );
    agent(&mut user).await;
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    let (_instance, mut user) = fresh_eidetica().await;
    agent(&mut user).await;
    for _ in 0..2 {
        create_memory_bank(&mut user, "source-mirror", &MemoryBankMeta::default())
            .await
            .unwrap();
    }
    assert!(NativeMemoryImport::bind(&mut user, scope()).await.is_err());
    println!("PASS missing/ambiguous/mismatched agent, duplicate bank and orphaned key refusal");
}
