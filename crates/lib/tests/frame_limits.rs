//! The YAML setting reaches real authenticated service connections, not a codec mock.
#![cfg(unix)]

use std::time::Duration;

use chaz_core::{
    bridge::process::{self, DISCORD, MATRIX},
    config::{Config, check_unknown_config_keys},
    instance::{self, ConnectError, ConnectedInstance, InstanceOwnership, ServiceGeneration},
};
use eidetica::{
    Instance, NewAccount,
    auth::types::SigKey,
    backend::database::InMemory,
    crdt::Doc,
    entry::ID,
    service::{
        ServiceConfig, ServiceServer,
        protocol::{ServerFrame, ServiceResponse},
    },
    store::DocStore,
};
use tokio::sync::watch;

fn config(url: &str, bytes: Option<u64>) -> Config {
    let limit = bytes
        .map(|value| format!("  max_frame_bytes: {value}\n"))
        .unwrap_or_default();
    let yaml = format!(
        "execution: client\neidetica:\n  connection: {url}\n{limit}  login:\n    username: frames\n    passwordless: true\n"
    );
    assert!(check_unknown_config_keys(&yaml).is_empty());
    serde_yaml::from_str(&yaml).unwrap()
}

async fn owner() -> Instance {
    Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::passwordless("frames"),
    )
    .await
    .unwrap()
    .0
}

async fn serve(
    owner: &Instance,
    socket: &std::path::Path,
    bytes: u64,
) -> (
    watch::Sender<()>,
    tokio::task::JoinHandle<eidetica::Result<()>>,
) {
    let server =
        ServiceServer::bind_with_config(owner.clone(), socket, ServiceConfig::new(bytes).unwrap())
            .await
            .unwrap();
    let (shutdown, rx) = watch::channel(());
    (shutdown, tokio::spawn(server.run(rx)))
}

async fn stop(shutdown: watch::Sender<()>, task: tokio::task::JoinHandle<eidetica::Result<()>>) {
    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

async fn stored_entry(owner: &Instance, bytes: usize) -> (ID, SigKey, ID) {
    let mut user = owner.login_account("frames", None).await.unwrap();
    let key = user.get_default_key().unwrap();
    let db = user.create_database(Doc::new(), &key).await.unwrap();
    db.with_transaction(|tx| async move {
        tx.get_store::<DocStore>("fixture")
            .await?
            .set("payload", "x".repeat(bytes))
            .await
    })
    .await
    .unwrap();
    let id = db
        .snapshot()
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .clone();
    (db.root_id().clone(), SigKey::from_pubkey(&key), id)
}

async fn read(
    connected: &ConnectedInstance,
    entry: &(ID, SigKey, ID),
) -> eidetica::Result<eidetica::Entry> {
    tokio::time::timeout(
        Duration::from_secs(60),
        connected
            .instance
            .remote_connection()
            .unwrap()
            .db_get_entry(entry.0.clone(), entry.1.clone(), entry.2.clone()),
    )
    .await
    .expect("frame refusal or read must complete, never strand an RPC")
}

#[tokio::test]
async fn invalid_frame_settings_fail_before_opening_or_mutating_targets() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("unopened.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let url = format!("unix://{}", socket.display());
    for bytes in [0, u64::from(u32::MAX) + 1, u64::MAX] {
        let config = config(&url, Some(bytes));
        let expected = ServiceConfig::new(bytes).unwrap_err().to_string();
        assert!(
            instance::required_settings(&config)
                .unwrap_err()
                .to_string()
                .contains(&expected)
        );
        let error = instance::connect(&config).await.err().unwrap();
        assert!(matches!(error, ConnectError::Config(_)));
        assert!(error.to_string().contains("eidetica.max_frame_bytes"));
        assert!(!error.is_transient_service());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "invalid limits must fail before connecting"
    );

    let path = dir.path().join("missing.db");
    for url in [
        format!("sqlite://{}?mode=rwc", path.display()),
        "memory://".into(),
        "postgres://localhost/chaz".into(),
    ] {
        let config = config(&url, Some(1024 * 1024 * 1024));
        let error = instance::connect(&config).await.err().unwrap();
        assert!(matches!(error, ConnectError::Config(_)));
        assert!(
            error
                .to_string()
                .contains("embedded backends have no service frames")
        );
    }
    assert!(
        !path.exists(),
        "reject the setting before SQLite creates a file"
    );
    for value in ["-1", "1.5", "\"1024\"", "18446744073709551616"] {
        let yaml = format!(
            "eidetica:\n  connection: {url}\n  max_frame_bytes: {value}\n  login: {{username: frames, passwordless: true}}\n"
        );
        assert!(serde_yaml::from_str::<Config>(&yaml).is_err(), "{value}");
    }
}

#[tokio::test]
async fn default_custom_and_small_limits_reach_connect_and_both_gateway_paths() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("service.sock");
    let owner = owner().await;
    let entry = stored_entry(&owner, 16 * 1024).await;
    let (shutdown, task) = serve(&owner, &socket, 128 * 1024).await;
    let url = format!("unix://{}", socket.display());
    assert_eq!(ServiceConfig::default().max_frame_bytes(), 67108864);
    for bytes in [None, Some(128 * 1024), Some(1024 * 1024 * 1024)] {
        let connected = instance::connect(&config(&url, bytes)).await.unwrap();
        assert_eq!(
            connected.capabilities.ownership(),
            InstanceOwnership::Service
        );
        assert!(!connected.capabilities.runs_agents());
        let reply = read(&connected, &entry).await.unwrap();
        assert_eq!(reply.id(), entry.2);
        let encoded = serde_json::to_vec(&ServerFrame::Response(Box::new(ServiceResponse::Entry(
            reply,
        ))))
        .unwrap()
        .len();
        assert!(
            encoded > 32 * 1024 && encoded < 128 * 1024,
            "small response: {encoded} bytes"
        );
    }
    let tiny = tokio::time::timeout(
        Duration::from_secs(5),
        instance::connect(&config(&url, Some(1))),
    )
    .await
    .expect("a refused handshake must not hang");
    assert!(matches!(tiny, Err(ConnectError::Connection(_))));
    for transport in [MATRIX, DISCORD] {
        let connected = process::connect(&config(&url, Some(32 * 1024)), transport, dir.path())
            .await
            .unwrap();
        assert!(
            read(&connected, &entry).await.is_err(),
            "gateway must retain the small client cap"
        );
        assert!(instance::probe_service(&connected.instance).await.is_err());
    }
    stop(shutdown, task).await;

    // The other mismatch direction: the client accepts a reply the server refuses.
    let (shutdown, task) = serve(&owner, &socket, 32 * 1024).await;
    let connected = instance::connect(&config(&url, None)).await.unwrap();
    assert!(read(&connected, &entry).await.is_err());
    assert!(instance::probe_service(&connected.instance).await.is_err());
    stop(shutdown, task).await;
}

