//! Jobs overview and observe-only tabs; no server registration or execution.
use super::*;
use chaz_core::server::JobMonitor;
use chaz_core::session::{
    jobs::JobState,
    steering::{JobInput, JobInputRequest},
};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph, Wrap},
};

#[derive(Default)]
pub(super) struct JobsView {
    pub data: JobMonitor,
    pub selected: usize,
    pub offset: usize,
    pub loading: bool,
    pub error: Option<String>,
}

pub(super) struct JobTab {
    pub state: Option<JobState>,
    pub inputs: Vec<JobInput>,
    pub feedback: String,
    pub retry: Option<JobInputRequest>,
    pub _callback: eidetica::WriteCallback,
}

pub(super) fn refresh(
    app: &mut App,
    server: &Arc<Server>,
    tx: &mpsc::Sender<Result<JobMonitor, String>>,
) {
    if app.jobs.loading {
        return;
    }
    app.jobs.loading = true;
    let server = server.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), server.job_monitor()).await;
        let result = match result {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => Err("refresh timed out; retained snapshot is stale".into()),
        };
        let _ = tx.send(result).await;
    });
}

pub(super) fn apply(app: &mut App, result: Result<JobMonitor, String>) {
    app.jobs.loading = false;
    match result {
        Ok(data) => {
            let selected_id = app
                .jobs
                .data
                .nodes
                .get(app.jobs.selected)
                .map(|n| n.session_db_id.clone());
            app.jobs.selected = selected_id
                .and_then(|id| data.nodes.iter().position(|n| n.session_db_id == id))
                .unwrap_or(0);
            app.jobs.data = data;
            app.jobs.error = None;
        }
        Err(error) => app.jobs.error = Some(error),
    }
}

pub(super) async fn open(
    app: &mut App,
    server: &Server,
    backend: &BackendManager,
    id: &str,
    tx: &mpsc::Sender<String>,
) -> anyhow::Result<()> {
    let open = async {
        if let Some(index) = app.tab_index_for(id) {
            app.set_active_tab(index);
            app.mode = TuiMode::Chat;
            return Ok(());
        }
        let (_, db) = server.registry().open_job_session(id).await?;
        let tips = db.snapshot().await?;
        let notify = tx.clone();
        let sid = id.to_string();
        let callback = db
            .on_write_at_tips(tips, move |_, _| {
                let notify = notify.clone();
                let sid = sid.clone();
                async move {
                    let _ = notify.send(sid).await;
                    Ok(())
                }
            })
            .await?;
        // Read after callback installation to cover both existing and new writes.
        let mut tab = build_tab(server, backend, db, id.into()).await;
        tab.job = Some(JobTab {
            state: None,
            inputs: Vec::new(),
            feedback: String::new(),
            retry: None,
            _callback: callback,
        });
        refresh_tab(&mut tab).await;
        app.push_tab(tab);
        app.mode = TuiMode::Chat;
        Ok(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), open)
        .await
        .map_err(|_| anyhow::anyhow!("job observation timed out; no execution acquired"))?
}

pub(super) async fn refresh_tab(tab: &mut Tab) {
    if tab.job.is_none() {
        return;
    }
    if tokio::time::timeout(std::time::Duration::from_secs(2), refresh_tab_snapshot(tab))
        .await
        .is_err()
        && let Some(job) = tab.job.as_mut()
    {
        job.state = None;
        job.feedback = "unavailable: refresh timed out; retained evidence is stale".into();
    }
}

async fn refresh_tab_snapshot(tab: &mut Tab) {
    let Some(job) = tab.job.as_mut() else {
        return;
    };
    let entries = async {
        chaz_core::session::jobs::require_observer_write(&tab.session_db).await?;
        let snapshot = tab.session_db.snapshot().await?;
        Session::entries_at_snapshot(&tab.session_db, &snapshot).await
    }
    .await;
    match entries {
        Ok(entries) => tab.entries = entries,
        Err(error) => {
            job.state = None;
            job.feedback = format!("unavailable; retained transcript is stale: {error}");
            return;
        }
    }
    let session = Session::new(
        chaz_core::types::ConversationId(tab.session_db_id.clone()),
        tab.session_db.clone(),
    )
    .await;
    match chaz_core::session::jobs::observer_status(&tab.session_db).await {
        Ok(status) => {
            tab.active_turns = usize::from(matches!(
                status.state,
                JobState::StartedUnknown {
                    activity_recent: true,
                    ..
                }
            ));
            job.state = Some(status.state);
        }
        Err(error) => {
            job.state = None;
            job.feedback = format!("unavailable: {error}");
        }
    }
    match session.job_inputs().await {
        Ok(inputs) => {
            job.inputs = inputs;
            if job.state.is_some()
                && (job.feedback.starts_with("unavailable")
                    || job.feedback.starts_with("input evidence unavailable"))
            {
                job.feedback.clear();
            }
        }
        Err(error) => job.feedback = format!("input evidence unavailable: {error}"),
    }
}

