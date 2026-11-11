//! Hosted Agent DBs are the only agent authority: root config defines no
//! agents, and managed folders explicitly round-trip scoped config. These tests
//! drive the real daemon build (`build` + its deferred startup) over an
//! Eidetica service over on-disk SQLite, create agents through administration,
//! stop both, and rebuild from the same file. No model backend is configured,
//! so nothing touches the network.

use super::build::{BuildOptions, BuiltServer, McpReadiness, build};
use crate::agent_db::AgentDbConfig;
use crate::commands::{Command, CommandOutcome};
use crate::config::Config;
use eidetica::Instance;
use std::path::{Path, PathBuf};

struct Disk {
    _dir: tempfile::TempDir,
    url: String,
    config_path: PathBuf,
    prompt_path: PathBuf,
    socket: PathBuf,
}

struct Service {
    shutdown: tokio::sync::watch::Sender<()>,
    task: tokio::task::JoinHandle<eidetica::Result<()>>,
}

impl Disk {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("chaz.db").display());
        let config_path = dir.path().join("config.yaml");
        let prompt_path = dir.path().join("PROMPT.md");
        let socket = dir.path().join("service.sock");
        Self {
            _dir: dir,
            url,
            config_path,
            prompt_path,
            socket,
        }
    }

    /// Write a runner config with `extra` root settings into the config file
    /// and parse it the way the daemon does. It never defines agents.
    fn config(&self, extra: &str) -> Config {
        let body = format!(
            "execution: executor\neidetica:\n  connection: \"{}\"\n  login: {{ username: chaz, passwordless: true }}\n{extra}",
            format_args!("unix://{}", self.socket.display())
        );
        std::fs::write(&self.config_path, &body).unwrap();
        let mut config: Config = serde_yaml::from_str(&body).unwrap();
        config.agent_config_dir = Some(self._dir.path().join("agents").display().to_string());
        config
    }
}

async fn boot(disk: &Disk, config: Config, first: bool) -> (BuiltServer, Service) {
    let (built, service) = try_boot(disk, config, first).await;
    (built.unwrap(), service)
}

async fn try_boot(
    disk: &Disk,
    mut config: Config,
    first: bool,
) -> (anyhow::Result<BuiltServer>, Service) {
    let owner = if first {
        let (instance, user) = Instance::create_backend(
            Box::new(
                eidetica::backend::database::Sqlite::connect(&disk.url)
                    .await
                    .unwrap(),
            ),
            eidetica::NewAccount::passwordless("chaz"),
        )
        .await
        .unwrap();
        drop(user);
        instance
    } else {
        // The previous generation's handles are dropped, but background
        // tasks may still be unwinding; wait for the store to be released.

        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                match Instance::connect(&disk.url).await {
                    Ok(instance) => break instance,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                }
            }
        })
        .await
        .expect("previous generation never released the store")
    };
    let server = eidetica::service::ServiceServer::bind(owner, &disk.socket)
        .await
        .unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(());
    let service = Service {
        shutdown,
        task: tokio::spawn(server.run(receiver)),
    };
    let instance = Instance::connect(&format!("unix://{}", disk.socket.display()))
        .await
        .unwrap();
    let user = instance.login_account("chaz", None).await.unwrap();
    let built = Box::pin(build(
        &mut config,
        instance,
        user,
        BuildOptions {
            config_path: disk.config_path.clone(),
            run_routine_engine: false,
            run_agent_loop: false,
            extra_auto_approved_tools: Vec::new(),
            mcp_readiness: McpReadiness::Deferred,
            targeted_session: None,
        },
    ))
    .await;
    let Ok(built) = built else {
        return (built, service);
    };
    // Folder initialization runs in the deferred startup task, before the
    // processing gate opens.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !built.server.is_startup_ready() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deferred startup never opened the processing gate");
    (Ok(built), service)
}

