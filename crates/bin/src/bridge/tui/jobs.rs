//! Jobs overview and observe-only tabs; no server registration or execution.
use super::*;
use chaz_core::server::{JobMonitor, JobMonitorNode};
use chaz_core::session::{
    jobs::JobState,
    steering::{JobInput, JobInputRequest, JobInputState},
};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Wrap},
};

#[derive(Default)]
pub(super) struct JobsView {
    pub data: JobMonitor,
    pub selected: usize,
    pub offset: usize,
    pub detail_scroll: u16,
    pub details: bool,
    pub loading: bool,
    pub error: Option<String>,
    ticket: u64,
    task: Option<supervised::Task<()>>,
    pub publications:
        std::collections::HashMap<String, Arc<tokio::sync::Mutex<Option<JobInputRequest>>>>,
}

pub(super) struct JobTab {
    pub state: Option<JobState>,
    pub inputs: Vec<JobInput>,
    pub feedback: String,
    pub retry: Arc<tokio::sync::Mutex<Option<JobInputRequest>>>,
    pub details: bool,
}

pub(super) struct Loaded {
    pub ticket: u64,
    pub result: Result<JobMonitor, String>,
}

pub(super) fn cancel(app: &mut App) {
    app.jobs.ticket += 1;
    app.jobs.task = None;
    app.jobs.loading = false;
}

pub(super) fn refresh(app: &mut App, server: &Arc<Server>, tx: &mpsc::Sender<Loaded>) {
    if !app.backend_available || app.jobs.loading {
        return;
    }
    let server = server.clone();
    request_with(app, tx, async move {
        server.job_monitor().await.map_err(|e| e.to_string())
    });
}

fn request_with(
    app: &mut App,
    tx: &mpsc::Sender<Loaded>,
    load: impl Future<Output = Result<JobMonitor, String>> + Send + 'static,
) {
    if app.jobs.loading {
        return;
    }
    app.jobs.loading = true;
    app.jobs.ticket += 1;
    let ticket = app.jobs.ticket;
    let tx = tx.clone();
    app.jobs.task = Some(supervised::Task(tokio::spawn(async move {
        let result = supervised::read(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), load)
                .await
                .unwrap_or_else(|_| Err("refresh timed out; retained snapshot is stale".into()))
        })
        .await;
        let _ = tx.send(Loaded { ticket, result }).await;
    })));
}

pub(super) fn apply_loaded(app: &mut App, loaded: Loaded) {
    if loaded.ticket != app.jobs.ticket {
        return;
    }
    app.jobs.task = None;
    apply(app, loaded.result);
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
                .as_ref()
                .and_then(|id| data.nodes.iter().position(|n| &n.session_db_id == id))
                .unwrap_or(0);
            if data.nodes.get(app.jobs.selected).map(|n| &n.session_db_id) != selected_id.as_ref() {
                app.jobs.detail_scroll = 0;
            }
            app.jobs.data = data;
            app.jobs.error = None;
        }
        Err(error) => app.jobs.error = Some(error),
    }
}

pub(super) async fn observe(
    db: &eidetica::Database,
    tx: mpsc::Sender<String>,
) -> anyhow::Result<Observation> {
    chaz_core::session::jobs::require_observer_write(db).await?;
    let tips = db.snapshot().await?;
    let sid = db.root_id().to_string();
    let callback = db
        .on_write_at_tips(tips, move |_, _| {
            let tx = tx.clone();
            let sid = sid.clone();
            async move {
                let _ = tx.send(sid).await;
                Ok(())
            }
        })
        .await?;
    Ok(Observation {
        id: 0,
        callback: Some(callback),
        forwarder: None,
    })
}

pub(super) async fn open_tab(
    server: &Server,
    backend: &BackendManager,
    id: &str,
    tx: mpsc::Sender<String>,
) -> Result<Tab, String> {
    let (_, db) = server
        .registry()
        .open_job_session(id)
        .await
        .map_err(|e| e.to_string())?;
    let observation = observe(&db, tx).await.map_err(|e| e.to_string())?;
    let mut tab = build_tab(server, backend, db.clone(), id.into()).await?;
    tab.job = Some(JobTab {
        state: None,
        inputs: Vec::new(),
        feedback: String::new(),
        retry: Default::default(),
        details: false,
    });
    tab.live.observation = Some(observation);
    apply_snapshot(&mut tab, snapshot(&db).await);
    Ok(tab)
}

