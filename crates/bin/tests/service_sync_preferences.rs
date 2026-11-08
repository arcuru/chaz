//! Preference-only commands use the signed service path, not owner sync APIs.
use std::sync::Arc;
use std::time::Duration;

use chaz_core::agent::AgentRegistry;
use chaz_core::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
use chaz_core::hosted_index::DbEntry;
use chaz_core::memory_bank_db::{MemoryBankMeta, create_memory_bank};
use chaz_core::session::SessionRegistry;
use chaz_core::skill_bank_db::{SkillBankMeta, create_skill_bank};
use eidetica::account::types::SyncSettings;
use eidetica::backend::database::InMemory;
use eidetica::crdt::doc::path;
use eidetica::service::ServiceServer;
use eidetica::store::DocStore;
use eidetica::sync::DatabaseTicket;
use eidetica::{Database, Instance, NewAccount};
use tokio::process::Command;
use tokio::sync::watch;

// Read the owner's derived state independently of the account preference. Never
// call sync_account here: only the daemon's signed-write callback may reconcile.
async fn combined_settings(owner: &Instance, db: &Database) -> Option<SyncSettings> {
    let store = owner
        .sync()
        .unwrap()
        .sync_tree()
        .get_store_viewer::<DocStore>("database_users")
        .await
        .unwrap();
    match store
        .get_path_as::<String>(path!(db.root_id().to_string(), "combined_settings"))
        .await
    {
        Ok(row) => Some(serde_json::from_str(&row).unwrap()),
        Err(error) if error.is_not_found() => None,
        Err(error) => panic!(
            "Failed to read combined settings for {}: {error}",
            db.root_id()
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_cli_unshare_commits_preferences_and_preserves_owner_gates() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("eidetica.sock");
    let (owner, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::passwordless("preferences"),
    )
    .await
    .unwrap();
    owner.enable_sync().await.unwrap();
    let (agent, pubkey) = create_agent_db(
        &mut user,
        "selected",
        &AgentDbConfig::default(),
        &AgentMeta {
            display_name: Some("selected".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (memory, _) = create_memory_bank(&mut user, "facts", &MemoryBankMeta::default())
        .await
        .unwrap();
    let (skills, _) = create_skill_bank(&mut user, "ops", &SkillBankMeta::default())
        .await
        .unwrap();
    let registry = SessionRegistry::new(
        owner.clone(),
        user,
        Arc::new(AgentRegistry::from_config(
            &chaz_core::config::Config::default(),
        )),
    )
    .await
    .unwrap();
    let (_, session) = registry.create_session(Some("fixture")).await.unwrap();
    let sid = session.root_id().to_string();
    registry
        .set_session_name(&sid, "fixture".into())
        .await
        .unwrap();
    registry
        .attach_agent_to_session(
            &sid,
            &DbEntry {
                db_id: agent.id(),
                display_name: "selected".into(),
                pubkey,
            },
        )
        .await
        .unwrap();
    let targets = [
        ("/memory unshare facts", memory.database()),
        ("/skills unshare ops", skills.database()),
        ("/agent unshare selected", agent.database()),
        ("/unshare", &session),
    ];
    for (_, db) in targets {
        registry.enable_sync_for(db.root_id()).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut ready = true;
            for (_, db) in targets {
                ready &= combined_settings(&owner, db)
                    .await
                    .is_some_and(|s| s.sync_enabled && s.sync_on_commit);
            }
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fixture must first reach enabled daemon state");

    let service = ServiceServer::bind(owner.clone(), &socket).await.unwrap();
    let (shutdown, receiver) = watch::channel(());
    let service_task = tokio::spawn(service.run(receiver));
    let config = dir.path().join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "state_dir: {}\nexecution: client\neidetica:\n  connection: unix://{}\n  login:\n    username: preferences\n    passwordless: true\nagents:\n  - name: selected\n    system_prompt: test\n    autonomous: false\nbackends: []\nmcp_servers: []\n",
            dir.path().join("state").display(),
            socket.display(),
        ),
    )
    .unwrap();
    let binary = std::env::var_os("NEXTEST_BIN_EXE_chaz")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_chaz").into());
    let run = async |command: &str| {
        tokio::time::timeout(
            Duration::from_secs(20),
            Command::new(&binary)
                .kill_on_drop(true)
                .env("XDG_CONFIG_HOME", dir.path().join("config"))
                .arg("--config")
                .arg(&config)
                .args(["cmd", command, "--session", "fixture"])
                .output(),
        )
        .await
        .unwrap_or_else(|_| panic!("{command} timed out"))
        .unwrap()
    };
    // The fixture session has not run an executor's session-start hook. Enable
    // its bank commands through the normal client surface before taking baselines.
    for command in ["/extensions add memory", "/extensions add skills"] {
        let output = run(command).await;
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let before_sync = owner.sync().unwrap().sync_tree().snapshot().await.unwrap();
    let before_targets = futures::future::join_all(targets.iter().map(|(_, db)| db.snapshot()))
        .await
        .into_iter()
        .collect::<eidetica::Result<Vec<_>>>()
        .unwrap();
    let ticket = DatabaseTicket::new(memory.id());
    for command in [
        "/share".to_string(),
        "/sync not-a-ticket".to_string(),
        "/agent share selected".to_string(),
        "/agent import not-a-ticket".to_string(),
        "/sharing requests".to_string(),
        "/sharing approve missing".to_string(),
        "/sharing reject missing".to_string(),
        "/sharing status".to_string(),
        "/memory share facts".to_string(),
        "/skills share ops".to_string(),
        "/memory import not-a-ticket".to_string(),
        "/skills import not-a-ticket".to_string(),
        format!("/memory attach {ticket}"),
        format!("/skills attach {ticket}"),
    ] {
        let output = run(&command).await;
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{command} unexpectedly succeeded");
        assert!(
            error.contains("Run it on the Eidetica owner"),
            "{command}: {error}"
        );
        assert!(error.contains("made no changes"), "{command}: {error}");
        for ((_, db), before) in targets.iter().zip(&before_targets) {
            assert_eq!(&db.snapshot().await.unwrap(), before, "{command}");
            assert!(registry.is_sync_enabled_for(db.root_id()).await.unwrap());
        }
        assert_eq!(
            owner.sync().unwrap().sync_tree().snapshot().await.unwrap(),
            before_sync,
            "{command} changed daemon sync state"
        );
    }

    for (command, db) in targets {
        let snapshot = db.snapshot().await.unwrap();
        for _ in 0..2 {
            let output = run(command).await;
            assert!(
                output.status.success(),
                "{command}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("Sync disabled"));
            // The owner reads persisted preferences, not the CLI's cached state.
            let user = registry.user_lock().await;
            let settings = user.database(db.root_id()).await.unwrap().sync_settings;
            assert!(
                !settings.sync_enabled,
                "{command} did not persist the preference"
            );
            assert!(
                settings.sync_on_commit,
                "unshare must preserve other settings"
            );
            drop(user);
            assert_eq!(db.snapshot().await.unwrap(), snapshot);
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while combined_settings(&owner, db).await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("daemon must remove combined settings after the last sharing preference");
    }

    drop(shutdown);
    service_task.await.unwrap().unwrap();
}
