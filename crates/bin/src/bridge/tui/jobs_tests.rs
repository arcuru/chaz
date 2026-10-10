//! Rendered behavior, not backend authority: service/PTY tests cover real evidence.
use super::*;
use chaz_core::server::{JobMonitor, JobMonitorNode};
use chaz_core::session::jobs::{JobState, JobWait};
use ratatui::{Terminal, backend::TestBackend};

fn node(id: &str, task: &str, state: Option<JobState>) -> JobMonitorNode {
    JobMonitorNode {
        session_db_id: id.into(),
        parent_id: None,
        agent: "scout".into(),
        executor: "ed25519:diagnostic-key".into(),
        claimed: true,
        state,
        unavailable: None,
        task: task.into(),
        waits: vec![],
        preview: vec![],
    }
}

fn app(nodes: Vec<JobMonitorNode>) -> App {
    let mut app = App::hub(HashSet::new());
    app.jobs.data = JobMonitor {
        nodes,
        sources: vec![],
        observed_at: Some(chrono::Utc::now()),
    };
    app.mode = TuiMode::Jobs;
    app
}

fn frame(app: &mut App, w: u16, h: u16) -> (String, ratatui::buffer::Buffer) {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| draw(f, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let text = (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    println!("{w}x{h}:\n{text}");
    (text, buffer)
}

fn no_diagnostics(screen: &str) {
    for diagnostic in [
        "Some(",
        "StartedUnknown",
        "Succeeded {",
        "attempt_id",
        "activity_recent",
        "bafyr4i",
        "ed25519:",
        "claimed job",
        "diagnostic-attempt",
    ] {
        assert!(
            !screen.contains(diagnostic),
            "default leaked {diagnostic}:\n{screen}"
        );
    }
}

#[test]
fn jobs_render_task_status_result_across_terminal_sizes() {
    for (state, label, outcome) in [
        (JobState::Queued, "Queued", None),
        (
            JobState::Succeeded {
                text: Some("Use the smaller example first.".into()),
            },
            "Completed",
            Some("Use the smaller example first."),
        ),
        (
            JobState::StartedUnknown {
                attempt_id: "diagnostic-attempt".into(),
                activity_recent: true,
            },
            "Started · recent activity",
            None,
        ),
        (
            JobState::StartedUnknown {
                attempt_id: "diagnostic-attempt".into(),
                activity_recent: false,
            },
            "Started · status uncertain",
            None,
        ),
        (
            JobState::Failed {
                message: "Tool limit reached".into(),
            },
            "Failed",
            Some("Tool limit reached"),
        ),
        (
            JobState::Rejected {
                message: "Scope refused".into(),
            },
            "Rejected",
            Some("Scope refused"),
        ),
        (
            JobState::Interrupted {
                attempt_id: "diagnostic-attempt".into(),
            },
            "Interrupted",
            None,
        ),
    ] {
        let mut app = app(vec![node(
            "bafyr4i-diagnostic-id",
            "Compare the migration options",
            Some(state),
        )]);
        for (w, h) in [(80, 24), (120, 35), (180, 45)] {
            let (text, buffer) = frame(&mut app, w, h);
            assert!(text.contains("Compare the migration options"));
            assert!(text.contains(label), "{text}");
            assert!(text.contains("1 jobs"));
            if let Some(outcome) = outcome {
                assert!(text.contains(outcome), "{text}");
            }
            no_diagnostics(&text);
            let selected = buffer.content.iter().find(|c| c.symbol() == "›").unwrap();
            assert_eq!(selected.bg, theme::ACCENT);
            assert!(selected.modifier.contains(ratatui::style::Modifier::BOLD));
        }
    }
}

#[test]
fn jobs_context_counts_titles_and_wait_uncertainty_are_separate() {
    let mut origin = node("origin-id", "Plan the migration", None);
    origin.claimed = false;
    let mut parent = node(
        "bafyr4i-parent",
        "Compare storage choices",
        Some(JobState::StartedUnknown {
            attempt_id: "diagnostic-attempt".into(),
            activity_recent: true,
        }),
    );
    parent.parent_id = Some(origin.session_db_id.clone());
    let mut child = node("child-id", "Measure SQLite latency", Some(JobState::Queued));
    child.parent_id = Some(parent.session_db_id.clone());
    child.claimed = false;
    parent.waits.push(JobWait {
        attempt_id: "diagnostic-attempt".into(),
        child_id: child.session_db_id.clone(),
        deadline_at: chrono::Utc::now(),
        finished: false,
    });
    let mut app = app(vec![origin, parent, child]);
    select(&mut app.jobs, 1);
    let (text, _) = frame(&mut app, 120, 35);
    assert!(text.contains("1 jobs · 0 finished · 0 queued · 1 unresolved · 2 context"));
    assert!(text.contains("From: Plan the migration"));
    assert!(text.contains("Children (1): Measure SQLite latency"));
    assert!(text.contains("Wait recorded (unfinished): Measure SQLite latency"));
    assert!(text.contains("current") && text.contains("wait not proven"));
    assert!(text.contains("Context · not a job"));
    assert!(
        text.find("Plan the migration").unwrap() < text.find("Compare storage choices").unwrap()
    );
    no_diagnostics(&text);
    app.jobs.data.nodes[1].waits[0].finished = true;
    assert!(
        frame(&mut app, 120, 35)
            .0
            .contains("Wait finished: Measure SQLite latency")
    );
}

#[test]
fn jobs_unavailable_stale_empty_and_details_are_distinct() {
    let mut unavailable = node(
        "bafyr4i-unavailable",
        "Check deployment outcome",
        Some(JobState::Queued),
    );
    unavailable.unavailable = Some("native Write query refused diagnostic-key".into());
    let mut app = app(vec![unavailable]);
    app.jobs
        .data
        .sources
        .push("remote-key: unavailable (not fetched)".into());
    let text = frame(&mut app, 80, 24).0;
    assert!(text.contains("Unavailable · last recorded: Queued"));
    assert!(text.contains("1 sources unavailable"));
    assert!(!text.contains("native Write"));
    app.jobs.error = Some("refresh timed out diagnostic-key".into());
    let text = frame(&mut app, 80, 24).0;
    assert!(text.contains("Stale · refresh failed/timed out"));
    assert!(text.contains("Check deployment outcome"));
    no_diagnostics(&text);
    app.jobs.details = true;
    let text = frame(&mut app, 120, 35).0;
    for expected in [
        "Details",
        "Session: bafyr4i-unavailable",
        "native Write query refused",
        "Source: remote-key:",
        "refresh timed out diagnostic-key",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    app.jobs.data.nodes.clear();
    app.jobs.details = false;
    assert!(frame(&mut app, 80, 24).0.contains("Jobs unavailable"));
    app.jobs.error = None;
    assert!(frame(&mut app, 80, 24).0.contains("No claimed jobs"));
    app.jobs.loading = true;
    assert!(frame(&mut app, 80, 24).0.contains("Loading jobs"));
}

#[test]
fn jobs_long_unicode_scroll_selection_refresh_and_resize() {
    let long = format!("調査 e\u{301} 👩‍💻 {} 最終行", "非常に長い説明 ".repeat(80));
    let nodes = (0..20)
        .map(|i| {
            node(
                &format!("bafyr4i-{i}"),
                &format!("Task {i}"),
                Some(JobState::Queued),
            )
        })
        .collect();
    let mut app = app(nodes);
    app.jobs.data.nodes[19].task = long;
    select(&mut app.jobs, 19);
    for (w, h) in [(80, 24), (120, 35), (180, 45), (40, 8), (1, 1), (80, 24)] {
        let text = frame(&mut app, w, h).0;
        assert_eq!(app.jobs.selected, 19);
        if w >= 80 {
            assert!(text.replace(' ', "").contains("調査"), "{text}");
        }
    }
    app.jobs.detail_scroll = u16::MAX;
    assert!(
        frame(&mut app, 80, 24)
            .0
            .replace(' ', "")
            .contains("最終行")
    );
    assert!(app.jobs.detail_scroll < u16::MAX);
    let updated = app.jobs.data.clone();
    super::apply(&mut app, Ok(updated));
    assert_eq!(app.jobs.selected, 19);
    assert!(app.jobs.detail_scroll > 0);
    select(&mut app.jobs, 0);
    assert_eq!(app.jobs.detail_scroll, 0);
    assert!(frame(&mut app, 80, 24).0.contains("Task 0"));
    let clipped = clip("界e\u{301}👩‍💻界", 5);
    assert_eq!(clipped, "界e\u{301}…");
    assert_eq!(clip("界", 0), "");
}

#[test]
fn jobs_input_labels_distinguish_publication_dispatch_inclusion() {
    use chaz_core::session::steering::JobInputState;
    for (state, expected) in [
        (
            JobInputState::Queued,
            "Queued · published, awaiting acceptance",
        ),
        (JobInputState::Accepted, "Accepted · awaiting next call"),
        (
            JobInputState::Dispatching { model_sequence: 7 },
            "Accepted · dispatch recorded for call 7; inclusion unconfirmed",
        ),
        (
            JobInputState::Included { model_sequence: 7 },
            "Included · call 7 returned",
        ),
        (
            JobInputState::NotApplied {
                reason: "executor closed input".into(),
            },
            "Not applied · executor closed input",
        ),
        (
            JobInputState::Uncertain {
                reason: "no automatic replay".into(),
            },
            "Uncertain · no automatic replay",
        ),
    ] {
        assert_eq!(input_label(&state), expected);
        assert!(!input_label(&state).contains('{'));
    }
}
