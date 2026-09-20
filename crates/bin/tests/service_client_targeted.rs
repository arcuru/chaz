use std::process::Command;
use std::sync::Arc;

use chaz_core::agent::AgentRegistry;
use chaz_core::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
use chaz_core::hosted_index::DbEntry;
use chaz_core::session::SessionRegistry;
use eidetica::backend::database::InMemory;
use eidetica::service::ServiceServer;
use eidetica::{Instance, NewUser};
use tokio::sync::watch;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_catalog_commands_do_not_create_sessions_and_agents_stay_session_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("eidetica.sock");
    let (owner, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewUser::passwordless("targeted-cli"),
    )
    .await
    .unwrap();
    let (agent_db, pubkey) = create_agent_db(
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
    let registry = Arc::new(
        SessionRegistry::new(
            owner.clone(),
            user,
            Arc::new(AgentRegistry::from_config(
                &chaz_core::config::Config::default(),
            )),
        )
        .await
        .unwrap(),
    );
    let (_, selected) = registry.create_session(Some("fixture")).await.unwrap();
    let selected_id = selected.root_id().to_string();
    registry
        .set_session_name(&selected_id, "selected-session".into())
        .await
        .unwrap();
    registry
        .attach_agent_to_session(
            &selected_id,
            &DbEntry {
                db_id: agent_db.id(),
                display_name: "selected".into(),
                pubkey,
            },
        )
        .await
        .unwrap();
    for n in 0..24 {
        registry
            .create_session(Some(&format!("history-{n}")))
            .await
            .unwrap();
    }
    let initial_count = registry.list_sessions().await.unwrap().len();

    let service = ServiceServer::bind(owner, &socket).await.unwrap();
    let (shutdown, receiver) = watch::channel(());
    let service_task = tokio::spawn(service.run(receiver));

    let state_dir = dir.path().join("state");
    let config_path = dir.path().join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "state_dir: {}\nexecution: client\neidetica:\n  connection: unix://{}\n  login:\n    username: targeted-cli\n    passwordless: true\nagents:\n  - name: selected\n    system_prompt: test\n    autonomous: false\ndefault_agents: [selected]\n",
            state_dir.display(),
            socket.display()
        ),
    )
    .unwrap();

    let run = |args: &[&str]| {
        let started = std::time::Instant::now();
        Command::new(env!("CARGO_BIN_EXE_chaz"))
            .arg("--config")
            .arg(&config_path)
            .args(args)
            .output()
            .map(|output| (output, started.elapsed()))
            .unwrap()
    };
    for _ in 0..2 {
        let (output, elapsed) = run(&["cmd", "/sessions"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&selected_id));
        assert!(elapsed < std::time::Duration::from_secs(10));
    }
    let command_log = std::fs::read_dir(&state_dir)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("chaz-cmd"))
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .unwrap();
    assert!(command_log.contains("eidetica opened"));
    assert!(!command_log.contains("Server built; handing off to gateway"));
    let (output, elapsed) = run(&["cmd", "/agents", "--session", "selected-session"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("selected"));
    assert!(elapsed < std::time::Duration::from_secs(10));
    assert_eq!(registry.list_sessions().await.unwrap().len(), initial_count);

    let (output, elapsed) = run(&["cmd", "/agents", "--session", "created-session"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("selected"));
    assert!(elapsed < std::time::Duration::from_secs(10));
    assert_eq!(
        registry.list_sessions().await.unwrap().len(),
        initial_count + 1
    );

    let (output, elapsed) = run(&["cmd", "/sessions"]);
    assert!(output.status.success());
    assert!(elapsed < std::time::Duration::from_secs(10));
    assert_eq!(
        registry.list_sessions().await.unwrap().len(),
        initial_count + 1
    );

    drop(shutdown);
    service_task.await.unwrap().unwrap();
}
