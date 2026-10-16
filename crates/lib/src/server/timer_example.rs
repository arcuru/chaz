//! A timer-shaped caller, not a scheduler or submission-retry API.
//! Run with the existing restart cases: see dev/timer-job/README.md.
use super::*;
use crate::session::jobs::JobStatus;
use crate::tool::{Tool, ToolDescriptor, ToolError};
use crate::tool_host::Capability;
use std::path::{Path, PathBuf};

const REPORT: &str = "# Offline timer report\n\nChecks: 3\nFailures: 0\n";
const TICK: &str = "Produce the offline timer report for this tick";

/// Fixed disposable output, no caller-controlled paths or model/provider code.
/// The hold is after the write but before its tool result becomes durable.
struct Report {
    path: PathBuf,
    writes: Arc<AtomicUsize>,
    hold_after_write: bool,
}

impl Tool for Report {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "report".into(),
            description: "Write the fixed offline fixture report".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn execute<'a>(
        &'a self,
        _: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
    {
        Box::pin(async move {
            ctx.host()
                .request(
                    &Capability::FileWrite {
                        path: self.path.to_str().unwrap().into(),
                        content: REPORT.into(),
                    },
                    ctx.grants(),
                )
                .await?;
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.hold_after_write {
                std::future::pending::<()>().await;
            }
            Ok("report written".into())
        })
    }
}

fn report_script() -> Arc<MockBackend> {
    let mock = Arc::new(MockBackend::new());
    mock.push_tool_calls([("report-call".into(), "report".into(), "{}".into())]);
    mock.push_text("report ready");
    mock
}

/// Inspect a known parent's catalog, then the candidate DB's request. These
/// fields are evidence, not a stable publication key: identical ticks collide.
async fn inspect_tick(client: &Server, parent: &str) -> anyhow::Result<String> {
    let mut candidates = Vec::new();
    for row in client.registry().list_sessions().await? {
        if !row
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("job-stage:"))
        {
            continue;
        }
        let (_, db) = client.registry().open_session(&row.session_db_id).await?;
        let request = crate::session::SessionRegistry::read_job_request(&db).await?;
        if request.parent_id == parent && request.target == "default" && request.task == TICK {
            candidates.push(row.session_db_id);
        }
    }
    anyhow::ensure!(
        candidates.len() == 1,
        "submission uncertain: {} matching published handles; inspect manually, do not resubmit",
        candidates.len()
    );
    Ok(candidates.pop().unwrap())
}

/// Application acceptance is stricter than the Agent's terminal receipt.
fn accept_report(status: &JobStatus, original_handle: &str, path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(status.session_db_id == original_handle, "wrong job handle");
    anyhow::ensure!(
        matches!(status.state, JobState::Succeeded { .. }),
        "job not successful"
    );
    anyhow::ensure!(
        std::fs::read(path)? == REPORT.as_bytes(),
        "wrong report content"
    );
    Ok(())
}

