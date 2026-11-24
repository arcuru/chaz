//! Disposable IPC owner for the TUI loss/recovery PTY regression. No model.
use chaz_core::{
    agent::AgentRegistry,
    agent_db::{AgentDbConfig, AgentMeta, create_agent_db},
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
        NewAccount::with_password("pty-service", "test-password"),
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
    let registry =
        SessionRegistry::new(owner.clone(), user, Arc::new(AgentRegistry::default())).await?;
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
                pubkey: pubkey.clone(),
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
    let mode = std::env::args().nth(2);
    let mut secondary = if mode.as_deref() == Some("outage") {
        let (conv, db) = registry.create_session(Some("tui")).await?;
        let id = db.root_id().to_string();
        registry.set_session_name(&id, "secondary".into()).await?;
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
        let mut session = Session::new(conv, db).await;
        session.add_entry(entry("SECONDARY-CACHED")).await?;
        Some(session)
    } else {
        None
    };
    if mode.as_deref() == Some("worker-links") {
        // Literal side-store fixture is also read by the supported older binary.
        // It must not change how either version reads the original transcript.
        let (_, child) = registry
            .create_child_session(&id, Some("spawn:fixture-spawn"))
            .await?;
        registry
            .set_session_name(&child.root_id().to_string(), "worker-compatibility".into())
            .await?;
        let rows: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../lib/src/session/fixtures/worker-links.snap"
        ))?;
        for row in rows {
            let target = if row["store"] == "worker_spawns_v1" {
                &db
            } else {
                &child
            };
            let mut value = row["value"].clone();
            if row["store"] == "worker_spawns_v1" {
                value["child_session_id"] = child.root_id().to_string().into();
            }
            if row["store"] == "worker_origin_v1" {
                value["parent_session_id"] = id.clone().into();
            }
            let txn = target.new_transaction().await?;
            txn.get_store::<eidetica::store::Table<serde_json::Value>>(
                row["store"].as_str().unwrap(),
            )
            .await?
            .set(row["id"].as_str().unwrap(), value)
            .await?;
            txn.commit().await?;
        }
        let txn = child.new_transaction().await?;
        let entries = txn
            .get_store::<eidetica::store::Table<SessionEntry>>("entries")
            .await?;
        let mut directive = entry("COMPAT-CHILD-ASSIGNMENT");
        directive.entry_type = EntryType::Directive;
        entries.set("child-directive", directive).await?;
        entries
            .set("child-final", entry("COMPAT-CHILD-RESULT"))
            .await?;
        txn.get_store::<eidetica::store::Table<chaz_core::session::TurnAttempt>>("turn_attempts")
            .await?
            .set(
                "child-attempt",
                chaz_core::session::TurnAttempt {
                    attempt_id: "child-attempt".into(),
                    request_id: chaz_core::session::TurnRequestId::parse("child-directive"),
                    started_at: chrono::Utc::now(),
                    generation: 1,
                    status: chaz_core::session::TurnAttemptStatus::Completed,
                    completed_at: Some(chrono::Utc::now()),
                },
            )
            .await?;
        txn.commit().await?;
        session
            .add_entry(entry("COMPAT-PARENT-AFTER-SIDE-RECORDS"))
            .await?;
    }
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
                if let Some(secondary) = secondary.as_mut() {
                    secondary.add_entry(entry("SECONDARY-FRESH")).await?;
                }
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
            "agent-cap" | "agent-repair" => {
                let capped = command == "agent-cap";
                agent_db
                    .write_config(&AgentDbConfig {
                        model: capped.then(|| "stub:agent-alt".into()),
                        max_context_tokens: capped.then_some(16_000),
                        ..Default::default()
                    })
                    .await?;
                println!("AGENT {command}");
            }
            "agent-corrupt" => {
                let txn = agent_db.database().new_transaction().await?;
                txn.get_store::<eidetica::store::DocStore>(chaz_core::agent_db::CONFIG_STORE)
                    .await?
                    .set("value", true)
                    .await?;
                txn.commit().await?;
                println!("AGENT {command}");
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