pub(super) fn open(
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    id: String,
    tx: &mpsc::Sender<String>,
) {
    if let Some(index) = app.tab_index_for(&id) {
        app.set_active_tab(index);
        app.mode = TuiMode::Chat;
        return;
    }
    let server = server.clone();
    let backend = backend.clone();
    let tx = tx.clone();
    operations::submit(app, false, None, async move {
        open_tab(&server, &backend, &id, tx)
            .await
            .map(|tab| operations::Effect::Opened(Box::new(tab)))
    });
}

pub(super) struct Snapshot {
    entries: Vec<SessionEntry>,
    state: JobState,
    inputs: Result<Vec<JobInput>, String>,
}

pub(super) async fn snapshot(db: &eidetica::Database) -> Result<Snapshot, String> {
    async {
        chaz_core::session::jobs::require_observer_write(db).await?;
        let tips = db.snapshot().await?;
        let entries = Session::entries_at_snapshot(db, &tips).await?;
        let state = chaz_core::session::jobs::observer_status(db).await?.state;
        let session = Session::new(
            chaz_core::types::ConversationId(db.root_id().to_string()),
            db.clone(),
        )
        .await;
        Ok::<_, anyhow::Error>(Snapshot {
            entries,
            state,
            inputs: session.job_inputs().await.map_err(|e| e.to_string()),
        })
    }
    .await
    .map_err(|e| e.to_string())
}

pub(super) fn apply_snapshot(tab: &mut Tab, result: Result<Snapshot, String>) {
    match result {
        Ok(snapshot) => {
            tab.set_entries(snapshot.entries);
            tab.active_turns = usize::from(matches!(
                snapshot.state,
                JobState::StartedUnknown {
                    activity_recent: true,
                    ..
                }
            ));
            let job = tab.job.as_mut().expect("job snapshot belongs to observer");
            job.state = Some(snapshot.state);
            match snapshot.inputs {
                Ok(inputs) => {
                    job.inputs = inputs;
                    if job.feedback.starts_with("unavailable")
                        || job.feedback.starts_with("input evidence unavailable")
                    {
                        job.feedback.clear();
                    }
                }
                Err(error) => job.feedback = format!("input evidence unavailable: {error}"),
            }
        }
        Err(error) => {
            let job = tab.job.as_mut().expect("job snapshot belongs to observer");
            job.state = None;
            job.feedback = format!("unavailable; retained evidence is stale: {error}");
            tab.touch_transcript();
        }
    }
}

pub(super) async fn send(
    db: eidetica::Database,
    retry: Arc<tokio::sync::Mutex<Option<JobInputRequest>>>,
    text: String,
) -> Result<operations::Effect, String> {
    let status = chaz_core::session::jobs::observer_status(&db)
        .await
        .map_err(|e| e.to_string())?;
    let attempt_id = match status.state {
        JobState::StartedUnknown {
            attempt_id,
            activity_recent: true,
        } => attempt_id,
        state if state.is_terminal() => {
            return Ok(operations::Effect::JobSent(
                "Finished job — view-only".into(),
            ));
        }
        _ => {
            return Ok(operations::Effect::JobSent(
                "No recent started attempt; steering unavailable, no automatic replay".into(),
            ));
        }
    };
    // This state belongs to the accepted publication, not the view. Closing
    // the tab cannot lose its ID or authorize a different uncertain input.
    let mut retry = retry.lock().await;
    let request = match retry.take() {
        Some(request) if request.text == text => request,
        Some(request) => {
            *retry = Some(request);
            return Err("Prior publication uncertain: resend the same text/ID before submitting different input".into());
        }
        None => JobInputRequest::new(attempt_id, text),
    };
    *retry = Some(request.clone());
    let publication = async {
        let session = Session::new(
            chaz_core::types::ConversationId(db.root_id().to_string()),
            db,
        )
        .await;
        session.submit_job_input(request).await
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), publication)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "publication acknowledgement timed out; outcome uncertain"
            ))
        });
    match result {
        Ok(_) => {
            *retry = None;
            Ok(operations::Effect::JobSent(
                "Input published · see current receipts below".into(),
            ))
        }
        Err(error) => Err(format!(
            "Refused or publication uncertain: {error}. Same text retries the same ID."
        )),
    }
}

pub(super) fn toggle_tab_details(tab: &mut Tab) {
    if let Some(job) = tab.job.as_mut() {
        job.details = !job.details;
        tab.touch_transcript();
    }
}