pub(super) async fn send(tab: &mut Tab, text: String) {
    refresh_tab(tab).await;
    let job = tab.job.as_mut().expect("job tab");
    let attempt_id = match &job.state {
        Some(JobState::StartedUnknown {
            attempt_id,
            activity_recent: true,
        }) => attempt_id.clone(),
        Some(state) if state.is_terminal() => {
            job.feedback = "Finished job — view-only".into();
            return;
        }
        _ => {
            job.feedback =
                "No recent started attempt; steering unavailable, no automatic replay".into();
            return;
        }
    };
    let request = match job.retry.take() {
        Some(request) if request.text == text => request,
        Some(request) => {
            job.retry = Some(request);
            job.feedback = "Prior publication uncertain: resend the same text/ID before submitting different input".into();
            return;
        }
        None => JobInputRequest::new(attempt_id, text),
    };
    let publication = async {
        let session = Session::new(
            chaz_core::types::ConversationId(tab.session_db_id.clone()),
            tab.session_db.clone(),
        )
        .await;
        session.submit_job_input(request.clone()).await
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), publication)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "publication acknowledgement timed out; outcome uncertain"
            ))
        });
    match result {
        Ok(input) => job.feedback = format!("{}: {:?}", input.request.id, input.state),
        Err(error) => {
            job.feedback = format!(
                "Refused or publication uncertain: {error}. Same text retries the same ID."
            );
            job.retry = Some(request);
        }
    }
    refresh_tab(tab).await;
}

pub(super) fn banner(tab: &Tab) -> String {
    let Some(job) = &tab.job else {
        return String::new();
    };
    let mut text = format!(
        "JOB observer (Write-capable credentials) — {:?} (recorded, not proven liveness)\n{}",
        job.state, job.feedback
    );
    for input in &job.inputs {
        text.push_str(&format!(
            "\nInput {}: {}\n{}",
            short_session_id(&input.request.id),
            match &input.state {
                chaz_core::session::steering::JobInputState::Dispatching { .. } =>
                    format!("Accepted; {:?}", input.state),
                state => format!("{state:?}"),
            },
            input.request.text
        ));
    }
    text
}

pub(super) fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let panes = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(3)])
        .split(f.area());
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(panes[0]);
    let nodes = &app.jobs.data.nodes;
    let window = view::ListWindow::uniform(
        nodes.len(),
        1,
        Some(app.jobs.selected),
        columns[0].height.saturating_sub(2) as usize,
        app.jobs.offset,
    );
    app.jobs.offset = window.offset;
    let mut rows = Vec::new();
    for i in window.rows() {
        let node = &nodes[i];
        let mut depth = 0;
        let mut parent = node.parent_id.as_deref();
        while let Some(id) = parent {
            depth += 1;
            if depth > nodes.len() {
                break;
            }
            parent = nodes
                .iter()
                .find(|n| n.session_db_id == id)
                .and_then(|n| n.parent_id.as_deref());
        }
        rows.push(format!(
            "{}{}{} {} {}",
            if i == app.jobs.selected { "> " } else { "  " },
            "  ".repeat(depth.min(8)),
            short_session_id(&node.session_db_id),
            node.agent,
            if node.claimed {
                "claimed"
            } else {
                "context/unclaimed"
            }
        ));
    }
    f.render_widget(
        Paragraph::new(rows.join("\n")).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Jobs ancestry — recorded evidence"),
        ),
        columns[0],
    );
    let mut preview = String::new();
    if let Some(node) = nodes.get(app.jobs.selected) {
        preview = format!(
            "{}\nAgent: {}\nExecutor: {}\nRecorded: {:?}\nParent: {:?}\n{}\n{}",
            node.session_db_id,
            node.agent,
            node.executor,
            node.state,
            node.parent_id,
            node.unavailable
                .as_deref()
                .unwrap_or("local session evidence; not proven liveness"),
            node.task
        );
        for wait in &node.waits {
            preview.push_str(&format!(
                "\nwait -> {} ({}, until {}; attempt {})",
                short_session_id(&wait.child_id),
                if wait.finished {
                    "finished"
                } else {
                    "recorded unfinished"
                },
                wait.deadline_at,
                wait.attempt_id
            ));
        }
        for entry in &node.preview {
            preview.push_str(&format!(
                "\n{:?} {}: {}",
                entry.entry_type, entry.sender, entry.content
            ));
        }
    }
    for source in &app.jobs.data.sources {
        preview.push_str(&format!("\nSource: {source}"));
    }
    f.render_widget(
        Paragraph::new(preview).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Selected job preview"),
        ),
        columns[1],
    );
    let age = app
        .jobs
        .data
        .observed_at
        .map(|t| format!("snapshot {}s ago", (chrono::Utc::now() - t).num_seconds()))
        .unwrap_or("no snapshot".into());
    f.render_widget(
        Paragraph::new(format!(
            "↑↓ select · Enter observe job · r refresh · Esc return\n{}{} {}",
            age,
            if app.jobs.loading {
                " · refreshing"
            } else {
                ""
            },
            app.jobs.error.as_deref().unwrap_or("")
        )),
        panes[1],
    );
}
