//! YAML seeds agents; the Agent DB is authoritative afterwards. These tests
//! drive the real daemon build (`build` + its deferred startup) over an
//! on-disk SQLite store, shut it down, and rebuild from the same file with an
//! edited yaml, so they cover a complete service + storage restart. No model
//! backend is configured, so nothing touches the network.

use super::build::{BuildOptions, BuiltServer, McpReadiness, build};
use crate::agent_db::AgentDbConfig;
use crate::commands::{Command, CommandContext, CommandOutcome, dispatch};
use crate::config::Config;
use eidetica::Instance;
use std::path::{Path, PathBuf};

struct Disk {
    _dir: tempfile::TempDir,
    url: String,
    config_path: PathBuf,
    prompt_path: PathBuf,
}

impl Disk {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("chaz.db").display());
        let config_path = dir.path().join("config.yaml");
        let prompt_path = dir.path().join("PROMPT.md");
        Self {
            _dir: dir,
            url,
            config_path,
            prompt_path,
        }
    }

    /// Write `agents_yaml` (an indented `agents:` list body) into the config
    /// file and parse it the way the daemon does.
    fn config(&self, agents_yaml: &str) -> Config {
        let body = format!(
            "execution: executor\neidetica:\n  connection: \"{}\"\n  login: {{ username: chaz, passwordless: true }}\nagents:\n{agents_yaml}",
            self.url
        );
        std::fs::write(&self.config_path, &body).unwrap();
        serde_yaml::from_str(&body).unwrap()
    }
}

async fn boot(disk: &Disk, mut config: Config, first: bool) -> BuiltServer {
    let (instance, user) = if first {
        Instance::create_backend(
            Box::new(
                eidetica::backend::database::Sqlite::connect(&disk.url)
                    .await
                    .unwrap(),
            ),
            eidetica::NewAccount::passwordless("chaz"),
        )
        .await
        .unwrap()
    } else {
        // The previous generation's handles are dropped, but background
        // tasks may still be unwinding; wait for the store to be released.
        let instance = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                match Instance::connect(&disk.url).await {
                    Ok(instance) => break instance,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                }
            }
        })
        .await
        .expect("previous generation never released the store");
        let user = instance.login_account("chaz", None).await.unwrap();
        (instance, user)
    };
    let built = build(
        &mut config,
        instance,
        user,
        BuildOptions {
            config_path: disk.config_path.clone(),
            run_routine_engine: false,
            bootstrap_agents_from_config: true,
            run_agent_loop: false,
            extra_auto_approved_tools: Vec::new(),
            mcp_readiness: McpReadiness::Deferred,
            targeted_session: None,
        },
    )
    .await
    .unwrap();
    // Seed completion runs in the deferred startup task, before the
    // processing gate opens.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !built.server.is_startup_ready() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deferred startup never opened the processing gate");
    built
}

/// Shut the service down and release every handle on the store, so the next
/// boot reopens the file the way a restarted process would.
async fn stop(built: BuiltServer) {
    built.shutdown().await;
    drop(built);
}

async fn read_agent(built: &BuiltServer, name: &str) -> (String, AgentDbConfig) {
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
    (
        format!("{}/{}", entry.db_id, entry.pubkey.to_prefixed_string()),
        db.read_config().await.unwrap(),
    )
}