struct Generation(ConnectedInstance);
impl ServiceGeneration for Generation {
    fn instance(&self) -> Instance {
        self.0.instance.clone()
    }
    async fn finished(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
    async fn shutdown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn configured_large_reply_survives_reconnect_while_default_still_refuses() {
    const LIMIT: u64 = 1024 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("large.sock");
    let owner = owner().await;
    let entry = stored_entry(&owner, 65 * 1024 * 1024).await;
    let (shutdown, task) = serve(&owner, &socket, LIMIT).await;
    let url = format!("unix://{}", socket.display());
    let config = config(&url, Some(LIMIT));
    let mut generation = Generation(instance::connect(&config).await.unwrap());
    let reply = read(&generation.0, &entry).await.unwrap();
    assert_eq!(reply.id(), entry.2);

    // Count the actual encoded response without another payload-sized allocation.
    #[derive(Default)]
    struct Count(u64);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len() as u64;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count::default();
    serde_json::to_writer(
        &mut count,
        &ServerFrame::Response(Box::new(ServiceResponse::Entry(reply))),
    )
    .unwrap();
    assert!(
        count.0 > 64 * 1024 * 1024 && count.0 < LIMIT,
        "actual response: {} bytes",
        count.0
    );
    println!(
        "configured service reply: {} encoded bytes; cap: {LIMIT}",
        count.0
    );
    instance::probe_service(&generation.0.instance)
        .await
        .unwrap();

    let default = instance::connect(&self::config(&url, None)).await.unwrap();
    assert!(
        read(&default, &entry).await.is_err(),
        "negative control: omitted field retains 64 MiB"
    );
    assert!(instance::probe_service(&default.instance).await.is_err());
    stop(shutdown, task).await;
    assert!(
        instance::probe_service(&generation.0.instance)
            .await
            .is_err()
    );
    let (shutdown, task) = serve(&owner, &socket, LIMIT).await;
    instance::replace_service_generation(&mut generation, || async {
        Ok(Generation(instance::connect(&config.clone()).await?))
    })
    .await
    .unwrap();
    assert_eq!(read(&generation.0, &entry).await.unwrap().id(), entry.2);
    instance::probe_service(&generation.0.instance)
        .await
        .unwrap();
    stop(shutdown, task).await;
}