/// Shut the runtime down and release every handle on the store, so the next
/// boot reopens the file the way a restarted process would.
async fn stop(built: BuiltServer, service: Service) {
    built.shutdown().await;
    drop(built);
    drop(service.shutdown);
    tokio::time::timeout(std::time::Duration::from_secs(30), service.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

async fn read_agent(built: &BuiltServer, name: &str) -> (String, AgentDbConfig) {
    let (entry, db) = open_agent(built, name).await;
    (
        format!("{}/{}", entry.db_id, entry.pubkey.to_prefixed_string()),
        db.read_config().await.unwrap(),
    )
}

async fn open_agent(
    built: &BuiltServer,
    name: &str,
) -> (crate::hosted_index::DbEntry, crate::agent_db::AgentDb) {
    let entry = built
        .server
        .agent_index()
        .find_by_name(name)
        .unwrap_or_else(|| panic!("agent {name} not hosted"));
    let db = built
        .registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    (entry, db)
}

async fn command(built: &BuiltServer, cmd: Command) -> Result<String, String> {
    match Box::pin(crate::commands::dispatch_without_session(
        cmd,
        &built.server,
    ))
    .await
    {
        Ok(CommandOutcome::Text(text)) => Ok(text),
        Ok(CommandOutcome::Error(error)) => Err(error),
        _ => Err("expected sessionless administration outcome".into()),
    }
}

async fn set(built: &BuiltServer, field: &str, value: &str) -> Result<String, String> {
    command(
        built,
        Command::AgentSet {
            agent_ref: "alpha".into(),
            field: field.into(),
            value: value.into(),
        },
    )
    .await
}

/// Create an agent through the existing administration command, the way an
/// operator does on a runner whose config defines no agents.
async fn create(built: &BuiltServer, name: &str, overrides: &[(&str, &str)]) -> String {
    command(
        built,
        Command::AgentNew {
            name: name.into(),
            overrides: overrides
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        },
    )
    .await
    .unwrap()
}

async fn prompt(built: &BuiltServer, name: &str) -> String {
    let agent = built.server.agents().get(name).unwrap();
    built
        .server
        .hydrate_agent_from_db(agent)
        .await
        .system_prompt
}

async fn write_document(built: &BuiltServer, name: &str, cfg: &AgentDbConfig) {
    let entry = built.server.agent_index().find_by_name(name).unwrap();
    let folder = built
        .server
        .agent_folders()
        .unwrap()
        .folder(name, &entry.db_id.to_string());
    let document = crate::agent_bundle::AgentDocument::from_config(&entry.db_id.to_string(), cfg);
    std::fs::write(
        folder.join("agent.yaml"),
        serde_yaml::to_string(&document).unwrap(),
    )
    .unwrap();
}

/// Create `alpha` on an empty runner with every scoped setting a legacy root
/// definition could carry: scalar overrides at creation, then the larger
/// structures through its managed folder and an explicit reload. Its memory
/// bank attachment uses the explicit grant path.
async fn create_alpha(built: &BuiltServer, prompt_path: &Path) -> AgentDbConfig {
    let path = prompt_path.display().to_string();
    create(
        built,
        "alpha",
        &[
            ("model", "m1"),
            ("autonomous", "true"),
            ("tools", "shell,read_file"),
            ("max_spawn_depth", "20"),
            ("tool_profile", "deep"),
            ("system_prompt", "inline v1"),
            ("system_prompt_files", &path),
        ],
    )
    .await;
    let (_, mut cfg) = read_agent(built, "alpha").await;
    cfg.capabilities =
        serde_json::from_value(serde_json::json!({"network": {"endpoints": []}})).unwrap();
    cfg.grants =
        serde_json::from_value(serde_json::json!({"shell": {"shell": {"allow": []}}})).unwrap();
    cfg.workers = vec![crate::agent_db::WorkerDbConfig {
        name: "researcher".into(),
        ..Default::default()
    }];
    write_document(built, "alpha", &cfg).await;
    assert!(
        built
            .server
            .reload_agent_folder("alpha")
            .await
            .unwrap()
            .changed
    );
    let (bank, _) = built
        .registry
        .create_new_memory_bank("notes", &Default::default())
        .await
        .unwrap();
    let (entry, db) = open_agent(built, "alpha").await;
    built
        .registry
        .grant_on_memory_bank(
            &bank.id(),
            &entry.pubkey,
            "memory:notes:alpha",
            crate::agent_db::BankPermission::Write,
        )
        .await
        .unwrap();
    db.attach_memory_bank(crate::agent_db::MemoryBankRef {
        name: "notes".into(),
        db_id: bank.id().to_string(),
        permission: crate::agent_db::BankPermission::Write,
    })
    .await
    .unwrap();
    read_agent(built, "alpha").await.1
}

#[tokio::test]
async fn hosted_agents_survive_restart_without_root_definitions() {
    let disk = Disk::new();
    std::fs::write(&disk.prompt_path, "file v1").unwrap();
    let runner = "default_agents: [alpha]\nagent_groups:\n  pair: [alpha, beta, ghost]\n";

    // ── Boot 1: a fresh runner hosts nothing and fabricates nothing. ────
    let (built, service) = boot(&disk, disk.config(runner), true).await;
    assert!(built.server.agents().is_empty());
    assert!(built.server.agents().default_agent().is_none());
    assert!(built.server.agent_index().list().is_empty());
    let (_, empty_session) = built.registry.create_session(Some("empty")).await.unwrap();
    assert!(
        built
            .server
            .auto_attach_default_agent(&empty_session.root_id().to_string())
            .await
            .is_empty()
    );
    assert!(
        built
            .registry
            .resolve_agent(
                &empty_session.root_id().to_string(),
                None,
                built.server.agent_index()
            )
            .await
            .is_none()
    );
    drop(empty_session);
    let hosted = command(&built, Command::AgentHosted).await.unwrap();
    assert!(!hosted.contains("alpha"), "{hosted}");

    // The first agent is created by administration, with no LLM turn.
    let seeded = create_alpha(&built, &disk.prompt_path).await;
    let (identity, _) = read_agent(&built, "alpha").await;
    assert_eq!(seeded.model.as_deref(), Some("m1"));
    assert!(seeded.autonomous);
    assert_eq!(
        seeded.tools,
        Some(vec!["shell".to_string(), "read_file".to_string()])
    );
    assert_eq!(seeded.max_spawn_depth, Some(20));
    assert_eq!(seeded.tool_profile.as_deref(), Some("deep"));
    assert_eq!(
        seeded
            .capabilities
            .network
            .as_ref()
            .map(|n| serde_json::to_value(n).unwrap()),
        Some(serde_json::json!({"endpoints": [], "allow_private": false}))
    );
    assert_eq!(seeded.workers.len(), 1);
    assert!(seeded.prompt_sources.is_some());
    assert!(seeded.system_prompt_ref.is_none());
    assert_eq!(prompt(&built, "alpha").await, "file v1\n\ninline v1");
    create(
        &built,
        "beta",
        &[("model", "b1"), ("system_prompt", "beta prompt")],
    )
    .await;
    let (_, beta) = read_agent(&built, "beta").await;
    let (entry, db) = open_agent(&built, "alpha").await;
    let bank_refs = db.list_memory_banks().await.unwrap();
    assert_eq!(bank_refs.len(), 1);
    let (session_id, session_db) = built
        .registry
        .create_session(Some("seed-test"))
        .await
        .unwrap();
    built
        .registry
        .attach_agent_to_session(&session_id.0, &entry)
        .await
        .unwrap();
    crate::session::update_meta_on_db(&session_db, |meta| {
        meta.model = Some("session-model".into());
    })
    .await
    .unwrap();
    let session_meta =
        serde_json::to_value(crate::session::read_meta_from_db(&session_db).await).unwrap();
    drop(session_db);
    drop(db);

    // Deliberate edits: a default (false), an empty list, a value equal to the
    // runtime default (10), a value equal to the yaml value, and an empty
    // prompt.
    set(&built, "autonomous", "false").await.unwrap();
    set(&built, "tools", "").await.unwrap();
    set(&built, "max_spawn_depth", "10").await.unwrap();
    set(&built, "model", "m1").await.unwrap();
    set(&built, "system_prompt", "").await.unwrap();
    // A rejected write leaves the stored config exactly as it was.
    let (_, before_reject) = read_agent(&built, "alpha").await;
    assert!(set(&built, "autonomous", "maybe").await.is_err());
    assert!(set(&built, "no_such_field", "1").await.is_err());
    let (_, edited) = read_agent(&built, "alpha").await;
    assert_eq!(edited, before_reject);
    assert!(!edited.autonomous);
    assert_eq!(edited.tools, Some(vec![]));
    assert_eq!(edited.max_spawn_depth, Some(10));
    assert_eq!(edited.system_prompt, "");

    // Explicit reload of the unchanged projection is a no-op, not global YAML apply.
    let report = command(&built, Command::AgentReload(None)).await.unwrap();
    assert!(report.contains("already matches accepted"), "{report}");
    assert_eq!(read_agent(&built, "alpha").await.1, edited);
    let (entry, _) = open_agent(&built, "alpha").await;
    let managed = built
        .server
        .agent_folders()
        .unwrap()
        .folder("alpha", &entry.db_id.to_string());
    std::fs::write(managed.join("agent.yaml"), "invalid pending draft: [").unwrap();
    let pending_prompt = managed.join(&edited.system_prompt_files[0]);
    std::fs::write(&pending_prompt, "PENDING LOCAL FILE").unwrap();
    stop(built, service).await;

    // ── Boot 2a: a legacy root `agents:` block refuses with guidance. ──
    // Even a definition that differs from the hosted agent, or names a new
    // one, must neither apply, seed nor be silently ignored.
    std::fs::write(&disk.prompt_path, "file v2").unwrap();
    let legacy = disk.config(&format!(
        "{runner}agents:\n  - name: alpha\n    model: m2\n  - name: gamma\n    model: g1\n"
    ));
    let (refused, service) = try_boot(&disk, legacy, false).await;
    let error = format!(
        "{:#}",
        refused.err().expect("legacy agents block must refuse")
    );
    assert!(
        error.contains("root config defines agents (alpha, gamma)"),
        "{error}"
    );
    assert!(error.contains("/agent new NAME"), "{error}");
    assert!(error.contains("/memory grant BANK NAME write"), "{error}");
    drop(service.shutdown);
    service.task.await.unwrap().unwrap();

    // ── Boot 2b: restart with only runner settings. ──────────────────────
    let config = disk.config(runner);
    let (built, service) = boot(&disk, config, false).await;
    let (identity2, after) = read_agent(&built, "alpha").await;
    assert_eq!(identity2, identity, "identity and key unchanged");
    assert_eq!(after, edited, "existing agent kept its DB config");
    assert_eq!(prompt(&built, "alpha").await, "file v1");
    let hydrated = built.server.agents().get("alpha").unwrap();
    assert_eq!(hydrated.allowed_tools, Some(vec![]));
    assert_eq!(hydrated.capabilities, edited.capabilities);
    assert_eq!(hydrated.grants, edited.grants);
    assert!(hydrated.workers.contains_key("researcher"));
    assert!(!hydrated.workers.contains_key("writer"));
    let (_, db) = open_agent(&built, "alpha").await;
    assert_eq!(db.list_memory_banks().await.unwrap(), bank_refs);
    drop(db);
    let (_, session_db) = built.registry.open_session(&session_id.0).await.unwrap();
    assert_eq!(
        serde_json::to_value(crate::session::read_meta_from_db(&session_db).await).unwrap(),
        session_meta
    );
    drop(session_db);
    assert_eq!(read_agent(&built, "beta").await.1, beta);
    assert_eq!(beta.model.as_deref(), Some("b1"));
    assert!(beta.prompt_sources.is_some());
    assert_eq!(prompt(&built, "beta").await, "beta prompt");
    assert!(built.server.agent_index().find_by_name("gamma").is_none());
    // Runner defaults and groups resolve against hosted Agent DBs; the
    // registry's fallback is the first configured default.
    assert_eq!(built.server.agents().default_agent().unwrap().name, "alpha");
    let (_, fresh) = built.registry.create_session(Some("fresh")).await.unwrap();
    let fresh = fresh.root_id().to_string();
    assert_eq!(
        built.server.auto_attach_default_agent(&fresh).await,
        ["alpha"]
    );
    let (_, grouped) = built.registry.create_session(Some("group")).await.unwrap();
    let grouped = grouped.root_id().to_string();
    assert_eq!(
        built
            .server
            .auto_attach_agents(&grouped, Some("pair"))
            .await,
        ["alpha", "beta"]
    );

    assert_eq!(
        std::fs::read_to_string(managed.join("agent.yaml")).unwrap(),
        "invalid pending draft: ["
    );
    assert_eq!(
        std::fs::read_to_string(&pending_prompt).unwrap(),
        "PENDING LOCAL FILE"
    );
    assert!(
        command(&built, Command::AgentReload(Some("alpha".into())))
            .await
            .is_err()
    );
    command(&built, Command::AgentProject("alpha".into()))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&pending_prompt).unwrap(), "file v1");
    // Full folder replacement through the explicit reload.
    let mut expected = edited.clone();
    expected.model = Some("m2".into());
    write_document(&built, "alpha", &expected).await;
    let outcome = built.server.reload_agent_folder("alpha").await.unwrap();
    assert!(outcome.changed);
    assert!(outcome.projection_error.is_none());
    assert_eq!(read_agent(&built, "alpha").await.1, expected);
    expected.workers = vec![crate::agent_db::WorkerDbConfig {
        name: "writer".into(),
        ..Default::default()
    }];
    write_document(&built, "alpha", &expected).await;
    built.server.reload_agent_folder("alpha").await.unwrap();
    let (_, picked) = read_agent(&built, "alpha").await;
    assert_eq!(
        picked, expected,
        "full scoped replacement preserved unrelated state"
    );
    stop(built, service).await;

    // ── Boot 3: repeated restart keeps identities, settings and references.
    for _ in 0..2 {
        let (built, service) = boot(&disk, disk.config(runner), false).await;
        let (identity3, alpha3) = read_agent(&built, "alpha").await;
        assert_eq!(identity3, identity);
        assert_eq!(alpha3, picked);
        assert_eq!(read_agent(&built, "beta").await.1, beta);
        let runtime = built.server.agents().get("alpha").unwrap();
        assert_eq!(runtime.default_model.as_deref(), Some("m2"));
        assert!(runtime.workers.contains_key("writer"));
        let (_, db) = open_agent(&built, "alpha").await;
        assert_eq!(db.list_memory_banks().await.unwrap(), bank_refs);
        assert_eq!(
            db.list_session_refs().await.unwrap()[0].session_db_id,
            session_id.0
        );
        drop(db);
        stop(built, service).await;
    }
}