pub(super) fn handle_key(
    app: &mut App,
    key: KeyEvent,
    server: &Arc<Server>,
    backend: &BackendManager,
    notify_tx: &mpsc::Sender<String>,
    jobs_tx: &mpsc::Sender<Loaded>,
) {
    match key.code {
        KeyCode::Esc => {
            app.mode = TuiMode::Chat;
            app.ensure_view();
        }
        KeyCode::Up => {
            let selected = app.jobs.selected.saturating_sub(1);
            select(&mut app.jobs, selected);
        }
        KeyCode::Down => {
            let selected = app.jobs.selected + 1;
            select(&mut app.jobs, selected);
        }
        KeyCode::PageUp => app.jobs.detail_scroll = app.jobs.detail_scroll.saturating_sub(5),
        KeyCode::PageDown => app.jobs.detail_scroll = app.jobs.detail_scroll.saturating_add(5),
        KeyCode::Home => app.jobs.detail_scroll = 0,
        KeyCode::End => app.jobs.detail_scroll = u16::MAX,
        KeyCode::Char('d') => {
            app.jobs.details = !app.jobs.details;
            app.jobs.detail_scroll = 0;
        }
        KeyCode::Char('r') => refresh(app, server, jobs_tx),
        KeyCode::Enter => {
            if let Some(node) = app
                .jobs
                .data
                .nodes
                .get(app.jobs.selected)
                .filter(|node| node.state.is_some())
            {
                let id = node.session_db_id.clone();
                open(app, server, backend, id, notify_tx);
            }
        }
        _ => {}
    }
}

// Labels describe persisted evidence, never turn a claim into liveness.
fn state_label(state: Option<&JobState>) -> &'static str {
    match state {
        Some(JobState::Pending) => "Pending · awaiting acceptance",
        Some(JobState::Queued) => "Queued",
        Some(JobState::Running { .. }) => "Started · executor reported active",
        Some(JobState::StartedUnknown {
            activity_recent: true,
            ..
        }) => "Started · recent activity",
        Some(JobState::StartedUnknown {
            activity_recent: false,
            ..
        }) => "Started · status uncertain",
        Some(JobState::Interrupted { .. }) => "Interrupted · explicit retry required",
        Some(JobState::Succeeded { .. }) => "Completed",
        Some(JobState::Failed { .. }) => "Failed",
        Some(JobState::Rejected { .. }) => "Rejected",
        None => "Unavailable · status unknown",
    }
}

fn state_style(state: Option<&JobState>) -> Style {
    match state {
        Some(JobState::Succeeded { .. }) => theme::accent(),
        Some(JobState::Failed { .. } | JobState::Rejected { .. }) | None => theme::error(),
        Some(
            JobState::StartedUnknown {
                activity_recent: false,
                ..
            }
            | JobState::Interrupted { .. },
        ) => Style::default().fg(theme::SYSTEM),
        _ => Style::default(),
    }
}

fn input_label(state: &JobInputState) -> String {
    match state {
        JobInputState::Queued => "Queued · published, awaiting acceptance".into(),
        JobInputState::Accepted => "Accepted · awaiting next call".into(),
        JobInputState::Dispatching { model_sequence } => {
            format!("Accepted · dispatch recorded for call {model_sequence}; inclusion unconfirmed")
        }
        JobInputState::Included { model_sequence } => {
            format!("Included · call {model_sequence} returned")
        }
        JobInputState::NotApplied { reason } => format!("Not applied · {reason}"),
        JobInputState::Uncertain { reason } => format!("Uncertain · {reason}"),
    }
}

pub(super) fn composer_title(tab: &Tab) -> &'static str {
    match tab.job.as_ref().and_then(|job| job.state.as_ref()) {
        Some(state) if state.is_terminal() => " Finished job · view-only ",
        Some(JobState::StartedUnknown {
            activity_recent: true,
            ..
        }) => " > next-call input ",
        _ => " Steering unavailable · no recent attempt ",
    }
}

pub(super) fn banner(tab: &Tab) -> String {
    let Some(job) = &tab.job else {
        return String::new();
    };
    let feedback = if job.feedback.starts_with("unavailable")
        || job.feedback.starts_with("input evidence unavailable")
    {
        "Unavailable · retained evidence is stale; F2 details"
    } else if job.feedback.starts_with("Refused or publication uncertain") {
        "Publication refused or uncertain · resend same text; F2 details"
    } else {
        &job.feedback
    };
    format!(
        "Job observer · {}\nWrite-capable · recorded, not proven live · Ctrl+G jobs · F2 details\n{}",
        state_label(job.state.as_ref()),
        if !feedback.is_empty() {
            feedback
        } else if job.state.as_ref().is_some_and(JobState::is_terminal) {
            "Finished job · view-only"
        } else {
            "Steering waits for the next safe call; no interruption"
        }
    )
}

