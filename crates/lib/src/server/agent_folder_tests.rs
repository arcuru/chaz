//! Real configuration/storage/runtime boundaries; every model request is
//! recorded by MockBackend (no provider fallback) and roots are disposable.

use super::*;
use crate::agent_db::WorkerDbConfig;
use crate::tool::Tool;
use std::time::Duration;

async fn portable_agent(
    server: &Arc<Server>,
    registry: &crate::session::SessionRegistry,
    sources: &std::path::Path,
) -> (DbEntry, crate::agent_db::AgentDb) {
    let (entry, db) = seed_agent(server, registry, "alpha").await;
    let first = sources.join("first.md");
    let second = sources.join("second.md");
    let worker = sources.join("worker.md");
    std::fs::write(&first, "FIRST\n").unwrap();
    std::fs::write(&second, "SECOND\n").unwrap();
    std::fs::write(&worker, "WORKER\n").unwrap();
    let mut cfg = db.read_config().await.unwrap();
    cfg.system_prompt = "INLINE".into();
    cfg.system_prompt_files = vec![first.display().to_string(), second.display().to_string()];
    cfg.workers = vec![WorkerDbConfig {
        name: "helper".into(),
        system_prompt: "WORKER INLINE".into(),
        system_prompt_files: vec![worker.display().to_string()],
        ..Default::default()
    }];
    crate::agent_bundle::import_sources(&mut cfg).unwrap();
    db.write_config(&cfg).await.unwrap();
    (entry, db)
}

fn folder(server: &Server, entry: &DbEntry) -> std::path::PathBuf {
    server
        .agent_folders()
        .unwrap()
        .folder(&entry.display_name, &entry.db_id.to_string())
}

async fn await_file(path: &std::path::Path, expected: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::read_to_string(path).ok().as_deref() == Some(expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} did not become {expected:?}", path.display()));
}

