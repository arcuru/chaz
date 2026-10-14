//! Frontend bootstrap against a real, disposable authenticated service.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use chaz_core::{
    agent::AgentRegistry,
    agent_db::{AgentDbConfig, AgentMeta, create_agent_db},
    backends::BackendManager,
    config::{Config, ExecutionRole},
    instance,
    server::{self, BuildOptions, McpReadiness},
    session::SessionRegistry,
};
use eidetica::{Instance, NewUser, backend::database::InMemory, service::ServiceServer};
use tokio::sync::watch;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_readiness_skips_delayed_executor_credentials_and_schedule_bootstrap() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("owner.sock");
    let proxy_socket = dir.path().join("proxy.sock");
    let (owner, mut user) =
        Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("startup"))
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
    let registry = SessionRegistry::new(
        owner.clone(),
        user,
        Arc::new(AgentRegistry::from_config(&Config::default())),
    )
    .await
    .unwrap();
    let (_, db) = registry.create_session(Some("fixture")).await.unwrap();
    let sid = db.root_id().to_string();
    registry
        .set_session_name(&sid, "selected-session".into())
        .await
        .unwrap();
    registry
        .attach_agent_to_session(
            &sid,
            &chaz_core::hosted_index::DbEntry {
                db_id: agent_db.id(),
                display_name: "selected".into(),
                pubkey,
            },
        )
        .await
        .unwrap();
    let service = ServiceServer::bind(owner, &socket).await.unwrap();
    let (shutdown, receiver) = watch::channel(());
    let service_task = tokio::spawn(service.run(receiver));

    // Delay the actual signed executor credential writes, not a replacement
    // storage result. The owner's authentication and persistence still run.
    let listener = tokio::net::UnixListener::bind(&proxy_socket).unwrap();
    let delayed = Arc::new(AtomicUsize::new(0));
    let proxy = tokio::spawn({
        let delayed = delayed.clone();
        async move {
            use eidetica::service::protocol::{
                DatabaseOp, ServiceRequest, read_frame, write_frame,
            };
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, _) = accepted.unwrap();
                        let service = tokio::net::UnixStream::connect(&socket).await.unwrap();
                        let delayed = delayed.clone();
                        connections.spawn(async move {
                            let (mut from_client, mut to_client) = client.into_split();
                            let (mut from_service, mut to_service) = service.into_split();
                            let requests = async {
                                while let Some(frame) = read_frame::<_, serde_json::Value>(&mut from_client).await? {
                                    if let Ok(ServiceRequest::AuthenticatedDb(request)) = serde_json::from_value(frame.clone())
                                        && let DatabaseOp::SubmitSignedEntry { entry } = request.op
                                        && let Ok(data) = entry.data("credentials")
                                        && (String::from_utf8_lossy(data).contains("secret:web_search")
                                            || String::from_utf8_lossy(data).contains("embedding:openai/"))
                                    {
                                        delayed.fetch_add(1, Ordering::SeqCst);
                                        tokio::time::sleep(Duration::from_millis(500)).await;
                                    }
                                    write_frame(&mut to_service, &frame).await?;
                                }
                                Ok::<_, eidetica::Error>(())
                            };
                            let responses = async {
                                tokio::io::copy(&mut from_service, &mut to_client).await?;
                                Ok::<_, eidetica::Error>(())
                            };
                            tokio::select! { result = requests => result, result = responses => result }
                        });
                    },
                    Some(result) = connections.join_next() => { result.unwrap().unwrap(); },
                }
            }
        }
    });

    let config: Config = serde_yaml::from_str(&format!(
        r#"
execution: client
eidetica:
  connection: unix://{}
  login: {{ username: startup, passwordless: true }}
agents:
  - name: selected
    model: display-model
    autonomous: false
default_agents: [selected]
backends:
  - name: catalog
    type: openaicompatible
    api_key: catalog-key
    api_base: http://127.0.0.1:1/v1
    models: [{{ name: fallback-model }}, {{ name: display-model, context_window: 8192 }}]
agent_state_allowlist:
  schedule: [selected]
web_search:
  backends: [{{ type: brave, api_key: executor-search-key }}]
embedding:
  backend: openai
  api_key: executor-embedding-key
  model: text-embedding-3-small
schedules:
  - name: executor-schedule
    cron: "0 0 0 1 1 *"
    session: selected-session
    agent: selected
    task: executor-only
"#,
        proxy_socket.display()
    ))
    .unwrap();

    let started = Instant::now();
    let connected = instance::connect(&config).await.unwrap();
    let mut client_config = config.clone();
    let mut opts = BuildOptions::local_oneshot(
        dir.path().join("config.yaml"),
        connected.capabilities,
        false,
        false,
        Vec::new(),
        McpReadiness::Deferred,
    );
    opts.targeted_session = Some("selected-session".into());
    let built = server::build(&mut client_config, connected.instance, connected.user, opts)
        .await
        .unwrap();
    println!(
        "client bootstrap: {:?}; executor credential delays: {}",
        started.elapsed(),
        delayed.load(Ordering::SeqCst)
    );
    let client_delays = delayed.load(Ordering::SeqCst);
    let backend = BackendManager::new(&client_config.backends, built.secret_store.clone());
    assert_eq!(backend.resolve_model_name(None), "fallback-model");
    assert_eq!(
        built.secret_store.get("backend:catalog").as_deref(),
        Some("catalog-key")
    );
    assert!(!built.server.is_executor_authorized());
    assert!(built.server.is_startup_ready());
    assert!(built.server.extensions().tools_for_registry().is_empty());
    let (_, observed) = built.registry.open_session(&sid).await.unwrap();
    let ctx = chaz_core::commands::CommandContext {
        server: &built.server,
        secrets: &built.secret_store,
        backend: &backend,
        session_db_id: &sid,
        session_db: &observed,
        current_agent: "selected",
        session_name: Some("selected-session"),
    };
    for name in ["memory", "skills", "schedule"] {
        let chaz_core::commands::Parsed::Command(command) =
            chaz_core::commands::parse(&format!("/extensions add {name}"))
        else {
            panic!("extension activation must parse");
        };
        // Commands retain ordinary per-session activation and attribution.
        assert!(matches!(
            chaz_core::commands::dispatch(command, &ctx).await,
            chaz_core::commands::CommandOutcome::Text(_)
        ));
        let command = chaz_core::commands::Command::Extension {
            name: name.into(),
            args: "list".into(),
        };
        assert!(
            matches!(
                chaz_core::commands::dispatch(command, &ctx).await,
                chaz_core::commands::CommandOutcome::Text(_)
            ),
            "client command /{name} must remain usable"
        );
    }
    let model =
        chaz_core::commands::dispatch(chaz_core::commands::Command::Model(None), &ctx).await;
    assert!(
        matches!(model, chaz_core::commands::CommandOutcome::Text(text) if text.contains("Current Model: display-model (agent default)"))
    );
    let schedule = chaz_core::commands::Command::Extension {
        name: "schedule".into(),
        args: "add interval client-schedule 60 selected client-command".into(),
    };
    assert!(matches!(
        chaz_core::commands::dispatch(schedule, &ctx).await,
        chaz_core::commands::CommandOutcome::Text(text) if text.contains("client-schedule")
    ));
    let saved = agent_db
        .find_schedule("client-schedule")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.prompt, "client-command");
    assert_eq!(
        saved.target,
        chaz_core::agent_db::ScheduleTarget::Pinned {
            session_db_id: sid.clone()
        }
    );
    assert!(
        built
            .server
            .register_session(&db, backend, None, None)
            .await
            .is_err()
    );
    // Tear down this generation before the executor positive control.
    built.shutdown().await;
    let client_schedule = agent_db.find_schedule("executor-schedule").await.unwrap();

    // Positive control: the identical executor config reaches both delayed
    // writes and still materializes schedules. It runs no agent/model loop.
    let mut executor_config = config;
    executor_config.execution = Some(ExecutionRole::Executor);
    executor_config.web_search.as_mut().unwrap().backends[0].api_key =
        Some("positive-search-key".into());
    executor_config.embedding.as_mut().unwrap().api_key = Some("positive-embedding-key".into());
    let connected = instance::connect(&executor_config).await.unwrap();
    let opts = BuildOptions::local_oneshot(
        dir.path().join("config.yaml"),
        connected.capabilities,
        false,
        false,
        Vec::new(),
        McpReadiness::Deferred,
    );
    let built = server::build(
        &mut executor_config,
        connected.instance,
        connected.user,
        opts,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while agent_db
            .find_schedule("executor-schedule")
            .await
            .unwrap()
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let executor_delays = delayed.load(Ordering::SeqCst) - client_delays;
    built.shutdown().await;
    proxy.abort();
    let _ = proxy.await;
    drop(shutdown);
    service_task.await.unwrap().unwrap();

    assert_eq!(
        client_delays, 0,
        "client readiness waited on executor credentials"
    );
    assert!(
        client_schedule.is_none(),
        "client startup materialized an executor schedule"
    );
    assert_eq!(
        executor_delays, 2,
        "delay probe must detect both real executor operations"
    );
}
