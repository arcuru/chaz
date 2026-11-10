//! Real native snapshots, transcript buffers and input/copy paths; no providers.
use super::transcript_tests::{app_with, entry, reference, rows};
use super::*;
use crate::bridge::tui::{input, refresh, tool_activity::ToolRow};
use chaz_core::runtime::{ToolCallRequest, ToolResultOutcome};
use chaz_core::session::{
    Session, TurnAttempt, TurnCallIdentity, TurnRequestId, TurnTranscriptMessage,
    TurnTranscriptRecord,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use eidetica::store::Table;
use ratatui::{Terminal, backend::TestBackend};

fn record(
    attempt: &TurnAttempt,
    sequence: u64,
    message: TurnTranscriptMessage,
) -> TurnTranscriptRecord {
    TurnTranscriptRecord {
        request_id: attempt.request_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        sequence,
        timestamp: attempt.started_at + chrono::Duration::microseconds(sequence as i64),
        identity: Some(TurnCallIdentity {
            agent: "chaz".into(),
            requested_model: "fixture".into(),
            execution_id: None,
        }),
        message,
    }
}

fn model(model_sequence: u64, count: usize, prose: &str) -> TurnTranscriptMessage {
    TurnTranscriptMessage::ModelResponse {
        model_sequence,
        content: Some(prose.into()),
        tool_calls: (0..count)
            .map(|i| ToolCallRequest {
                id: format!("call-{i}"),
                name: "query".into(),
                arguments: format!(r#"{{"path":"file-{model_sequence}-{i}","extra":true}}"#),
            })
            .collect(),
        provider_extra: Default::default(),
        metadata: None,
        terminal: false,
    }
}

/// Use the actual native writer; the expected display envelope mirrors the
/// server boundary and is checked after reopening by the real snapshot loader.
async fn append(session: &Session, record: TurnTranscriptRecord) {
    let agent = record.identity.as_ref().unwrap().agent.as_str();
    let mut display = match &record.message {
        TurnTranscriptMessage::ModelResponse {
            content,
            tool_calls,
            terminal: false,
            ..
        } => {
            let mut rows = Vec::new();
            if let Some(text) = content.as_ref().filter(|s| !s.is_empty()) {
                rows.push(entry(agent, text, EntryType::TurnContext));
            }
            rows.extend(tool_calls.iter().map(|c| {
                entry(
                    agent,
                    &format!("{}({})", c.name, c.arguments),
                    EntryType::ToolCall,
                )
            }));
            rows
        }
        TurnTranscriptMessage::ToolResult {
            outcome: ToolResultOutcome::Unknown { .. },
            ..
        } => Vec::new(),
        TurnTranscriptMessage::ToolResult {
            name,
            output,
            outcome,
            ..
        } => vec![entry(
            agent,
            &if *outcome == ToolResultOutcome::Success {
                format!("{name}: {}", chaz_core::util::truncate_chars(output, 500))
            } else {
                format!("{name}: ERROR: {output}")
            },
            EntryType::ToolResult,
        )],
        _ => Vec::new(),
    };
    for row in &mut display {
        row.timestamp = record.timestamp;
    }
    if record.identity.as_ref().unwrap().execution_id.is_none() {
        session
            .append_turn_transcript(record, display)
            .await
            .unwrap();
    } else {
        // Branch writing is core-private; seed its documented native table
        // shape without opening a provider/executor or changing the core API.
        let txn = session.database().new_transaction().await.unwrap();
        let branch = record
            .identity
            .as_ref()
            .unwrap()
            .execution_id
            .as_deref()
            .unwrap();
        for (index, row) in display.into_iter().enumerate() {
            txn.get_store::<Table<chaz_core::session::SessionEntry>>("entries")
                .await
                .unwrap()
                .set(
                    format!(
                        "{}:{branch}:{:020}:{index:010}",
                        record.attempt_id, record.sequence
                    ),
                    row,
                )
                .await
                .unwrap();
        }
        txn.get_store::<Table<TurnTranscriptRecord>>("turn_transcript")
            .await
            .unwrap()
            .set(
                format!("{}:{branch}:{}", record.attempt_id, record.sequence),
                record,
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
}

fn result(
    model_sequence: u64,
    index: usize,
    output: &str,
    outcome: ToolResultOutcome,
) -> TurnTranscriptMessage {
    TurnTranscriptMessage::ToolResult {
        model_sequence,
        call_index: index,
        call_id: format!("call-{index}"),
        name: "query".into(),
        output: output.into(),
        outcome,
    }
}

async fn reload(app: &mut App) {
    let id = app.active().session_db_id.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    refresh::request(app, &id, refresh::RefreshKind::Session, &tx);
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    refresh::apply(app, msg, |_, _| refresh::Resolved {
        effective_model: "fixture".into(),
        context_budget: 0,
        roster: Vec::new(),
    });
}

fn draw(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    app.click_regions.clear();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| ui_chat(f, app, &[])).unwrap();
    terminal
}

fn text(app: &App) -> String {
    app.active()
        .live
        .render
        .lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn click(app: &mut App, region: ClickRegion, right: bool) {
    input::handle_mouse(
        app,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            region.x + if right { region.w - 1 } else { 0 },
            region.y,
        ),
    );
}

#[tokio::test]
async fn nested_tool_compact_height_is_constant_live_completed_and_reopened() {
    let mut heights = std::collections::BTreeMap::<u16, Vec<usize>>::new();
    let mut settled_heights = std::collections::BTreeMap::<u16, Vec<usize>>::new();
    for count in [2, 48, 96] {
        let (_instance, mut app) = app_with(Vec::new()).await;
        let db = app.active().session_db.clone();
        let mut session =
            Session::new(chaz_core::types::ConversationId("tools".into()), db.clone()).await;
        let attempt = session
            .start_turn_attempt(TurnRequestId::parse("request"))
            .await
            .unwrap();
        append(
            &session,
            record(
                &attempt,
                0,
                model(0, count / 2, "I will inspect these files."),
            ),
        )
        .await;
        append(
            &session,
            record(
                &attempt,
                1,
                model(1, count / 2, "First checks are back; looking further."),
            ),
        )
        .await;
        reload(&mut app).await;
        assert!(app.active().live.error.is_none());
        for width in [3, 20, 40, 80] {
            let terminal = draw(&mut app, width, 60);
            let cache = &app.active().live.render;
            assert_eq!(cache.clicks.len(), 2, "only the two run summary rows");
            for &(line, _) in &cache.clicks {
                assert_eq!(
                    cache.offsets[line + 1] - cache.offsets[line],
                    1,
                    "hard-clipped summary"
                );
            }
            heights
                .entry(width)
                .or_default()
                .push(*cache.offsets.last().unwrap());
            let source = text(&app);
            assert!(source.contains("I will inspect"));
            assert!(source.contains("First checks are back"));
            assert!(!source.contains("file-0-0"));
            if count == 48 && width == 40 {
                println!(
                    "ACTUAL LIVE COMPACT {width}:\n{}",
                    rows(&terminal, 0, 60).join("\n")
                );
            }
        }
        // Parallel calls return in reverse order, across both model exchanges.
        for model_sequence in (0..2).rev() {
            for index in (0..count / 2).rev() {
                let seq = 2 + (1 - model_sequence) * (count / 2) + count / 2 - index;
                let output = format!(
                    "result-{model_sequence}-{index} {} END-{model_sequence}-{index}",
                    "payload ".repeat(120)
                );
                append(
                    &session,
                    record(
                        &attempt,
                        seq as u64,
                        result(
                            model_sequence as u64,
                            index,
                            &output,
                            ToolResultOutcome::Success,
                        ),
                    ),
                )
                .await;
            }
        }
        // A terminal model observation must not leak unpublished final prose.
        let terminal = TurnTranscriptMessage::ModelResponse {
            model_sequence: 2,
            content: Some("NOT PUBLISHED".into()),
            tool_calls: Vec::new(),
            provider_extra: Default::default(),
            metadata: None,
            terminal: true,
        };
        // Construct separately rather than publishing a Message with the record.
        session
            .append_turn_transcript(record(&attempt, 200, terminal), Vec::new())
            .await
            .unwrap();
        reload(&mut app).await;
        draw(&mut app, 80, 60);
        assert!(!text(&app).contains("NOT PUBLISHED"));
        assert_eq!(app.active().live.tool_activity.items.len(), count);
        for item in &app.active().live.tool_activity.items {
            let result = item.result.as_ref().unwrap();
            assert!(result.output.ends_with(&format!(
                "END-{}-{}",
                item.key.model_sequence, item.key.index
            )));
            assert!(result.output.len() > 500);
        }
        let mut final_entry = entry("chaz", "Final answer remains visible.", EntryType::Message);
        final_entry.timestamp = attempt.started_at + chrono::Duration::seconds(1);
        session
            .complete_turn_attempt(&attempt, Some(final_entry))
            .await
            .unwrap();
        reload(&mut app).await;
        for width in [3, 20, 40, 80] {
            draw(&mut app, width, 60);
            settled_heights
                .entry(width)
                .or_default()
                .push(*app.active().live.render.offsets.last().unwrap());
        }
        assert!(text(&app).contains("Final answer remains visible."));
        assert!(text(&app).contains(&format!("{count} done")));
        assert!(text(&app).contains("attempt completed"));
        let run_key = app.active().live.tool_activity.runs[0].key.clone();
        let item_key = app.active().live.tool_activity.items[0].key.clone();
        app.active_mut().live.expanded_runs.insert(run_key);
        app.active_mut().live.expanded_tools.insert(item_key);
        app.active_mut().touch_transcript();
        draw(&mut app, 80, 60);
        assert_eq!(
            text(&app).matches("END-0-0").count(),
            1,
            "expansion must render the native result suffix beyond the 500-character audit preview, once"
        );
        let snapshot = refresh::load_session_snapshot("tools", db.clone())
            .await
            .unwrap();
        assert_eq!(snapshot.tool_activity.items.len(), count);
        assert_eq!(snapshot.tool_activity.runs.len(), 1);
        assert_eq!(
            snapshot
                .tool_activity
                .rows
                .values()
                .filter(|r| matches!(r, ToolRow::Result))
                .count(),
            count
        );
        // Reopening reconstructs exactly the same result associations, not previews.
        for (a, b) in snapshot
            .tool_activity
            .items
            .iter()
            .zip(&app.active().live.tool_activity.items)
        {
            assert_eq!(a.key, b.key);
            assert_eq!(
                a.result.as_ref().unwrap().output,
                b.result.as_ref().unwrap().output
            );
        }
    }
    for (width, samples) in heights.into_iter().chain(settled_heights) {
        assert!(
            samples.windows(2).all(|w| w[0] == w[1]),
            "width {width}: compact height grew: {samples:?}"
        );
    }
}

#[tokio::test]
async fn nested_tool_run_identity_separates_attempt_branch_agent_and_request() {
    let (_instance, mut app) = app_with(Vec::new()).await;
    let db = app.active().session_db.clone();
    let session = Session::new(chaz_core::types::ConversationId("tools".into()), db).await;
    let first = session
        .start_turn_attempt(TurnRequestId::parse("request-one"))
        .await
        .unwrap();
    let second = session
        .start_turn_attempt(TurnRequestId::parse("request-two"))
        .await
        .unwrap();
    for (n, (attempt, branch, agent)) in [
        (&first, None, "chaz"),
        (&second, None, "chaz"),
        (&first, Some("branch-a"), "chaz"),
        (&first, Some("branch-b"), "guest"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut invocation = record(
            attempt,
            (n * 10) as u64,
            model(0, 1, &format!("Commentary {n}")),
        );
        let identity = invocation.identity.as_mut().unwrap();
        identity.execution_id = branch.map(str::to_owned);
        identity.agent = agent.into();
        let mut response = record(
            attempt,
            (n * 10 + 1) as u64,
            result(0, 0, &format!("RESULT-{n}"), ToolResultOutcome::Success),
        );
        response.identity = invocation.identity.clone();
        append(&session, invocation).await;
        append(&session, response).await;
    }
    reload(&mut app).await;
    assert_eq!(app.active().live.tool_activity.runs.len(), 4);
    app.expand_all = true;
    draw(&mut app, 120, 90);
    let source = text(&app);
    for n in 0..4 {
        assert!(source.contains(&format!("RESULT-{n}")));
        assert!(source.contains(&format!("Commentary {n}")));
    }
    assert_eq!(
        source.matches("    Result: Success").count(),
        4,
        "no separate audit-result copies"
    );
    for item in &app.active().live.tool_activity.items {
        let n = match item.key.run.execution.as_deref() {
            Some("branch-a") => 2,
            Some("branch-b") => 3,
            None if item.key.run.attempt == second.attempt_id => 1,
            None => 0,
            _ => unreachable!(),
        };
        assert_eq!(item.result.as_ref().unwrap().output, format!("RESULT-{n}"));
    }
}

#[tokio::test]
async fn nested_tool_outcomes_missing_expiry_and_stop_request_are_honest() {
    let (_instance, mut app) = app_with(Vec::new()).await;
    let mut session = Session::new(
        chaz_core::types::ConversationId("tools".into()),
        app.active().session_db.clone(),
    )
    .await;
    let attempt = session
        .start_turn_attempt(TurnRequestId::parse("request"))
        .await
        .unwrap();
    let outcomes = [
        ToolResultOutcome::Success,
        ToolResultOutcome::Error,
        ToolResultOutcome::Denied,
        ToolResultOutcome::ApprovalTimedOut,
        ToolResultOutcome::RateLimited,
        ToolResultOutcome::Blocked,
        ToolResultOutcome::TimedOut,
        ToolResultOutcome::Unavailable,
        ToolResultOutcome::Cancelled,
        ToolResultOutcome::Unknown {
            kind: "Future".into(),
            raw: serde_json::json!({"Future": {"reason": "retained"}}),
        },
    ];
    append(
        &session,
        record(&attempt, 0, model(0, outcomes.len() + 1, "partial **prose")),
    )
    .await;
    session
        .add_entry(entry("user", "stop", EntryType::Cancel))
        .await
        .unwrap();
    reload(&mut app).await;
    let item = &app.active().live.tool_activity.items[0];
    assert_eq!(
        item.status(&app.active().live.tool_activity.runs[0]),
        "awaiting result"
    );
    assert!(refresh::age_activity(
        &mut app,
        chrono::Utc::now() + chaz_core::session::TURN_ACTIVITY_TIMEOUT
    ));
    assert_eq!(
        app.active().live.tool_activity.items[0].status(&app.active().live.tool_activity.runs[0]),
        "result not recorded; activity unknown"
    );
    // Job observers use a separate lifecycle banner, but tool evidence must
    // still expire even when no write changes the pinned snapshot.
    // Force a fresh snapshot after artificial expiry with a benign write.
    session
        .add_entry(entry("system", "observer audit", EntryType::Ack))
        .await
        .unwrap();
    app.active_mut().job = None;
    reload(&mut app).await;
    app.active_mut().job = Some(crate::bridge::tui::jobs::JobTab {
        state: None,
        details: false,
    });
    assert!(app.active().live.tool_activity.runs[0].active);
    assert!(refresh::age_activity(
        &mut app,
        chrono::Utc::now() + chaz_core::session::TURN_ACTIVITY_TIMEOUT
    ));
    assert!(!app.active().live.tool_activity.runs[0].active);
    app.active_mut().job = None;
    for (i, outcome) in outcomes.into_iter().enumerate() {
        append(
            &session,
            record(
                &attempt,
                i as u64 + 1,
                result(
                    0,
                    i,
                    if i == 0 {
                        ""
                    } else {
                        "partial {\"text\":\"not repaired\\n"
                    },
                    outcome,
                ),
            ),
        )
        .await;
    }
    reload(&mut app).await;
    app.expand_all = true;
    draw(&mut app, 100, 90);
    let source = text(&app);
    app.expand_all = false;
    draw(&mut app, 3, 90);
    let header = app.active().live.render.clicks[0].0;
    assert_eq!(
        app.active().live.render.lines[header].to_string(),
        "!",
        "failure marker survives a one-cell pane"
    );
    app.expand_all = true;
    assert!(source.contains("stop requested by user"));
    assert!(source.contains("outcome unknown"));
    assert!(source.contains("Result: Success"));
    assert!(source.contains("(empty)"));
    for label in [
        "Error",
        "Denied",
        "ApprovalTimedOut",
        "RateLimited",
        "Blocked",
        "TimedOut",
        "Unavailable",
        "Cancelled",
        "Future",
    ] {
        assert!(source.contains(label), "{label}");
    }
    assert!(source.contains(r#"partial {"text":"not repaired\n"#));
    assert!(
        !source.contains("cancelled turn"),
        "a cancelled call or stop request does not acknowledge turn cancellation"
    );
    session.complete_turn_attempt(&attempt, None).await.unwrap();
    reload(&mut app).await;
    assert_eq!(
        app.active()
            .live
            .tool_activity
            .items
            .last()
            .unwrap()
            .status(&app.active().live.tool_activity.runs[0]),
        "missing result"
    );
    append(
        &session,
        record(&attempt, 30, TurnTranscriptMessage::Cancelled),
    )
    .await;
    reload(&mut app).await;
    assert_eq!(
        app.active()
            .live
            .tool_activity
            .items
            .last()
            .unwrap()
            .status(&app.active().live.tool_activity.runs[0]),
        "cancelled turn; result missing"
    );
    assert_eq!(
        app.active().live.tool_activity.items[0].status(&app.active().live.tool_activity.runs[0]),
        "done",
        "cancellation never relabels a successful result"
    );
}

#[tokio::test]
async fn nested_tool_toggles_refresh_keyboard_mouse_and_copy_keep_identity() {
    let (_instance, mut app) = app_with(Vec::new()).await;
    let mut session = Session::new(
        chaz_core::types::ConversationId("tools".into()),
        app.active().session_db.clone(),
    )
    .await;
    let attempt = session
        .start_turn_attempt(TurnRequestId::parse("request"))
        .await
        .unwrap();
    append(
        &session,
        record(&attempt, 0, model(0, 2, "Agent commentary")),
    )
    .await;
    let output = "start\n\n\twide 界 e\u{301} and literal \\n\n  indented\nend \u{1b}[31m";
    append(
        &session,
        record(
            &attempt,
            1,
            result(
                0,
                0,
                &serde_json::to_string(output).unwrap(),
                ToolResultOutcome::Success,
            ),
        ),
    )
    .await;
    reload(&mut app).await;
    app.active_mut().active_turns = 0;
    for _ in 0..3 {
        draw(&mut app, 100, 64);
        let header = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ToggleToolRun(0)))
            .unwrap();
        click(&mut app, header, true);
        draw(&mut app, 100, 64);
        assert!(text(&app).contains("file-0-0"));
        let item = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ToggleToolItem(0)))
            .unwrap();
        click(&mut app, item, true);
        draw(&mut app, 100, 64);
        assert!(text(&app).contains("    Arguments:"));
        let key = app.active().live.tool_activity.items[0].key.clone();
        assert!(app.active().live.expanded_tools.contains(&key));
        let outer = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ToggleToolRun(0)))
            .unwrap();
        click(&mut app, outer, false);
        draw(&mut app, 100, 64);
        assert!(!text(&app).contains("    Arguments:"));
        let outer = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ToggleToolRun(0)))
            .unwrap();
        click(&mut app, outer, true);
        draw(&mut app, 100, 64);
        assert!(text(&app).contains("    Arguments:"));
        // Toggle the item back off so each round starts with the same default.
        let item = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ToggleToolItem(0)))
            .unwrap();
        click(&mut app, item, false);
        let outer = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ToggleToolRun(0)))
            .unwrap();
        click(&mut app, outer, false);
    }
    input::handle_chat_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
    );
    assert!(app.expand_all);
    // Local command rows sort with persisted rows but have no evidence ID.
    let mut local = entry("system", "local output", EntryType::Message);
    local.timestamp = attempt.started_at - chrono::Duration::seconds(1);
    app.active_mut().add_local(local);
    session
        .add_entry(entry("system", "persistent audit", EntryType::Ack))
        .await
        .unwrap();
    reload(&mut app).await;
    assert!(app.active().live.entry_ids[0].is_none());
    assert_eq!(app.active().live.tool_activity.items[0].key.index, 0);
    for width in [24, 40, 100] {
        let terminal = draw(&mut app, width, 160);
        let drawn = rows(&terminal, 0, 160);
        println!("ACTUAL EXPANDED {width}:\n{}", drawn.join("\n"));
        let position = |fragment: &str| {
            drawn
                .iter()
                .enumerate()
                .rev()
                .find_map(|(y, row)| {
                    row.find(fragment).map(|byte| {
                        let prefix = &row[..byte];
                        (prefix.width() as u16, y as u16)
                    })
                })
                .unwrap_or_else(|| panic!("missing {fragment}: {drawn:?}"))
        };
        let start = position("start");
        let end = position("end �[31m");
        assert_eq!(
            drawn[usize::from(start.1) + 1].trim_matches(['│', ' ']),
            "",
            "a real blank line must survive JSON decoding and terminal rendering"
        );
        if width == 100 {
            assert!(drawn[usize::from(start.1) + 2].starts_with("│          wide"));
            assert!(drawn[usize::from(start.1) + 3].starts_with("│        indented"));
            assert!(drawn[usize::from(start.1) + 2].contains("literal \\n"));
        }
        // Tool body/provenance rows are not toggle hits, even when wrapped.
        assert!(!app.click_regions.iter().any(|r| r.hit(start.0, start.1)));
        let before = app.active().live.expanded_tools.clone();
        input::handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), start.0, start.1),
        );
        input::handle_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), end.0 + 8, end.1),
        );
        let copied = input::handle_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), end.0 + 8, end.1),
        );
        let Some(input::MouseOutcome::Copy(copied)) = copied else {
            panic!("tool payload must copy");
        };
        assert_eq!(
            copied,
            "start\n\n\twide 界 e\u{301} and literal \\n\n  indented\nend �[31m"
        );
        assert_eq!(app.active().live.expanded_tools, before);
        let ptr = app.active().live.render.lines_ptr();
        app.input.push('x');
        draw(&mut app, width, 160);
        assert_eq!(
            app.active().live.render.lines_ptr(),
            ptr,
            "input-only frames reuse the cache"
        );
        app.input.clear();
        app.debug_mode = true;
        draw(&mut app, width, 160);
        assert!(
            text(&app).contains(r#"\twide"#),
            "raw spelling is recoverable in debug"
        );
        app.debug_mode = false;
    }
    // Missing result arriving later updates the same item and expansion key.
    let key = app.active().live.tool_activity.items[1].key.clone();
    append(
        &session,
        record(
            &attempt,
            2,
            result(
                0,
                1,
                "late result\n\n  literal \\n",
                ToolResultOutcome::Success,
            ),
        ),
    )
    .await;
    reload(&mut app).await;
    assert_eq!(app.active().live.tool_activity.items[1].key, key);
    let terminal = draw(&mut app, 80, 60);
    let drawn = rows(&terminal, 0, 60);
    assert!(
        drawn
            .windows(3)
            .any(|lines| lines[0].starts_with("│      late result")
                && lines[1].trim_matches(['│', ' ']).is_empty()
                && lines[2].starts_with("│        literal \\n")),
        "plain text preserves newlines and indentation without unescaping literal backslashes: {drawn:?}"
    );
    assert_eq!(
        text(&app).matches("late result").count(),
        2,
        "one item preview plus its body, not a separate audit row"
    );
    for scroll in [0, 1, 7, 20, 100] {
        app.active_mut().scroll_offset = scroll;
        for width in [20, 40, 80] {
            let terminal = draw(&mut app, width, 30);
            assert_eq!(
                rows(&terminal, 1, 26),
                reference(&app, width, 25),
                "width {width} scroll {scroll}"
            );
        }
    }
    // /expand uses the same established global override.
    app.input = "/expand".into();
    app.cursor = app.input.len();
    input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.expand_all);
    let stored = refresh::load_session_snapshot("tools", session.database().clone())
        .await
        .unwrap();
    assert_eq!(
        stored.tool_activity.items[0]
            .result
            .as_ref()
            .unwrap()
            .output,
        serde_json::to_string(output).unwrap()
    );
    session
        .add_entry(entry("chaz", "Final answer", EntryType::Message))
        .await
        .unwrap();
}

