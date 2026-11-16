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
                            Some("close-worker-parent") => {
                                use chaz_core::{agent::AgentRegistry, session::{SessionRegistry, worker_links}};
                                let user = instance.login_account("pty-test", "test-password").await?;
                                let registry = SessionRegistry::new(instance.clone(), user, std::sync::Arc::new(AgentRegistry::from_config(&chaz_core::config::Config::default()))).await?;
                                let mut closed = Vec::new();
                                for row in registry.list_sessions().await? {
                                    if row.source.as_deref() == Some("tui") {
                                        let (_, parent) = registry.open_session(&row.session_db_id).await?;
                                        if !worker_links::worker_spawns(&parent).await?.is_empty() {
                                            registry.mark_session_closed(&row.session_db_id).await?;
                                            closed.push(row.session_db_id);
                                        }
                                    }
                                }
                                println!("CLOSED-PARENTS {}", serde_json::to_string(&closed)?);
                            }
                            Some("worker-cases") => {
                                println!("WORKER-CASES {}", worker_cases(&instance).await?);
                            }
                            Some("worker-cases-audit") => {
                                println!("WORKER-AUDIT {}", worker_audit(&instance).await?);
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

/// Literal failure evidence in the real disposable service. No callbacks or
/// runtimes are installed; spawn: sources are excluded from resident adoption.
async fn worker_cases(instance: &Instance) -> anyhow::Result<serde_json::Value> {
    use chaz_core::{
        agent::AgentRegistry,
        runtime::{ToolCallRequest, ToolResultOutcome},
        session::{worker_links::*, *},
        types::ConversationId,
    };
    use eidetica::store::Table;
    use std::sync::Arc;
    let user = instance.login_account("pty-test", "test-password").await?;
    user.admin()
        .await?
        .create_account(NewAccount::with_password("case-stranger", "test-password"))
        .await?;
    let agents = Arc::new(AgentRegistry::from_config(
        &chaz_core::config::Config::default(),
    ));
    let registry = SessionRegistry::new(instance.clone(), user, agents.clone()).await?;
    let (_, parent) = registry.create_session(Some("spawn:case-parent")).await?;
    let parent_id = parent.root_id().to_string();
    registry
        .set_session_name(&parent_id, "evidence-parent".into())
        .await?;
    let parent_session = Session::new(ConversationId(parent_id.clone()), parent.clone()).await;
    let (_, root) = registry.create_session(Some("tui")).await?;
    registry
        .set_session_name(&root.root_id().to_string(), "independent-root".into())
        .await?;
    let (_, legacy) = registry
        .create_child_session(&parent_id, Some("spawn:legacy"))
        .await?;
    registry
        .set_session_name(&legacy.root_id().to_string(), "legacy-worker".into())
        .await?;
    Session::new(ConversationId(legacy.root_id().to_string()), legacy)
        .await
        .add_entry(case_entry(EntryType::Directive, "LEGACY-ASSIGNMENT"))
        .await?;
    let stranger = instance
        .login_account("case-stranger", "test-password")
        .await?;
    let stranger = SessionRegistry::new(instance.clone(), stranger, agents).await?;
    let mut children = serde_json::Map::new();
    for (index, case) in [
        "failed",
        "missing",
        "interrupted",
        "stop",
        "cancelled",
        "late",
        "not-started",
        "no-reply",
        "denied",
        "forged",
    ]
    .into_iter()
    .enumerate()
    {
        let spawn_id = format!("case-{case}");
        let source = format!("spawn:{spawn_id}");
        let (_, child) = if case == "denied" {
            stranger.create_session(Some(&source)).await?
        } else {
            registry
                .create_child_session(&parent_id, Some(&source))
                .await?
        };
        let id = child.root_id().to_string();
        if case != "denied" {
            registry
                .set_session_name(&id, format!("case-{case}"))
                .await?;
        }
        let mut session = Session::new(ConversationId(id.clone()), child.clone()).await;
        let directive = session
            .add_entry(case_entry(
                EntryType::Directive,
                if case == "denied" {
                    "DENIED-SECRET"
                } else {
                    case
                },
            ))
            .await?;
        let call = WorkerCallSite {
            request_id: TurnRequestId::parse("case-parent-request"),
            attempt_id: "case-parent-attempt".into(),
            execution_id: None,
            agent: "chaz".into(),
            model_sequence: index as u64,
            call_index: 0,
            call_id: "reused-provider-id".into(),
        };
        let arguments =
            serde_json::json!({"name":"helper", "task":format!("CASE-{case}")}).to_string();
        let mut invocation = case_entry(EntryType::ToolCall, &format!("spawn_worker({arguments})"));
        invocation.sender = "chaz".into();
        parent_session
            .append_turn_transcript(
                TurnTranscriptRecord {
                    request_id: call.request_id.clone(),
                    attempt_id: call.attempt_id.clone(),
                    sequence: index as u64 * 2,
                    timestamp: chrono::Utc::now(),
                    identity: Some(TurnCallIdentity {
                        agent: "chaz".into(),
                        requested_model: "synthetic".into(),
                        execution_id: None,
                    }),
                    message: TurnTranscriptMessage::ModelResponse {
                        model_sequence: index as u64,
                        content: None,
                        tool_calls: vec![ToolCallRequest {
                            id: call.call_id.clone(),
                            name: "spawn_worker".into(),
                            arguments,
                        }],
                        provider_extra: Default::default(),
                        metadata: None,
                        terminal: false,
                    },
                },
                vec![invocation],
            )
            .await?;
        let spawn = WorkerSpawn {
            version: 1,
            spawn_id: spawn_id.clone(),
            call: call.clone(),
            child_session_id: id.clone(),
            directive_id: directive.clone(),
        };
        let txn = child.new_transaction().await?;
        txn.get_store::<Table<WorkerOrigin>>("worker_origin_v1")
            .await?
            .set(
                "origin",
                WorkerOrigin {
                    version: 1,
                    parent_session_id: if case == "forged" {
                        root.root_id().to_string()
                    } else {
                        parent_id.clone()
                    },
                    spawn_id: spawn_id.clone(),
                },
            )
            .await?;
        txn.commit().await?;
        let txn = parent.new_transaction().await?;
        txn.get_store::<Table<WorkerSpawn>>("worker_spawns_v1")
            .await?
            .set(&spawn_id, spawn.clone())
            .await?;
        txn.commit().await?;
        if case != "not-started" && case != "denied" && case != "forged" {
            let mut attempt = session.start_turn_attempt(directive).await?;
            match case {
                "failed" => {
                    session
                        .complete_turn_attempt(
                            &attempt,
                            Some(case_entry(EntryType::Error, "SYNTHETIC-FAILURE")),
                        )
                        .await?
                }
                "missing" | "interrupted" => {
                    attempt.started_at -= chrono::Duration::minutes(2);
                    if case == "missing" {
                        attempt.status = TurnAttemptStatus::Completed;
                        attempt.completed_at = Some(chrono::Utc::now());
                    }
                    let txn = child.new_transaction().await?;
                    txn.get_store::<Table<TurnAttempt>>("turn_attempts")
                        .await?
                        .set(&attempt.attempt_id, attempt.clone())
                        .await?;
                    txn.commit().await?;
                }
                "stop" => {
                    session.request_stop("user").await?;
                }
                "cancelled" => {
                    session.request_stop("user").await?;
                    session
                        .complete_turn_attempt_with_transcript(
                            &attempt,
                            Some(case_entry(EntryType::Error, "CANCEL-ACK")),
                            Some(TurnTranscriptRecord {
                                request_id: attempt.request_id.clone(),
                                attempt_id: attempt.attempt_id.clone(),
                                sequence: 0,
                                timestamp: chrono::Utc::now(),
                                identity: None,
                                message: TurnTranscriptMessage::Cancelled,
                            }),
                        )
                        .await?;
                }
                "late" => {
                    // A failed parent call does not overwrite a later child reply.
                    let mut display =
                        case_entry(EntryType::ToolResult, "spawn_worker: ERROR: timeout");
                    display.sender = "chaz".into();
                    parent_session
                        .append_turn_transcript(
                            TurnTranscriptRecord {
                                request_id: call.request_id,
                                attempt_id: call.attempt_id,
                                sequence: index as u64 * 2 + 1,
                                timestamp: chrono::Utc::now(),
                                identity: Some(TurnCallIdentity {
                                    agent: "chaz".into(),
                                    requested_model: "synthetic".into(),
                                    execution_id: None,
                                }),
                                message: TurnTranscriptMessage::ToolResult {
                                    model_sequence: index as u64,
                                    call_index: 0,
                                    call_id: call.call_id,
                                    name: "spawn_worker".into(),
                                    output: "parent observation timed out".into(),
                                    outcome: ToolResultOutcome::TimedOut,
                                },
                            },
                            vec![display],
                        )
                        .await?;
                    session
                        .complete_turn_attempt(
                            &attempt,
                            Some(case_entry(EntryType::Message, "LATER-CHILD-REPLY")),
                        )
                        .await?;
                }
                "no-reply" => session.complete_turn_attempt(&attempt, None).await?,
                _ => unreachable!(),
            }
        }
        if case != "denied" && case != "forged" {
            worker_links::verify_worker_spawn(&parent, &child, &spawn).await?;
        }
        children.insert(case.into(), id.into());
    }
    Ok(serde_json::json!({"parent":parent_id,"children":children}))
}
fn case_entry(
    entry_type: chaz_core::session::EntryType,
    content: &str,
) -> chaz_core::session::SessionEntry {
    chaz_core::session::SessionEntry {
        sender: "user".into(),
        content: content.into(),
        timestamp: chrono::Utc::now(),
        entry_type,
        metadata: None,
        routing: None,
    }
}
async fn worker_audit(instance: &Instance) -> anyhow::Result<serde_json::Value> {
    use chaz_core::{agent::AgentRegistry, session::SessionRegistry};
    let user = instance.login_account("pty-test", "test-password").await?;
    let registry = SessionRegistry::new(
        instance.clone(),
        user,
        std::sync::Arc::new(AgentRegistry::from_config(
            &chaz_core::config::Config::default(),
        )),
    )
    .await?;
    let mut tips = std::collections::BTreeMap::new();
    for row in registry.list_sessions().await? {
        let (_, db) = registry.open_session(&row.session_db_id).await?;
        let mut ids: Vec<_> = db
            .snapshot()
            .await?
            .into_tips()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        ids.sort();
        tips.insert(row.session_db_id, ids);
    }
    Ok(serde_json::to_value(tips)?)
}