#[tokio::test]
async fn legacy_saved_prompt_migration_preserves_identity_and_hash_across_restart() {
    let disk = Disk::new();
    std::fs::write(&disk.prompt_path, "file v1").unwrap();
    let mut older = disk.config("");
    older.agent_config_dir = None; // A real older-format writer with a saved snapshot.
    let (built, service) = boot(&disk, older, true).await;
    let path = disk.prompt_path.display().to_string();
    create(
        &built,
        "alpha",
        &[
            ("system_prompt", "inline v1"),
            ("system_prompt_files", &path),
        ],
    )
    .await;
    let (_, mut snapshot) = read_agent(&built, "alpha").await;
    built
        .server
        .refresh_prompt_ref(&mut snapshot)
        .await
        .unwrap();
    let (_, db) = open_agent(&built, "alpha").await;
    db.write_config(&snapshot).await.unwrap();
    drop(db);
    let (identity, mut legacy) = read_agent(&built, "alpha").await;
    assert!(legacy.prompt_sources.is_none() && legacy.system_prompt_ref.is_some());
    legacy.applied_config_hash = Some("hash-from-older-chaz".into());
    legacy.autonomous = false;
    let (_, db) = open_agent(&built, "alpha").await;
    db.write_config(&legacy).await.unwrap();
    let bank_refs = db.list_memory_banks().await.unwrap();
    drop(db);
    stop(built, service).await;

    std::fs::write(&disk.prompt_path, "EXTERNAL EDIT NOT APPROVED").unwrap();
    let config = disk.config("");
    let (built, service) = boot(&disk, config.clone(), false).await;
    let (after_identity, migrated) = read_agent(&built, "alpha").await;
    assert_eq!(after_identity, identity);
    assert_eq!(migrated.model, legacy.model);
    assert_eq!(migrated.capabilities, legacy.capabilities);
    assert_eq!(migrated.grants, legacy.grants);
    assert_eq!(migrated.workers, legacy.workers);
    assert_eq!(migrated.autonomous, legacy.autonomous);
    assert_eq!(migrated.applied_config_hash, legacy.applied_config_hash);
    assert!(migrated.prompt_sources.is_some() && migrated.system_prompt_ref.is_none());
    assert_eq!(prompt(&built, "alpha").await, "file v1\n\ninline v1");
    assert_eq!(
        std::fs::read_to_string(&disk.prompt_path).unwrap(),
        "EXTERNAL EDIT NOT APPROVED"
    );
    let (_, db) = open_agent(&built, "alpha").await;
    assert_eq!(db.list_memory_banks().await.unwrap(), bank_refs);
    drop(db);
    stop(built, service).await;
    let (built, service) = boot(&disk, config, false).await;
    assert_eq!(read_agent(&built, "alpha").await, (identity, migrated));
    assert_eq!(prompt(&built, "alpha").await, "file v1\n\ninline v1");
    stop(built, service).await;
}

