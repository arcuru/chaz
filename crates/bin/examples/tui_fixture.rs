//! Provision only the disposable store used by the local PTY test.
use eidetica::{
    Instance, NewAccount,
    backend::database::{InMemory, Sqlite},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("database path required");
    // Match the library's disposable same-service fixtures: the service
    // owner lives across client/executor disconnects and restarts. The
    // existing direct-owner lifecycle fixture still uses its SQLite file.
    let (instance, user) = if std::env::args().nth(2).is_some() {
        Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::with_password("pty-test", "test-password"),
        )
        .await?
    } else {
        let backend = Sqlite::connect(&format!("sqlite://{path}?mode=rwc")).await?;
        Instance::create_backend(
            Box::new(backend),
            NewAccount::with_password("pty-test", "test-password"),
        )
        .await?
    };
    drop(user);
    if let Some(socket) = std::env::args().nth(2) {
        if std::env::args().nth(3).as_deref() == Some("controlled") {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            let mut running = None;
            loop {
                if running.is_none() {
                    let service =
                        eidetica::service::ServiceServer::bind(instance.clone(), &socket).await?;
                    let (shutdown, receiver) = tokio::sync::watch::channel(());
                    running = Some((shutdown, tokio::spawn(service.run(receiver))));
                    println!("fixture service ready");
                }
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    line = lines.next_line() => {
                        match line?.as_deref() {
                            Some("stop") => {
                                let (shutdown, task) = running.take().expect("service running");
                                drop(shutdown);
                                task.await??;
                                println!("fixture service stopped");
                                anyhow::ensure!(
                                    lines.next_line().await?.as_deref() == Some("start"),
                                    "expected explicit start"
                                );
                            }
                            Some("worker-records") => {
                                use chaz_core::{agent::AgentRegistry, session::{SessionRegistry, worker_links}};
                                let user = instance.login_account("pty-test", "test-password").await?;
                                let registry = SessionRegistry::new(instance.clone(), user, std::sync::Arc::new(AgentRegistry::from_config(&chaz_core::config::Config::default()))).await?;
                                let mut records = Vec::new();
                                for row in registry.list_sessions().await? {
                                    let (_, parent) = registry.open_session(&row.session_db_id).await?;
                                    for spawn in worker_links::worker_spawns(&parent).await? {
                                        let (_, child) = registry.open_session(&spawn.child_session_id).await?;
                                        worker_links::verify_worker_spawn(&parent, &child, &spawn).await?;
                                        records.push(serde_json::json!({
                                            "parent": parent.root_id().to_string(),
                                            "origin": worker_links::worker_origin(&child).await?,
                                            "outcome": worker_links::latest_worker_outcome(&child, &spawn.directive_id).await?,
                                            "spawn": spawn,
                                        }));
                                    }
                                }
                                println!("WORKER-RECORDS {}", serde_json::to_string(&records)?);
                            }
                            None | Some("quit") => break,
                            _ => anyhow::bail!("unknown fixture command"),
                        }
                    }
                }
            }
            if let Some((shutdown, task)) = running {
                drop(shutdown);
                task.await??;
            }
            return Ok(());
        }
        let service = eidetica::service::ServiceServer::bind(instance, &socket).await?;
        let (shutdown, receiver) = tokio::sync::watch::channel(());
        println!("fixture service ready");
        tokio::select! {
            result = service.run(receiver) => result?,
            result = tokio::signal::ctrl_c() => result?,
        }
        drop(shutdown);
    } else {
        drop(instance);
    }
    Ok(())
}
