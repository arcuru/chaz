//! Disposable IPC owner for the TUI loss/recovery PTY regression. No model.
use chaz_core::{
    agent::AgentRegistry,
    agent_db::{AgentDbConfig, AgentMeta, create_agent_db},
    config::Config,
    extension::{ExtensionOutput, write_extension_outputs},
    hosted_index::DbEntry,
    session::{EntryType, Session, SessionEntry, SessionRegistry},
};
use eidetica::{Instance, NewAccount, backend::database::InMemory, service::ServiceServer};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::watch,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let socket = std::env::args().nth(1).expect("socket path required");
    let (owner, mut user) = Instance::create_backend(
        Box::new(InMemory::new()),
        NewAccount::passwordless("pty-service"),
    )
    .await?;
    let (agent_db, pubkey) = create_agent_db(
        &mut user,
        "chaz",
        &AgentDbConfig::default(),
        &AgentMeta {
            display_name: Some("chaz".into()),
            ..Default::default()
        },
    )
    .await?;
    let registry = SessionRegistry::new(
        owner.clone(),
        user,
        Arc::new(AgentRegistry::from_config(&Config::default())),
    )
    .await?;
    let (conv, db) = registry.create_session(Some("tui")).await?;
    let id = db.root_id().to_string();
    registry
        .set_session_name(&id, "service-fixture".into())
        .await?;
    registry
        .attach_agent_to_session(
            &id,
            &DbEntry {
                db_id: agent_db.id(),
                display_name: "chaz".into(),
                pubkey,
            },
        )
        .await?;
    let mut session = Session::new(conv, db.clone()).await;
    session
        .add_entry(entry("SERVICE-CACHED-TRANSCRIPT"))
        .await?;
    write_extension_outputs(
        &db,
        &BTreeMap::from([(
            "fixture".into(),
            ExtensionOutput {
                status: BTreeMap::from([("state".into(), "SERVICE-CACHED-STATUS".into())]),
            },
        )]),
    )
    .await?;
    let agents_json = serde_json::to_string(&session.try_read_meta().await?.agents)?;
    let service = ServiceServer::bind(owner.clone(), &socket).await?;
    let (shutdown, receiver) = watch::channel(());
    let mut running = Some((shutdown, tokio::spawn(service.run(receiver))));
    println!("READY {id}");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(command) = lines.next_line().await? {
        match command.as_str() {
            "stop" => {
                if let Some((shutdown, task)) = running.take() {
                    drop(shutdown);
                    task.await??;
                }
                println!("STOPPED");
            }
            "start" => {
                anyhow::ensure!(running.is_none(), "service already running");
                let service = ServiceServer::bind(owner.clone(), &socket).await?;
                let (shutdown, receiver) = watch::channel(());
                running = Some((shutdown, tokio::spawn(service.run(receiver))));
                println!("STARTED");
            }
            "corrupt-meta" | "repair-meta" => {
                let value = if command == "corrupt-meta" {
                    "not-json".to_string()
                } else {
                    agents_json.clone()
                };
                db.with_transaction(|txn| async move {
                    txn.get_store::<eidetica::store::DocStore>("meta")
                        .await?
                        .set_string("agents", &value)
                        .await
                })
                .await?;
                println!("META {}", command);
            }
            "sessions" => {
                println!("SESSIONS {}", registry.list_sessions().await?.len());
            }
            "entries" => {
                let (conv, db) = registry.open_session(&id).await?;
                let snapshot = Session::try_new(conv, db).await?;
                println!("ENTRIES {}", snapshot.entries().len());
            }
            "name" => {
                println!(
                    "NAME {}",
                    session.try_read_meta().await?.name.unwrap_or_default()
                );
            }
            "append" => {
                session.add_entry(entry("RECOVERY-VISIBLE")).await?;
                write_extension_outputs(
                    &db,
                    &BTreeMap::from([(
                        "fixture".into(),
                        ExtensionOutput {
                            status: BTreeMap::from([("state".into(), "RECOVERY-STATUS".into())]),
                        },
                    )]),
                )
                .await?;
                println!("APPENDED");
            }
            "quit" => break,
            _ => anyhow::bail!("unknown fixture command"),
        }
    }
    if let Some((shutdown, task)) = running {
        drop(shutdown);
        task.await??;
    }
    Ok(())
}
fn entry(content: &str) -> SessionEntry {
    SessionEntry {
        sender: "user".into(),
        content: content.into(),
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    }
}