#[tokio::test]
async fn empty_file_prompt_stays_pinned_until_explicit_refresh() {
    let disk = Disk::new();
    std::fs::write(&disk.prompt_path, "").unwrap();
    let (built, service) = boot(&disk, disk.config(""), true).await;
    let path = disk.prompt_path.display().to_string();
    create(&built, "alpha", &[("system_prompt_files", &path)]).await;
    assert!(read_agent(&built, "alpha").await.1.prompt_sources.is_some());
    std::fs::write(&disk.prompt_path, "file edit").unwrap();
    assert_eq!(prompt(&built, "alpha").await, "");

    // The command re-reads even unchanged paths, and can pin empty content too.
    let path = disk.prompt_path.to_str().unwrap();
    set(&built, "system_prompt_files", path).await.unwrap();
    assert_eq!(prompt(&built, "alpha").await, "file edit");
    std::fs::write(&disk.prompt_path, "").unwrap();
    set(&built, "system_prompt_files", path).await.unwrap();
    let (_, empty) = read_agent(&built, "alpha").await;
    assert!(empty.prompt_sources.is_some());
    std::fs::write(&disk.prompt_path, "another file edit").unwrap();
    assert_eq!(prompt(&built, "alpha").await, "");
    command(&built, Command::AgentReload(None)).await.unwrap();
    assert_eq!(read_agent(&built, "alpha").await.1, empty);
    stop(built, service).await;

    let (built, service) = boot(&disk, disk.config(""), false).await;
    assert_eq!(read_agent(&built, "alpha").await.1, empty);
    assert_eq!(prompt(&built, "alpha").await, "");
    // Inline scalar edits do not import pending external or managed file edits.
    set(&built, "system_prompt", "inline imported")
        .await
        .unwrap();
    let (_, merged) = read_agent(&built, "alpha").await;
    assert_eq!(merged.system_prompt_files, empty.system_prompt_files);
    assert_eq!(merged.system_prompt, "inline imported");
    assert_eq!(prompt(&built, "alpha").await, "inline imported");
    stop(built, service).await;
}