pub(super) fn input_lines(tab: &Tab) -> Vec<Line<'static>> {
    let Some(job) = &tab.job else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    if job.details {
        lines.push(Line::styled("Observer details", theme::accent_bold()));
        lines.push(Line::from(format!("Session: {}", tab.session_db_id)));
        if let Some(id) = attempt_id(job.state.as_ref()) {
            lines.push(Line::from(format!("Attempt: {id}")));
        }
        lines.push(Line::from(job.feedback.clone()));
    }
    for (i, input) in job.inputs.iter().enumerate() {
        lines.push(Line::styled(
            format!("Input {} · {}", i + 1, input_label(&input.state)),
            theme::accent(),
        ));
        lines.push(Line::from(input.request.text.clone()));
        if job.details {
            lines.push(Line::from(format!(
                "Request: {} · attempt: {}",
                input.request.id, input.request.attempt_id
            )));
        }
    }
    lines
}

fn attempt_id(state: Option<&JobState>) -> Option<&str> {
    match state {
        Some(
            JobState::Running { attempt_id }
            | JobState::StartedUnknown { attempt_id, .. }
            | JobState::Interrupted { attempt_id },
        ) => Some(attempt_id),
        _ => None,
    }
}

fn node_title(node: &JobMonitorNode) -> String {
    if !node.task.trim().is_empty() {
        return node.task.clone();
    }
    if !node.claimed
        && let Some(entry) = node
            .preview
            .iter()
            .find(|e| matches!(e.entry_type, EntryType::Message | EntryType::Directive))
    {
        return entry.content.clone();
    }
    if node.unavailable.is_some() {
        "Unavailable task".into()
    } else if node.claimed {
        "Untitled task".into()
    } else {
        "Originating conversation / referenced child".into()
    }
}

pub(super) fn tab_title(tab: &Tab) -> String {
    tab.entries
        .iter()
        .find(|e| matches!(e.entry_type, EntryType::Directive))
        .map(|e| e.content.clone())
        .unwrap_or_else(|| "Observed job".into())
}

// Clip graphemes by terminal cells, not UTF-8 bytes or codepoints.
pub(super) fn clip(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.width() <= width {
        return text;
    }
    let mut cells = 0;
    let mut out: String = text
        .graphemes(true)
        .take_while(|g| {
            cells += g.width();
            cells <= width.saturating_sub(1)
        })
        .collect();
    if width > 0 {
        out.push('…');
    }
    out
}

fn node_status(node: &JobMonitorNode) -> String {
    if node.unavailable.is_some() {
        format!(
            "Unavailable · last recorded: {}",
            state_label(node.state.as_ref())
        )
    } else if !node.claimed {
        match node.state.as_ref() {
            Some(state) => format!("Context · unclaimed · {}", state_label(Some(state))),
            None => "Context · not a job".into(),
        }
    } else {
        state_label(node.state.as_ref()).into()
    }
}

fn relationship_title(nodes: &[JobMonitorNode], id: &str) -> String {
    nodes
        .iter()
        .find(|n| n.session_db_id == id)
        .map(|n| clip(&node_title(n), 70))
        .unwrap_or_else(|| "Unavailable reference".into())
}