#[tokio::test]
async fn nested_tool_evidence_failure_retains_view_and_retry_recovers() {
    let (_instance, mut app) = app_with(Vec::new()).await;
    let db = app.active().session_db.clone();
    let session = Session::new(chaz_core::types::ConversationId("tools".into()), db.clone()).await;
    let attempt = session
        .start_turn_attempt(TurnRequestId::parse("request"))
        .await
        .unwrap();
    append(&session, record(&attempt, 0, model(0, 2, "retained prose"))).await;
    reload(&mut app).await;
    draw(&mut app, 200, 30);
    let before = text(&app);
    let old_snapshot = db.snapshot().await.unwrap();
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<serde_json::Value>>("turn_transcript")
        .await
        .unwrap()
        .set("broken", serde_json::json!({"not": "a typed record"}))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    reload(&mut app).await;
    assert!(app.active().live.error.is_some());
    assert!(
        app.active().live.covered.is_none(),
        "failed evidence must not cover a fresh snapshot"
    );
    let terminal = draw(&mut app, 200, 30);
    assert!(rows(&terminal, 0, 30).join("\n").contains("refresh failed"));
    assert_eq!(text(&app), before);
    // A pinned old native snapshot is still coherent despite later corruption.
    let old = Session::try_new_at(
        chaz_core::types::ConversationId("tools".into()),
        db.clone(),
        &old_snapshot,
    )
    .await
    .unwrap();
    let loaded = refresh::snapshot_from_session(&old, &db, old_snapshot)
        .await
        .unwrap();
    assert_eq!(loaded.tool_activity.items.len(), 2);
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<Table<TurnTranscriptRecord>>("turn_transcript")
        .await
        .unwrap()
        .set(
            "broken",
            record(&attempt, 99, TurnTranscriptMessage::Cancelled),
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    refresh::request_tick(&mut app, &tx);
    while app.active().live.failed(refresh::RefreshKind::Session) {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        refresh::apply(&mut app, msg, |_, _| refresh::Resolved {
            effective_model: String::new(),
            context_budget: 0,
            roster: Vec::new(),
        });
    }
    assert!(app.active().live.error.is_none());
    assert!(app.active().live.covered.is_some());
    assert!(app.active().live.tool_activity.runs[0].cancelled);
}

#[tokio::test]
async fn nested_tool_legacy_history_is_unpaired_and_full_available_text_expandable() {
    let raw = "arbitrary_tool({\"path\":\"unfinished\n  exact raw suffix)";
    let (_instance, mut app) = app_with(Vec::new()).await;
    let mut session = Session::new(
        chaz_core::types::ConversationId("legacy".into()),
        app.active().session_db.clone(),
    )
    .await;
    session
        .add_entry(entry("chaz", raw, EntryType::ToolCall))
        .await
        .unwrap();
    session
        .add_entry(entry(
            "chaz",
            "arbitrary_tool: stored-preview…",
            EntryType::ToolResult,
        ))
        .await
        .unwrap();
    session
        .add_entry(entry("chaz", "ALL prose remains", EntryType::Message))
        .await
        .unwrap();
    reload(&mut app).await;
    assert!(app.active().live.tool_activity.runs.is_empty());
    app.expand_all = true;
    draw(&mut app, 80, 40);
    let source = text(&app);
    assert_eq!(source.matches("unpaired / identity unavailable").count(), 2);
    assert!(source.contains("Stored preview only"));
    assert!(source.contains("  exact raw suffix)"));
    assert!(source.contains("ALL prose remains"));
    assert_eq!(app.active().entries[0].content, raw);
}

#[tokio::test]
async fn nested_tool_wrapped_scrolled_header_hits_exclude_provenance_and_bodies() {
    let (_instance, mut app) = app_with(Vec::new()).await;
    let session = Session::new(
        chaz_core::types::ConversationId("tools".into()),
        app.active().session_db.clone(),
    )
    .await;
    let attempt = session
        .start_turn_attempt(TurnRequestId::parse("request"))
        .await
        .unwrap();
    append(&session, record(&attempt, 0, model(0, 2, "commentary"))).await;
    reload(&mut app).await;
    app.active_mut().active_turns = 0;
    app.expand_all = true;
    for width in [20, 40, 80] {
        app.active_mut().scroll_offset = 0;
        draw(&mut app, width, 24);
        let cache = &app.active().live.render;
        let (line, _) = *cache
            .clicks
            .iter()
            .find(|(_, t)| matches!(t, ClickTarget::ToggleToolItem(0)))
            .unwrap();
        let start = cache.offsets[line];
        let end = cache.offsets[line + 1];
        // Start the viewport inside this header, hiding its first wrapped row.
        let scroll = if end - start > 1 { start + 1 } else { start };
        let total = *cache.offsets.last().unwrap();
        app.active_mut().scroll_offset = total.saturating_sub(17).saturating_sub(scroll) as u16;
        draw(&mut app, width, 24);
        let region = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ToggleToolItem(0)))
            .unwrap();
        assert_eq!(region.x, 1);
        assert_eq!(region.w, width - 2);
        assert_eq!(region.h as usize, (end - scroll).min(17));
        assert!(
            !region.hit(region.x, region.y + region.h),
            "body/provenance begins after header hit"
        );
        let key = app.active().live.tool_activity.items[0].key.clone();
        assert!(!app.active().live.expanded_tools.contains(&key));
        click(&mut app, region, true);
        assert!(
            app.active().live.expanded_tools.contains(&key),
            "right edge of visible header toggles"
        );
        draw(&mut app, width, 24);
        let region = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::ToggleToolItem(0)))
            .unwrap();
        click(&mut app, region, false);
        assert!(!app.active().live.expanded_tools.contains(&key));
    }
}