async fn command(built: &BuiltServer, cmd: Command) -> Result<String, String> {
    let (session_db_id, session_db) = built
        .registry
        .create_session(Some("seed-test"))
        .await
        .unwrap();
    let backend = crate::backends::BackendManager::new(&None, built.secret_store.clone());
    let ctx = CommandContext {
        server: &built.server,
        secrets: &built.secret_store,
        backend: &backend,
        session_db_id: &session_db_id.0,
        session_db: &session_db,
        current_agent: "alpha",
        session_name: None,
    };
    match dispatch(cmd, &ctx).await {
        CommandOutcome::Text(text) => Ok(text),
        CommandOutcome::Error(error) => Err(error),
        _ => Err("unexpected outcome".into()),
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

async fn prompt(built: &BuiltServer, name: &str) -> String {
    let agent = built.server.agents().get(name).unwrap();
    built
        .server
        .hydrate_agent_from_db(agent)
        .await
        .system_prompt
}

fn alpha_v1(prompt_path: &Path) -> String {
    format!(
        "  - name: alpha\n    model: m1\n    autonomous: true\n    tools: [shell, read_file]\n    max_spawn_depth: 20\n    tool_profile: deep\n    system_prompt: \"inline v1\"\n    system_prompt_files: [\"{}\"]\n    capabilities:\n      network:\n        endpoints: []\n    workers:\n      - name: researcher\n",
        prompt_path.display()
    )
}

#[tokio::test]
async fn yaml_seeds_new_agents_and_db_config_survives_full_restart() {
    let disk = Disk::new();
    std::fs::write(&disk.prompt_path, "file v1").unwrap();

    // ── Boot 1: a new agent is created from yaml. ───────────────────────
    let built = boot(&disk, disk.config(&alpha_v1(&disk.prompt_path)), true).await;
    let (identity, seeded) = read_agent(&built, "alpha").await;
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
    assert!(seeded.system_prompt_ref.is_some());
    assert_eq!(prompt(&built, "alpha").await, "file v1\n\ninline v1");

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

    // `/agent reload` only reports.
    let report = command(&built, Command::AgentReload(None)).await.unwrap();
    assert!(report.contains("Nothing was changed"), "{report}");
    assert!(report.contains("alpha: DB differs from yaml"), "{report}");
    assert_eq!(read_agent(&built, "alpha").await.1, edited);
    stop(built).await;

    // ── Boot 2: yaml changed in every way an old reconcile reacted to. ──
    // Values change, keys disappear (including the restrictive capability
    // ceiling), the worker list and the prompt file contents change, and a
    // new agent is declared.
    std::fs::write(&disk.prompt_path, "file v2").unwrap();
    let config = disk.config(&format!(
        "  - name: alpha\n    model: m2\n    autonomous: true\n    max_spawn_depth: 30\n    system_prompt: \"inline v2\"\n    system_prompt_files: [\"{}\"]\n    workers:\n      - name: writer\n  - name: beta\n    model: b1\n    system_prompt: \"beta prompt\"\n",
        disk.prompt_path.display()
    ));
    let built = boot(&disk, config, false).await;
    let (identity2, after) = read_agent(&built, "alpha").await;
    assert_eq!(identity2, identity, "identity and key unchanged");
    assert_eq!(after, edited, "existing agent kept its DB config");
    assert_eq!(prompt(&built, "alpha").await, "file v1");
    let (_, beta) = read_agent(&built, "beta").await;
    assert_eq!(beta.model.as_deref(), Some("b1"));
    assert!(beta.system_prompt_ref.is_some());
    assert_eq!(prompt(&built, "beta").await, "beta prompt");

    // An explicit pick is the deliberate way to take a yaml value; it writes
    // only that field.
    let outcome = built
        .server
        .merge_agent_from_yaml(
            "alpha",
            &crate::agent_diff::AgentMergeMode::Pick(vec![crate::agent_diff::AgentField::Model]),
        )
        .await
        .unwrap();
    assert!(outcome.changed);
    let (_, picked) = read_agent(&built, "alpha").await;
    let mut expected = edited.clone();
    expected.model = Some("m2".into());
    assert_eq!(picked, expected);
    stop(built).await;

    // ── Boot 3: unchanged yaml is idempotent; a removed entry is kept. ──
    let config = disk.config("  - name: beta\n    model: changed\n");
    let built = boot(&disk, config, false).await;
    let (identity3, alpha3) = read_agent(&built, "alpha").await;
    assert_eq!(identity3, identity, "removing the yaml entry keeps the DB");
    assert_eq!(alpha3, picked);
    assert_eq!(read_agent(&built, "beta").await.1, beta);
    stop(built).await;
}

#[tokio::test]
async fn legacy_applied_config_hash_never_triggers_reapply() {
    let disk = Disk::new();
    std::fs::write(&disk.prompt_path, "file v1").unwrap();
    let built = boot(&disk, disk.config(&alpha_v1(&disk.prompt_path)), true).await;

    // Simulate a database last written by the hash-gated reconcile, then
    // edited by the user afterwards.
    let entry = built.server.agent_index().find_by_name("alpha").unwrap();
    let db = built
        .registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    let mut legacy = db.read_config().await.unwrap();
    legacy.applied_config_hash = Some("hash-from-older-chaz".into());
    legacy.autonomous = false;
    db.write_config(&legacy).await.unwrap();
    drop(db);
    stop(built).await;

    let config = disk.config("  - name: alpha\n    model: m9\n    autonomous: true\n");
    let built = boot(&disk, config, false).await;
    assert_eq!(read_agent(&built, "alpha").await.1, legacy);
    stop(built).await;
}