async fn timer_client(fx: &Fixture) -> (Arc<Server>, String) {
    let connection =
        crate::instance::connect_with(&fx.settings, crate::config::ExecutionRole::Client)
            .await
            .unwrap();
    let registry = Arc::new(
        crate::session::SessionRegistry::new(
            connection.instance,
            connection.user,
            Arc::new(AgentRegistry::with_default_agent()),
        )
        .await
        .unwrap(),
    );
    let (parent, _) = registry
        .create_session(Some("offline-timer"))
        .await
        .unwrap();
    registry
        .attach_agent_to_session(&parent.0, &fx.agent)
        .await
        .unwrap();
    (
        client_server_fixture_from_registry(registry).await,
        parent.0,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_ack_is_inspected_and_receipt_is_not_artifact_acceptance() {
    let fx = Fixture::new("timer-report").await;
    let (client, parent) = timer_client(&fx).await;
    assert!(!client.is_executor_authorized());
    let path = fx._dir.path().join("report.md");
    let writes = Arc::new(AtomicUsize::new(0));

    // An unpublished prepared DB is inert, and zero candidates is uncertainty,
    // not proof that it is safe to publish another job.
    let prepared = client
        .registry()
        .prepare_agent_job(&parent, "default", TICK)
        .await
        .unwrap();
    let original = prepared.db.root_id().to_string();
    assert!(
        inspect_tick(&client, &parent)
            .await
            .unwrap_err()
            .to_string()
            .contains("0 matching")
    );
    let executor = fx.start_with(report_script()).await;
    executor.server.tools.register(Report {
        path: path.clone(),
        writes: writes.clone(),
        hold_after_write: false,
    });
    let permits = executor
        .server
        .semaphore
        .clone()
        .try_acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
        .unwrap();
    client.registry().publish_agent_job(prepared).await.unwrap(); // discard acknowledgement
    let handle = inspect_tick(&client, &parent).await.unwrap();
    assert_eq!(handle, original);
    let pending = client
        .wait_job(&handle, Duration::from_millis(20))
        .await
        .unwrap();
    assert!(
        matches!(pending.state, JobState::Pending | JobState::Queued),
        "timeout is not completion: {pending:?}"
    );
    assert!(!path.exists());
    assert!(executor.child_mock.recorded_calls().is_empty());
    assert!(accept_report(&pending, &original, &path).is_err());
    drop(permits);
    let receipt = tokio::time::timeout(
        Duration::from_secs(30),
        client.wait_job(&handle, Duration::from_secs(20)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        receipt.state,
        JobState::Succeeded {
            text: Some("report ready".into())
        }
    );
    assert_eq!(client.job_status(&original).await.unwrap(), receipt);
    accept_report(&receipt, &original, &path).unwrap();
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(executor.child_mock.recorded_calls().len(), 2);
    assert_eq!(job_count(&executor.server).await, 1);
    let (_, db) = client.registry().open_session(&original).await.unwrap();
    assert_eq!(
        Session::new(ConversationId(original.clone()), db)
            .await
            .entries()
            .iter()
            .filter(|e| e.entry_type == EntryType::Directive)
            .count(),
        1
    );
    eprintln!(
        "timer: lost acknowledgement -> inspected original handle; bounded wait -> Succeeded; report bytes accepted"
    );

    // Known-bad exit-only acceptance stays green for missing/wrong output.
    // Our application check must reject both despite the same success receipt.
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(receipt.state, JobState::Succeeded { .. }));
    assert!(accept_report(&receipt, &original, &path).is_err());
    std::fs::write(&path, "wrong report").unwrap();
    assert!(matches!(receipt.state, JobState::Succeeded { .. }));
    assert!(accept_report(&receipt, &original, &path).is_err());
    std::fs::write(&path, REPORT).unwrap();
    assert!(accept_report(&receipt, "another handle", &path).is_err());
    accept_report(&receipt, &original, &path).unwrap();
    eprintln!("timer: negative control rejected missing/wrong artifact despite Succeeded");
    executor.server.shutdown().await;

    // Deliberately unsafe counterexample: blindly resubmitting the same tick
    // really creates a different durable handle. Inspection must not pick one.
    let duplicate = client
        .submit_agent_job(&parent, "default", TICK)
        .await
        .unwrap();
    assert_ne!(duplicate, original);
    assert_eq!(job_count(&client).await, 2);
    assert!(
        inspect_tick(&client, &parent)
            .await
            .unwrap_err()
            .to_string()
            .contains("2 matching")
    );
    assert_eq!(
        client.job_status(&duplicate).await.unwrap().state,
        JobState::Pending
    );
    eprintln!(
        "timer: blind-republish control created two handles -> submission uncertain; no automatic retry"
    );
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_retry_can_repeat_an_already_written_report() {
    let fx = Fixture::new("timer-retry").await;
    let (client, parent) = timer_client(&fx).await;
    let path = fx._dir.path().join("report.md");
    let writes = Arc::new(AtomicUsize::new(0));
    let first = fx.start_with(report_script()).await;
    first.server.tools.register(Report {
        path: path.clone(),
        writes: writes.clone(),
        hold_after_write: true,
    });
    let handle = client
        .submit_agent_job(&parent, "default", TICK)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while writes.load(Ordering::SeqCst) != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), REPORT.as_bytes());
    let running = first.server.job_status(&handle).await.unwrap();
    assert!(matches!(running.state, JobState::Running { .. }));
    assert!(
        accept_report(&running, &handle, &path).is_err(),
        "file alone is not acceptance either"
    );
    first.server.shutdown().await;
    drop(first);

    let restarted = fx.start_with(report_script()).await;
    restarted.server.tools.register(Report {
        path: path.clone(),
        writes: writes.clone(),
        hold_after_write: false,
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        while !restarted.server.is_watching_session(&handle).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let interrupted = restarted.server.job_status(&handle).await.unwrap();
    assert!(matches!(interrupted.state, JobState::Interrupted { .. }));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert!(
        restarted.child_mock.recorded_calls().is_empty(),
        "no automatic replay"
    );
    let turns = restarted.server.interrupted_turns(&handle).await.unwrap();
    assert_eq!(turns.len(), 1);
    let (_, db) = client.registry().open_session(&handle).await.unwrap();
    let transcript = Session::new(ConversationId(handle.clone()), db)
        .await
        .turn_transcript(&turns[0].attempt_id)
        .await
        .unwrap();
    assert!(
        !transcript
            .iter()
            .any(|r| matches!(r.message, TurnTranscriptMessage::ToolResult { .. })),
        "fault is after the effect but before its durable result"
    );
    let retry = restarted
        .server
        .retry_interrupted_turn(&handle, &turns[0].request_id)
        .await
        .unwrap();
    assert_ne!(retry, turns[0].attempt_id);
    let receipt = tokio::time::timeout(
        Duration::from_secs(30),
        client.wait_job(&handle, Duration::from_secs(20)),
    )
    .await
    .unwrap()
    .unwrap();
    accept_report(&receipt, &handle, &path).unwrap();
    assert_eq!(
        writes.load(Ordering::SeqCst),
        2,
        "explicit retry repeated the file-write effect"
    );
    assert_eq!(
        job_count(&client).await,
        1,
        "same handle, not another submission"
    );
    eprintln!(
        "timer: interrupted after write -> held; explicit retry -> same handle, new attempt, two writes (not exactly-once)"
    );
    restarted.server.shutdown().await;
    client.shutdown().await;
}