fn detail_lines(jobs: &JobsView, node: &JobMonitorNode) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if jobs.details {
        // Technical view is explicit; no Debug formatting, even here.
        lines.extend([
            Line::styled(node_title(node), theme::accent_bold()),
            Line::from(format!("Session: {}", node.session_db_id)),
            Line::from(format!("Executor: {}", node.executor)),
            Line::from(format!(
                "Projection: {}",
                if node.claimed {
                    "claimed job"
                } else {
                    "context reference (excluded from job counts)"
                }
            )),
        ]);
        if let Some(id) = &node.parent_id {
            lines.push(Line::from(format!("Parent: {id}")));
        }
        if let Some(id) = attempt_id(node.state.as_ref()) {
            lines.push(Line::from(format!("Attempt: {id}")));
        }
        if let Some(error) = &node.unavailable {
            lines.push(Line::from(error.clone()));
        }
        if let Some(error) = &jobs.error {
            lines.push(Line::from(error.clone()));
        }
        for wait in &node.waits {
            lines.push(Line::from(format!(
                "Wait child: {} · attempt: {} · deadline: {} · finished: {}",
                wait.child_id, wait.attempt_id, wait.deadline_at, wait.finished
            )));
        }
        for source in &jobs.data.sources {
            lines.push(Line::from(format!("Source: {source}")));
        }
    } else {
        lines.push(Line::styled(
            node_title(node),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        if node.claimed {
            lines.push(Line::styled(format!("Agent: {}", node.agent), theme::dim()));
        }
        if let Some(parent) = &node.parent_id {
            lines.push(Line::from(format!(
                "From: {}",
                relationship_title(&jobs.data.nodes, parent)
            )));
        }
        let children = jobs
            .data
            .nodes
            .iter()
            .filter(|n| n.parent_id.as_deref() == Some(&node.session_db_id))
            .collect::<Vec<_>>();
        if !children.is_empty() {
            lines.push(Line::from(format!(
                "Children ({}): {}",
                children.len(),
                children
                    .iter()
                    .map(|n| clip(&node_title(n), 50))
                    .collect::<Vec<_>>()
                    .join(" · ")
            )));
        }
        for wait in &node.waits {
            lines.push(Line::from(format!(
                "Wait {}: {}{}",
                if wait.finished {
                    "finished"
                } else {
                    "recorded (unfinished)"
                },
                relationship_title(&jobs.data.nodes, &wait.child_id),
                if wait.finished {
                    ""
                } else {
                    " · current wait not proven"
                }
            )));
        }
        match &node.state {
            Some(JobState::Succeeded { text }) => {
                lines.push(Line::styled("Result", theme::accent_bold()));
                lines
                    .push(Line::from(text.clone().unwrap_or_else(|| {
                        "Completed · no result text recorded".into()
                    })));
            }
            Some(JobState::Failed { message } | JobState::Rejected { message }) => {
                lines.push(Line::styled("Outcome", theme::error()));
                lines.push(Line::from(message.clone()));
            }
            _ => {}
        }
        if node.unavailable.is_some() {
            lines.push(Line::styled(
                "Source unavailable · retained evidence only; d for diagnostics",
                theme::error(),
            ));
        }
        if !node.preview.is_empty() {
            lines.push(Line::styled("Recent activity", theme::dim()));
        }
    }
    for entry in &node.preview {
        if jobs.details {
            lines.push(Line::from(format!(
                "{} · {}",
                entry.timestamp, entry.content
            )));
            continue;
        }
        let activity = match &entry.entry_type {
            EntryType::ToolCall => format!(
                "Tool requested: {}",
                view::summarize_tool_call(&entry.content).0
            ),
            EntryType::ToolResult => {
                let (name, summary, error) = view::summarize_tool_result(&entry.content);
                format!(
                    "Tool {}: {name} · {}",
                    if error { "failed" } else { "result" },
                    if summary.starts_with('{') || summary.starts_with('[') {
                        "output recorded (d details)"
                    } else {
                        &summary
                    }
                )
            }
            EntryType::Message => format!("{}: {}", entry.sender, entry.content),
            EntryType::Error => format!("Error: {}", entry.content),
            EntryType::Directive => "Original task recorded".into(),
            EntryType::Ack => "Turn acknowledged".into(),
            EntryType::Summary => format!("Summary: {}", entry.content),
            _ => "Audit event recorded".into(),
        };
        lines.push(Line::from(activity));
    }
    lines
}

pub(super) fn select(jobs: &mut JobsView, selected: usize) {
    let selected = selected.min(jobs.data.nodes.len().saturating_sub(1));
    if jobs.selected != selected {
        jobs.detail_scroll = 0;
    }
    jobs.selected = selected;
}

pub(super) fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let jobs = &mut app.jobs;
    let nodes = &jobs.data.nodes;
    let total = nodes.iter().filter(|n| n.claimed).count();
    let finished = nodes
        .iter()
        .filter(|n| n.claimed && n.state.as_ref().is_some_and(JobState::is_terminal))
        .count();
    let queued = nodes
        .iter()
        .filter(|n| n.claimed && matches!(n.state, Some(JobState::Pending | JobState::Queued)))
        .count();
    let counts = format!(
        "{total} jobs · {finished} finished · {queued} queued · {} unresolved · {} context",
        total.saturating_sub(finished + queued),
        nodes.len() - total
    );
    let panes = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(f.area());
    widgets::header(f, panes[0], "Jobs", Some(&counts), None);
    let age = jobs
        .data
        .observed_at
        .map(|t| {
            format!(
                "Snapshot {}s ago",
                (chrono::Utc::now() - t).num_seconds().max(0)
            )
        })
        .unwrap_or_else(|| "No snapshot".into());
    let gaps = jobs
        .data
        .sources
        .iter()
        .filter(|s| s.contains("unavailable"))
        .count();
    let freshness = if jobs.error.is_some() {
        format!(" Stale · refresh failed/timed out · {age} · d diagnostics")
    } else if jobs.loading {
        format!(" Refreshing · {age}")
    } else {
        format!(" {age} · {gaps} sources unavailable · recorded, not proven live")
    };
    f.render_widget(
        Paragraph::new(freshness).style(if jobs.error.is_some() {
            theme::error()
        } else {
            theme::dim()
        }),
        panes[1],
    );
    let columns = if panes[2].width >= 110 {
        Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)]).split(panes[2])
    } else {
        Layout::vertical([Constraint::Percentage(35), Constraint::Percentage(65)]).split(panes[2])
    };
    let row_height = if columns[0].height >= 6 { 2 } else { 1 };
    let window = view::ListWindow::uniform(
        nodes.len(),
        row_height,
        Some(jobs.selected),
        columns[0].height.saturating_sub(2) as usize,
        jobs.offset,
    );
    jobs.offset = window.offset;
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
        let indent = "  ".repeat(depth.min(4));
        let selected = i == jobs.selected;
        let style = if selected {
            theme::selected()
        } else if !node.claimed {
            theme::dim()
        } else {
            Style::default()
        };
        let prefix = format!(
            "{}{}{}",
            if selected { "› " } else { "  " },
            indent,
            if depth > 0 { "↳ " } else { "" }
        );
        let title = format!("{prefix}{}", node_title(node));
        rows.push(Line::styled(
            clip(&title, columns[0].width.saturating_sub(2) as usize),
            style,
        ));
        if row_height == 2 {
            let label = if node.claimed {
                format!("{} · {}", node_status(node), node.agent)
            } else {
                node_status(node)
            };
            rows.push(Line::styled(
                clip(
                    &format!("  {indent}{label}"),
                    columns[0].width.saturating_sub(2) as usize,
                ),
                if selected {
                    theme::selected()
                } else {
                    state_style(node.state.as_ref())
                },
            ));
        }
    }
    if nodes.is_empty() {
        rows.push(Line::from(if jobs.error.is_some() {
            "Jobs unavailable · d diagnostics"
        } else if jobs.loading {
            "Loading jobs…"
        } else {
            "No claimed jobs in this snapshot"
        }));
    }
    let block = |title: String| {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::dim())
            .title(Span::styled(title, theme::accent()))
    };
    f.render_widget(
        Paragraph::new(rows).block(block(format!(
            " Tasks · {}/{} ",
            usize::from(!nodes.is_empty()) * (jobs.selected + 1),
            nodes.len()
        ))),
        columns[0],
    );
    let (status, lines) = if let Some(node) = nodes.get(jobs.selected) {
        (node_status(node), detail_lines(jobs, node))
    } else {
        let mut lines = vec![Line::from("Select a task to inspect recorded evidence.")];
        if jobs.details {
            lines.extend(jobs.data.sources.iter().cloned().map(Line::from));
            if let Some(error) = &jobs.error {
                lines.push(Line::from(error.clone()));
            }
        }
        ("Selected task".into(), lines)
    };
    let title = format!(
        " {} · {} ",
        if jobs.details { "Details" } else { "Task" },
        status
    );
    let detail_block = block(clip(&title, columns[1].width.saturating_sub(2) as usize));
    let inner = detail_block.inner(columns[1]);
    f.render_widget(detail_block, columns[1]);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width)
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    jobs.detail_scroll = jobs.detail_scroll.min(max_scroll);
    f.render_widget(paragraph.scroll((jobs.detail_scroll, 0)), inner);
    let help = if f.area().width >= 78 {
        " ↑↓ select · Enter observe · PgUp/PgDn read · d details · r refresh · Esc back"
    } else {
        " ↑↓ select · Enter open · Pg↑↓ read · d details · r refresh"
    };
    widgets::status_strip(
        f,
        panes[3],
        &format!("{}{help}", app.operations.indicator()),
    );
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