#[tokio::test]
async fn unreadable_config_refuses_reload_and_set_without_writes() {
    use eidetica::store::DocStore;

    let disk = Disk::new();
    let (built, service) = boot(&disk, disk.config(""), true).await;
    create(&built, "alpha", &[("model", "seeded")]).await;
    let (entry, db) = open_agent(&built, "alpha").await;
    let healthy = db.read_config().await.unwrap();
    let runtime_model = built.server.agents().get("alpha").unwrap().default_model;
    for bad_json in [false, true] {
        let txn = db.database().new_transaction().await.unwrap();
        let store = txn.get_store::<DocStore>("config").await.unwrap();
        if bad_json {
            store.set_string("value", "{").await.unwrap();
        } else {
            store.set("value", 42i64).await.unwrap();
        }
        txn.commit().await.unwrap();
        assert!(
            db.read_config().await.is_err(),
            "fault must reach the reader"
        );
        let before = db.database().snapshot().await.unwrap();
        assert!(built.server.agent_diff("alpha").await.is_err());
        assert!(built.server.reload_agent_folder("alpha").await.is_err());
        assert!(command(&built, Command::AgentReload(None)).await.is_err());
        assert!(set(&built, "model", "replacement").await.is_err());
        assert_eq!(db.database().snapshot().await.unwrap(), before);
        assert_eq!(
            built.server.agents().get("alpha").unwrap().default_model,
            runtime_model
        );
    }
    db.write_config(&healthy).await.unwrap();
    // Selecting a non-writable signer must not grant config-write authority.
    // The service may refuse at open or commit; neither may change the agent.
    let reader = built
        .registry
        .new_ephemeral_key("config-reader")
        .await
        .unwrap();
    built
        .registry
        .grant_on_agent_db(
            &entry.db_id,
            &reader,
            "config-reader",
            eidetica::auth::types::Permission::Read,
        )
        .await
        .unwrap();
    built
        .registry
        .user_lock()
        .await
        .map_key(
            &reader,
            &entry.db_id,
            eidetica::auth::SigKey::global(&reader),
        )
        .await
        .unwrap();
    built
        .server
        .agent_index()
        .register(crate::hosted_index::DbEntry {
            pubkey: reader,
            ..entry
        });
    let before = db.database().snapshot().await.unwrap();
    let error = set(&built, "model", "replacement").await.unwrap_err();
    assert!(error.contains("Permission denied"), "{error}");
    assert!(built.server.reload_agent_folder("alpha").await.is_err());
    assert_eq!(db.database().snapshot().await.unwrap(), before);
    assert_eq!(db.read_config().await.unwrap(), healthy);
    assert_eq!(
        built.server.agents().get("alpha").unwrap().default_model,
        runtime_model
    );
    drop(db);
    stop(built, service).await;
}