async fn record_model_prompts(
    server: &Arc<Server>,
    registry: &Arc<crate::session::SessionRegistry>,
    entry: &DbEntry,
    parent_prompt: &str,
    worker_prompt: &str,
) {
    let db = registry
        .open_agent_db(&entry.db_id, None)
        .await
        .unwrap()
        .unwrap();
    let cfg = db.read_config().await.unwrap();
    let (_conv, parent_db) = registry
        .create_session(Some("folder-runtime"))
        .await
        .unwrap();
    let sid = parent_db.root_id().to_string();
    let local_entry = server.agent_index().find_by_id(&entry.db_id).unwrap();
    registry
        .attach_agent_to_session(&sid, &local_entry)
        .await
        .unwrap();
    let mock = Arc::new(crate::test_support::MockBackend::new());
    // A model response that looks like administration is still just a reply.
    mock.push_text("/agent set alpha model forbidden-model");
    mock.push_text("worker response");
    let backend = BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    let ctx = crate::commands::CommandContext {
        server,
        secrets: &secrets,
        backend: &backend,
        session_db_id: &sid,
        session_db: &parent_db,
        current_agent: "alpha",
        session_name: None,
    };
    match crate::commands::dispatch(
        crate::commands::Command::AgentRehost {
            agent_ref: "alpha".into(),
            pubkey: None,
            scope: crate::commands::RehostScope::Session,
            clear: false,
        },
        &ctx,
    )
    .await
    {
        crate::commands::CommandOutcome::Text(_) => {}
        crate::commands::CommandOutcome::Error(error) => panic!("{error}"),
        _ => panic!("unexpected rehost outcome"),
    }
    server
        .register_session(&parent_db, backend.clone(), Some("alpha".into()), None)
        .await
        .unwrap();
    let mut session = Session::new(ConversationId(sid.clone()), parent_db.clone()).await;
    session
        .add_entry(SessionEntry {
            sender: "user".into(),
            content: "record parent prompt".into(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    assert!(
        server.peer_is_home_for(&sid, "alpha").await,
        "recording peer must be the session home"
    );
    tokio::time::timeout(Duration::from_secs(5), await_call_count(&mock, 1))
        .await
        .expect("recorded parent request did not reach stub");
    await_no_processing(server, &sid).await;
    assert_eq!(
        db.read_config().await.unwrap(),
        cfg,
        "model output is never command-dispatched"
    );

    let slot = crate::instance::ServerSlot::default();
    slot.set(server.clone());
    let tool = crate::tools::SpawnWorker {
        server: slot,
        backend,
        security: crate::test_support::permissive_security(),
    };
    let mut ctx = crate::test_support::tool_context(
        Arc::new(Mutex::new(
            Session::new(ConversationId(sid), parent_db).await,
        )),
        Arc::new(ToolRegistry::new()),
    );
    ctx.agent_name = "alpha".into();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tool.execute(
            serde_json::json!({"name":"helper","task":"record worker prompt"}),
            &ctx,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, "worker response");
    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 2);
    for (call, expected) in calls.iter().zip([parent_prompt, worker_prompt]) {
        assert!(
            call.messages.iter().any(
                |m| matches!(m, crate::runtime::RuntimeMessage::System(text) if text == expected)
            ),
            "wrong request prompt: {:?}",
            call.messages
        );
    }
}

#[tokio::test]
async fn folder_reload_drives_parent_and_worker_recorded_model_prompts() {
    let (_instance, server, registry) = server_fixture().await;
    let external = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    server.set_agent_config_root(local.path().join("agents"));
    let (entry, db) = portable_agent(&server, &registry, external.path()).await;
    server.initialize_agent_folders().await;
    let folder = folder(&server, &entry);
    let accepted = db.read_config().await.unwrap();
    let parent_file = folder.join(&accepted.system_prompt_files[0]);
    let worker_file = folder.join(&accepted.workers[0].system_prompt_files[0]);
    std::fs::write(&parent_file, "DRAFT").unwrap();
    let no_op = server
        .update_agent_config("alpha", false, false, |_| Ok(()))
        .await
        .unwrap();
    assert!(!no_op.changed);
    assert_eq!(std::fs::read_to_string(&parent_file).unwrap(), "DRAFT");
    db.write_meta(&AgentMeta {
        description: Some("unrelated memory/admin metadata".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(
        !server
            .project_agent_config("alpha", false)
            .await
            .unwrap()
            .changed
    );
    assert_eq!(std::fs::read_to_string(&parent_file).unwrap(), "DRAFT");
    let outcome = server
        .update_agent_config("alpha", false, false, |cfg| {
            cfg.model = Some("new-model".into());
            Ok(())
        })
        .await
        .unwrap();
    assert!(outcome.changed);
    assert!(outcome.projection_error.is_none());
    assert_eq!(
        db.read_config().await.unwrap().prompt_sources,
        accepted.prompt_sources,
        "scalar edit cannot import drafts"
    );
    assert_eq!(
        std::fs::read_to_string(&parent_file).unwrap(),
        "FIRST\n",
        "accepted update overwrites pending draft"
    );

    // Content-only edits, with no path change, become active only on reload.
    std::fs::write(&parent_file, "NEW PARENT\n").unwrap();
    std::fs::write(&worker_file, "NEW WORKER\n").unwrap();
    let diff = server.agent_diff("alpha").await.unwrap().unwrap();
    assert!(diff.rows.iter().any(
        |r| r.label == "prompt_contents" && r.status == crate::agent_diff::FieldStatus::Changed
    ));
    let reload = server.reload_agent_folder("alpha").await.unwrap();
    assert!(reload.changed && reload.projection_error.is_none());
    let cfg = db.read_config().await.unwrap();
    assert_eq!(cfg.system_prompt_files, accepted.system_prompt_files);
    assert_eq!(
        cfg.workers[0].system_prompt_files,
        accepted.workers[0].system_prompt_files
    );
    assert_eq!(
        std::fs::read_to_string(external.path().join("first.md")).unwrap(),
        "FIRST\n"
    );
    drop(external); // A next turn has no original files available to fall back to.

    record_model_prompts(
        &server,
        &registry,
        &entry,
        "NEW PARENT\n\nSECOND\n\nINLINE",
        "NEW WORKER\n\nWORKER INLINE",
    )
    .await;
    server.shutdown().await;
}

#[tokio::test]
async fn folder_migration_preserves_saved_text_and_unavailable_snapshot_blocks() {
    let (_instance, server, registry) = server_fixture().await;
    let local = tempfile::tempdir().unwrap();
    let external = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(external.path(), "SAVED FILE\n").unwrap();
    let (_entry, db) = seed_agent(&server, &registry, "alpha").await;
    let mut legacy = db.read_config().await.unwrap();
    legacy.system_prompt = "INLINE WITH TRAILING  \n".into();
    legacy.system_prompt_files = vec![external.path().display().to_string()];
    server.refresh_prompt_ref(&mut legacy).await.unwrap();
    legacy.applied_config_hash = Some("historical-bookkeeping".into());
    legacy.default_memory_banks = vec!["keep".into()];
    db.write_config(&legacy).await.unwrap();
    std::fs::write(external.path(), "EXTERNAL EDIT MUST NOT BECOME ACTIVE").unwrap();
    server.set_agent_config_root(local.path().join("agents"));
    let outcome = server.project_agent_config("alpha", false).await.unwrap();
    assert!(outcome.flattened);
    let migrated = db.read_config().await.unwrap();
    assert_eq!(migrated.applied_config_hash, legacy.applied_config_hash);
    assert_eq!(migrated.default_memory_banks, legacy.default_memory_banks);
    assert_eq!(
        server.agents().get("alpha").unwrap().system_prompt,
        "SAVED FILE\n\nINLINE WITH TRAILING  \n"
    );
    assert_eq!(
        std::fs::read_to_string(external.path()).unwrap(),
        "EXTERNAL EDIT MUST NOT BECOME ACTIVE"
    );
    let (entry, missing) = seed_agent(&server, &registry, "missing").await;
    let mut unrecoverable = missing.read_config().await.unwrap();
    // A snapshot on a different DB cannot resolve in this peer's prompt store.
    unrecoverable.system_prompt_ref = Some(db.database().snapshot().await.unwrap());
    missing.write_config(&unrecoverable).await.unwrap();
    let before = missing.database().snapshot().await.unwrap();
    let error = server
        .project_agent_config("missing", false)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("saved prompt snapshot unavailable"),
        "{error:#}"
    );
    assert_eq!(missing.database().snapshot().await.unwrap(), before);
    assert_eq!(missing.read_config().await.unwrap(), unrecoverable);
    assert!(
        !folder(&server, &entry).exists(),
        "must not invent a partial bundle"
    );
    let runtime = server
        .agents()
        .build_from_db_config("missing", &AgentDbConfig::default());
    assert!(
        server.agent_for_execution(runtime).await.is_err(),
        "must not execute a guessed prompt after migration failure"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn folder_failures_preserve_active_state_and_committed_projection_is_repairable() {
    let (_instance, server, registry) = server_fixture().await;
    let external = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    server.set_agent_config_root(local.path().join("agents"));
    let (entry, db) = portable_agent(&server, &registry, external.path()).await;
    server.initialize_agent_folders().await;
    let folder = folder(&server, &entry);
    let cfg = db.read_config().await.unwrap();
    let active_prompt = server.agents().get("alpha").unwrap().system_prompt;
    let yaml_path = folder.join("agent.yaml");
    let valid = std::fs::read_to_string(&yaml_path).unwrap();
    for invalid in [
        "invalid: [".to_string(),
        valid.replace("version: 1", "version: 99"),
        valid.replace(&entry.db_id.to_string(), "wrong-identity"),
        format!("{valid}\nunknown: true\n"),
        format!("{valid}\nmodel: duplicate\n"),
    ] {
        std::fs::write(&yaml_path, &invalid).unwrap();
        let before = db.database().snapshot().await.unwrap();
        assert!(
            server.reload_agent_folder("alpha").await.is_err(),
            "accepted invalid document"
        );
        assert_eq!(db.database().snapshot().await.unwrap(), before);
        assert_eq!(db.read_config().await.unwrap(), cfg);
        assert_eq!(
            server.agents().get("alpha").unwrap().system_prompt,
            active_prompt
        );
    }
    std::fs::write(&yaml_path, &valid).unwrap();
    let source = folder.join(&cfg.system_prompt_files[0]);
    std::fs::remove_file(&source).unwrap();
    assert!(server.reload_agent_folder("alpha").await.is_err());
    assert_eq!(db.read_config().await.unwrap(), cfg);
    // A real filesystem type failure occurs AFTER the DB accepts a scalar
    // edit. The journal remembers outward intent; reload must refuse it.
    std::fs::create_dir(&source).unwrap();
    let outcome = server
        .update_agent_config("alpha", false, false, |cfg| {
            cfg.model = Some("committed".into());
            Ok(())
        })
        .await
        .unwrap();
    assert!(outcome.changed && outcome.projection_error.is_some());
    assert_eq!(
        db.read_config().await.unwrap().model.as_deref(),
        Some("committed")
    );
    assert!(server.agent_folder_status("alpha").contains("blocked"));
    assert!(server.reload_agent_folder("alpha").await.is_err());
    let journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(folder.join(".projection.json")).unwrap()).unwrap();
    assert!(journal["pending"].is_string());
    std::fs::remove_dir(&source).unwrap();
    std::fs::write(folder.join("notes.txt"), "UNRELATED").unwrap();
    let repaired = server.project_agent_config("alpha", false).await.unwrap();
    assert!(!repaired.changed && repaired.projection_error.is_none());
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "FIRST\n");
    assert_eq!(
        std::fs::read_to_string(folder.join("notes.txt")).unwrap(),
        "UNRELATED"
    );
    assert_eq!(
        server.agents().get("alpha").unwrap().system_prompt,
        active_prompt
    );
    server.shutdown().await;
}

#[tokio::test]
async fn folder_two_peer_sync_reconstructs_sources_without_feedback() {
    use eidetica::auth::Permission;
    use eidetica::auth::types::AuthKey;
    use eidetica::sync::Address;
    use eidetica::sync::transports::http::HttpTransport;

    let (owner, server, registry) = server_fixture().await;
    let external = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    server.set_agent_config_root(local.path().join("agents"));
    let (entry, db) = portable_agent(&server, &registry, external.path()).await;
    server.initialize_agent_folders().await;
    let cfg = db.read_config().await.unwrap();
    let expected_sources = cfg.prompt_sources.as_ref().unwrap().files.clone();
    drop(external);
    owner.enable_sync().await.unwrap();
    let txn = db.database().new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_global_auth_key(AuthKey::active(None, Permission::Write(0)))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    db.database().share().await.unwrap();
    let owner_sync = owner.sync().unwrap();
    owner_sync
        .register_transport("http", HttpTransport::builder().bind("127.0.0.1:0"))
        .await
        .unwrap();
    owner_sync.accept_connections().await.unwrap();
    let address = Address::http(owner_sync.get_server_address_for("http").await.unwrap());
    let (peer, mut peer_user) =
        Instance::create_backend(Box::new(InMemory::new()), NewAccount::passwordless("peer"))
            .await
            .unwrap();
    peer.enable_sync().await.unwrap();
    let peer_key = peer_user.get_default_key().unwrap();
    let peer_signing = peer_user.get_signing_key(&peer_key).unwrap();
    let peer_sync = peer.sync().unwrap();
    peer_sync
        .register_transport("http", HttpTransport::builder())
        .await
        .unwrap();
    peer_sync
        .sync_with_peer_for_bootstrap_with_key(
            &address,
            &entry.db_id,
            &peer_signing,
            &peer_key.to_string(),
            Permission::Write(10),
        )
        .await
        .unwrap();
    peer_sync.flush().await.unwrap();
    let (sigkey, _) = eidetica::Database::find_sigkeys(&peer, &entry.db_id, &peer_key)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    peer_user
        .map_key(&peer_key, &entry.db_id, sigkey)
        .await
        .unwrap();
    let peer_registry = Arc::new(
        crate::session::SessionRegistry::new(
            peer.clone(),
            peer_user,
            Arc::new(AgentRegistry::with_default_agent()),
        )
        .await
        .unwrap(),
    );
    let (_, peer_server, _) = server_fixture_from_registry(peer_registry.clone()).await;
    peer_server.agent_index().register(DbEntry {
        pubkey: peer_key,
        ..entry.clone()
    });
    peer_server.set_agent_config_root(remote.path().join("agents"));
    peer_server.initialize_agent_folders().await;
    let peer_db = peer_registry
        .open_agent_db(&entry.db_id, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer_db.read_config().await.unwrap(), cfg);
    let peer_folder = folder(&peer_server, &entry);
    for (path, text) in &expected_sources {
        assert_eq!(
            std::fs::read_to_string(peer_folder.join(path)).unwrap(),
            *text
        );
    }
    assert_eq!(
        peer_server.agents().get("alpha").unwrap().system_prompt,
        "FIRST\n\nSECOND\n\nINLINE"
    );
    assert_eq!(
        peer_server.agents().get("alpha").unwrap().workers["helper"].system_prompt,
        "WORKER\n\nWORKER INLINE"
    );
    // No peer-local prompt cache was copied; both config and runtime work.
    assert!(
        peer_db
            .read_config()
            .await
            .unwrap()
            .system_prompt_ref
            .is_none()
    );
    let before = peer_db.database().snapshot().await.unwrap();
    peer_server
        .project_agent_config("alpha", false)
        .await
        .unwrap();
    assert_eq!(
        peer_db.database().snapshot().await.unwrap(),
        before,
        "projection must never write back to synchronized config"
    );

    let target = peer_folder.join(&cfg.system_prompt_files[0]);
    std::fs::write(&target, "REMOTE DRAFT").unwrap();
    let mut changed = cfg.clone();
    changed
        .prompt_sources
        .as_mut()
        .unwrap()
        .files
        .insert(cfg.system_prompt_files[0].clone(), "SYNCED\n".into());
    let peer_folders = peer_server.agent_folders().unwrap();
    let foreign_projection = peer_folders.lock(&entry.db_id.to_string()).unwrap();
    db.write_config(&changed).await.unwrap(); // Goes through the actual DB notification, not the operator helper.
    peer_sync
        .sync_with_peer(&address, Some(&entry.db_id))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(75)).await;
    drop(foreign_projection);
    await_file(&target, "SYNCED\n").await;
    assert_eq!(peer_db.read_config().await.unwrap(), changed);
    let synced_tip = peer_db.database().snapshot().await.unwrap();
    peer_server
        .project_agent_config("alpha", false)
        .await
        .unwrap();
    assert_eq!(peer_db.database().snapshot().await.unwrap(), synced_tip);
    let mut meta = db.read_meta().await.unwrap();
    meta.description = Some("unrelated write".into());
    std::fs::write(&target, "KEEP DRAFT").unwrap();
    db.write_meta(&meta).await.unwrap();
    peer_sync
        .sync_with_peer(&address, Some(&entry.db_id))
        .await
        .unwrap();
    peer_server
        .project_agent_config("alpha", false)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "KEEP DRAFT");
    assert_eq!(
        peer_server.agents().get("alpha").unwrap().system_prompt,
        "SYNCED\n\nSECOND\n\nINLINE"
    );
    record_model_prompts(
        &peer_server,
        &peer_registry,
        &entry,
        "SYNCED\n\nSECOND\n\nINLINE",
        "WORKER\n\nWORKER INLINE",
    )
    .await;
    server.shutdown().await;
    peer_server.shutdown().await;
}

#[tokio::test]
async fn folder_replacement_adopts_only_explicit_sources_and_resets_scope() {
    let (_instance, server, registry) = server_fixture().await;
    let external = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    server.set_agent_config_root(local.path().join("agents"));
    let (entry, db) = portable_agent(&server, &registry, external.path()).await;
    let mut old = db.read_config().await.unwrap();
    old.autonomous = true;
    old.capabilities =
        serde_json::from_value(serde_json::json!({"fs":{"allow_write":[]}})).unwrap();
    old.grants.insert(
        "shell".into(),
        serde_json::from_value(serde_json::json!({"shell":{"allow":[]}})).unwrap(),
    );
    old.applied_config_hash = Some("preserve-history".into());
    old.default_memory_banks = vec!["preserve-bank".into()];
    old.default_skill_banks = vec!["preserve-skills".into()];
    db.write_config(&old).await.unwrap();
    let before_meta = db.read_meta().await.unwrap();
    server.initialize_agent_folders().await;
    let folder = folder(&server, &entry);
    let notes = folder.join("notes.txt");
    std::fs::write(&notes, "UNREFERENCED SECRET MUST STAY LOCAL").unwrap();
    std::fs::create_dir(folder.join("operator")).unwrap();
    std::fs::write(folder.join("operator/new.md"), "NEW EXPLICIT SOURCE\n").unwrap();
    std::fs::write(folder.join("agent.yaml"), format!("version: 1\nagent_id: {}\nsystem_prompt_files: [operator/new.md]\nmodel: null\ntools: []\nautonomous: false\nmax_spawn_depth: 10\n", entry.db_id)).unwrap();
    let outcome = server.reload_agent_folder("alpha").await.unwrap();
    assert!(
        outcome.changed && outcome.projection_error.is_none(),
        "{outcome:?}"
    );
    let after = db.read_config().await.unwrap();
    assert!(!after.autonomous);
    assert_eq!(after.model, None);
    assert_eq!(after.tools, Some(vec![]));
    assert_eq!(after.max_spawn_depth, Some(10));
    assert!(after.workers.is_empty() && after.grants.is_empty());
    assert_eq!(after.capabilities, crate::grants::Grants::default());
    assert_eq!(after.default_memory_banks, old.default_memory_banks);
    assert_eq!(after.default_skill_banks, old.default_skill_banks);
    assert_eq!(after.applied_config_hash, old.applied_config_hash);
    assert_eq!(db.read_meta().await.unwrap(), before_meta);
    assert_eq!(after.prompt_sources.as_ref().unwrap().files.len(), 1);
    assert_eq!(
        server.agents().get("alpha").unwrap().system_prompt,
        "NEW EXPLICIT SOURCE"
    );
    assert_eq!(
        std::fs::read_to_string(&notes).unwrap(),
        "UNREFERENCED SECRET MUST STAY LOCAL"
    );
    for removed in &old.system_prompt_files {
        assert!(!folder.join(removed).exists());
    }
    assert_eq!(
        std::fs::read_to_string(external.path().join("first.md")).unwrap(),
        "FIRST\n"
    );
    let (_tool_instance, session) = crate::test_support::fresh_session().await;
    let model_ctx = crate::test_support::tool_context_with_host(
        session,
        Arc::new(ToolRegistry::new()),
        server.host.clone(),
    );
    let error = crate::tools::WriteFile
        .execute(
            serde_json::json!({"path":folder.join("agent.yaml"),"content":"MODEL"}),
            &model_ctx,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("operator-owned"));
    let allowed = local.path().join("workspace.md");
    crate::tools::WriteFile
        .execute(
            serde_json::json!({"path":allowed,"content":"ALLOWED"}),
            &model_ctx,
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(allowed).unwrap(), "ALLOWED");
    // Sessionless trusted administration does not create a conversation/turn.
    let sessions_before = registry.list_sessions().await.unwrap().len();
    match crate::commands::dispatch_without_session(
        crate::commands::Command::AgentReload(Some(entry.db_id.to_string())),
        &server,
    )
    .await
    .unwrap_or_else(|_| panic!("reload must be sessionless"))
    {
        crate::commands::CommandOutcome::Text(_) => {}
        crate::commands::CommandOutcome::Error(error) => panic!("{error}"),
        _ => panic!("unexpected reload result"),
    }
    assert_eq!(
        registry.list_sessions().await.unwrap().len(),
        sessions_before
    );
    server.shutdown().await;
}

#[tokio::test]
async fn folder_rejected_commit_keeps_sources_runtime_and_projection_unchanged() {
    use eidetica::auth::Permission;
    use eidetica::auth::types::AuthKey;
    use eidetica::store::DocStore;
    let (_instance, server, registry) = server_fixture().await;
    let external = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    server.set_agent_config_root(local.path().join("agents"));
    let (entry, db) = portable_agent(&server, &registry, external.path()).await;
    server.initialize_agent_folders().await;
    let accepted = db.read_config().await.unwrap();
    let runtime = server.agents().get("alpha").unwrap().system_prompt;
    let folder = folder(&server, &entry);
    let yaml = std::fs::read(folder.join("agent.yaml")).unwrap();
    let journal = std::fs::read(folder.join(".projection.json")).unwrap();
    let mut attempted = accepted.clone();
    attempted.model = Some("REFUSED".into());
    attempted.prompt_sources.as_mut().unwrap().files.insert(
        accepted.system_prompt_files[0].clone(),
        "REFUSED SOURCE".into(),
    );
    let settings = db.database().new_transaction().await.unwrap();
    settings
        .get_settings()
        .unwrap()
        .set_auth_key(
            &entry.pubkey,
            AuthKey::active(Some("reader"), Permission::Read),
        )
        .await
        .unwrap();
    settings.commit().await.unwrap();
    let before = db.database().snapshot().await.unwrap();
    // Retain the already-open native handle. Reads and local staging succeed;
    // the actual engine commit must reject its now read-only signing key.
    let transaction = db.database().new_transaction().await.unwrap();
    transaction
        .get_store::<DocStore>("config")
        .await
        .unwrap()
        .set_string("value", serde_json::to_string(&attempted).unwrap())
        .await
        .unwrap();
    let error = transaction.commit().await.unwrap_err();
    assert_eq!(error.to_string(), "Entry validation failed");
    assert_eq!(db.database().snapshot().await.unwrap(), before);
    assert!(db.write_config(&attempted).await.is_err());
    assert!(
        server
            .update_agent_config("alpha", false, false, |cfg| {
                *cfg = attempted;
                Ok(())
            })
            .await
            .is_err()
    );
    assert_eq!(db.read_config().await.unwrap(), accepted);
    assert_eq!(server.agents().get("alpha").unwrap().system_prompt, runtime);
    assert_eq!(std::fs::read(folder.join("agent.yaml")).unwrap(), yaml);
    assert_eq!(
        std::fs::read(folder.join(".projection.json")).unwrap(),
        journal
    );
    server.shutdown().await;
}
