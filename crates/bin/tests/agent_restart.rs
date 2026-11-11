//! Exercise Chaz client/executor restarts against a SQLite-backed service.
use eidetica::{Instance, NewAccount, backend::database::Sqlite};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_model_survives_unchanged_config_process_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let connection = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("eidetica.db").display()
    );
    let (owner, user) = Instance::create_backend(
        Box::new(Sqlite::connect(&connection).await.unwrap()),
        NewAccount::passwordless("fixture"),
    )
    .await
    .unwrap();
    drop(user);
    let socket = dir.path().join("eidetica.sock");
    let service = eidetica::service::ServiceServer::bind(owner, &socket)
        .await
        .unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(());
    let service_task = tokio::spawn(service.run(receiver));
    let connection = format!("unix://{}", socket.display());
    let model = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices":[{
            "message":{"role":"assistant","content":"fixture reply"}, "finish_reason":"stop"
        }]})))
        .mount(&model)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list", "data":[]})))
        .mount(&model)
        .await;
    let prompt_path = dir.path().join("prompt.md");
    std::fs::write(&prompt_path, "fixture prompt").unwrap();
    let config = json!({
        "state_dir":dir.path().join("state"), "execution":"executor",
        "eidetica":{"connection":connection, "login":{"username":"fixture", "passwordless":true}},
        "backends":[{"name":"stub", "type":"openaicompatible", "api_base":format!("{}/v1",model.uri()), "api_key":"fixture", "models":[{"name":"baseline"},{"name":"live"}]}],
        "default_agents":["chaz"]
    });
    let config_path = dir.path().join("config.yaml");
    std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
    let client_path = dir.path().join("client.yaml");
    let mut client_config = config.clone();
    client_config["execution"] = json!("client");
    std::fs::write(&client_path, serde_yaml::to_string(&client_config).unwrap()).unwrap();
    let binary = std::env::var_os("NEXTEST_BIN_EXE_chaz")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_chaz").into());
    let run = |args: &[&str]| {
        let mut child = tokio::process::Command::new(&binary);
        child
            .env_clear()
            .env(
                "LD_LIBRARY_PATH",
                std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default(),
            )
            .envs(std::env::var_os("SSL_CERT_FILE").map(|v| ("SSL_CERT_FILE", v)))
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path())
            .arg("--config")
            .arg(if args[0] == "cmd" {
                &client_path
            } else {
                &config_path
            })
            .args(args)
            .kill_on_drop(true);
        async move {
            let output = tokio::time::timeout(std::time::Duration::from_secs(30), child.output())
                .await
                .expect("child deadline")
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            println!("{stdout}");
            stdout
        }
    };
    // Root config defines no agents: create the hosted Agent DB first.
    let created = run(&[
        "cmd",
        &format!(
            "/agent new chaz system_prompt_files={}",
            prompt_path.display()
        ),
    ])
    .await;
    assert!(created.contains("chaz"), "{created}");
    run(&["--print", "first session"]).await;
    let confirmation = run(&["cmd", "/agent set chaz model live"]).await;
    assert!(confirmation.contains("Set model=live on agent 'chaz'"));
    let displayed = run(&["cmd", "/model"]).await;
    for _ in 0..2 {
        run(&["--print", "new session"]).await;
    }
    // Editing the prompt file after creation must not clear the accepted
    // live model on restart or import an unaccepted prompt.
    std::fs::write(&prompt_path, "updated fixture prompt").unwrap();
    run(&["--print", "session after prompt file edit"]).await;
    let after_prompt_edit = run(&["cmd", "/model"]).await;
    let requests = model.received_requests().await.unwrap();
    let completions: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .collect();
    let models: Vec<_> = completions
        .iter()
        .map(|r| {
            r.body_json::<serde_json::Value>().unwrap()["model"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    drop(shutdown);
    tokio::time::timeout(std::time::Duration::from_secs(10), service_task)
        .await
        .expect("service shutdown deadline")
        .unwrap()
        .unwrap();
    // Check transport before presentation so a stale display cannot hide
    // whether the restarted executor actually lost the live model.
    println!("outgoing models: {models:?}");
    assert_eq!(models, ["baseline", "live", "live", "live"]);
    for request in completions {
        let body = request.body_json::<serde_json::Value>().unwrap();
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("fixture prompt"), "{body}");
        assert!(!system.contains("updated fixture prompt"), "{body}");
    }
    assert!(
        displayed.contains("Current Model: live (agent default)"),
        "{displayed}"
    );
    assert!(
        after_prompt_edit.contains("Current Model: live (agent default)"),
        "{after_prompt_edit}"
    );
}
