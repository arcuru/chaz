//! Nested rendering of the identified, snapshot-local tool projection.
use super::*;
use crate::bridge::tui::tool_activity::{ToolItem, ToolRun};
use chaz_core::runtime::ToolResultOutcome;
use serde_json::Value;

type ToolLines = (
    Vec<Line<'static>>,
    Vec<(usize, ClickTarget)>,
    Vec<(usize, usize, String)>,
);

use content::clipped;

fn preview(text: &str, arguments: bool) -> String {
    let decoded = serde_json::from_str::<Value>(text);
    let summary = match &decoded {
        Ok(Value::Object(fields)) => {
            let selected = arguments
                .then(|| {
                    ["command", "path", "query", "url"]
                        .into_iter()
                        .find_map(|key| {
                            fields
                                .get(key)
                                .and_then(Value::as_str)
                                .map(|value| format!("{key}: {}", ellipsize(value, 80)))
                        })
                })
                .flatten();
            selected.unwrap_or_else(|| format!("object ({} fields)", fields.len()))
        }
        Ok(Value::Array(items)) => format!("array ({} items)", items.len()),
        Ok(Value::String(text)) => ellipsize(text.lines().next().unwrap_or(""), 80),
        Ok(other) => other.to_string(),
        Err(_) => ellipsize(text.lines().next().unwrap_or(""), 80),
    };
    content::safe_line(&summary.replace(['\n', '\r'], " "))
}

fn counts(run: &ToolRun, items: &[ToolItem]) -> (char, String) {
    let (mut done, mut failed, mut cancelled, mut pending, mut unknown) = (0, 0, 0, 0, 0);
    for &index in &run.calls {
        match items[index].result.as_ref().map(|r| &r.outcome) {
            Some(ToolResultOutcome::Success) => done += 1,
            Some(ToolResultOutcome::Cancelled) => cancelled += 1,
            Some(ToolResultOutcome::Unknown { .. }) => unknown += 1,
            Some(_) => failed += 1,
            None => pending += 1,
        }
    }
    let marker = if failed > 0 {
        '!'
    } else if cancelled > 0 || run.cancelled {
        '×'
    } else if unknown > 0 {
        '?'
    } else if pending > 0 {
        '…'
    } else {
        '✓'
    };
    (
        marker,
        format!(
            "{done} done · {failed} failed · {cancelled} cancelled · {pending} without result · {unknown} unknown"
        ),
    )
}

pub(super) fn payload(
    lines: &mut Vec<Line<'static>>,
    sources: &mut Vec<(usize, usize, String)>,
    raw: &str,
    debug: bool,
) {
    let readable;
    let text = if debug {
        raw
    } else {
        readable = content::tool_output(raw);
        &readable
    };
    for line in text.replace("\r\n", "\n").split('\n') {
        let safe = content::safe_line(line);
        sources.push((lines.len(), 6, content::copy_line(line, &safe)));
        lines.push(Line::from(Span::styled(
            format!("      {safe}"),
            theme::dim(),
        )));
    }
}

pub(super) fn render(
    tab: &super::super::Tab,
    index: usize,
    debug: bool,
    expand_all: bool,
    width: u16,
) -> ToolLines {
    let activity = &tab.live.tool_activity;
    let item = &activity.items[index];
    let run = &activity.runs[item.run];
    let run_label = item.run + 1;
    let outer = expand_all != tab.live.expanded_runs.contains(&run.key);
    let inner = expand_all != tab.live.expanded_tools.contains(&item.key);
    let mut lines = Vec::new();
    let mut clicks = Vec::new();
    let mut sources = Vec::new();
    if run.calls.first() == Some(&index) {
        let workers = run
            .calls
            .iter()
            .filter(|&&i| activity.items[i].worker.is_some())
            .count();
        let (marker, counts) = counts(run, &activity.items);
        for (row, text) in [
            format!(
                "{} {marker} R{run_label} {} · {} tools · {}",
                if outer { "▾" } else { "▸" },
                run.key.agent,
                run.calls.len(),
                run.lifecycle()
            ),
            if workers > 0 {
                format!("  {workers} linked Workers · Open workers (Alt+W) · {counts}")
            } else {
                format!("  {counts}")
            },
        ]
        .into_iter()
        .enumerate()
        {
            clicks.push((
                lines.len(),
                if row == 1 && workers > 0 {
                    ClickTarget::OpenWorkers
                } else {
                    ClickTarget::ToggleToolRun(item.run)
                },
            ));
            let text = if row == 0 && (1..4).contains(&width) {
                marker.to_string()
            } else {
                clipped(&text, width)
            };
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(COLOR_TOOL),
            )));
        }
        if outer {
            if let Some(error) = &activity.worker_error {
                lines.push(Line::from(Span::styled(
                    clipped(
                        &format!("    Worker references unavailable: {error}"),
                        width,
                    ),
                    theme::error(),
                )));
            }
            lines.push(Line::from(Span::styled(
                content::safe_line(&format!("    R{run_label}: {} · {counts}", run.lifecycle())),
                theme::dim(),
            )));
        }
    }
    if !outer {
        return (lines, clicks, sources);
    }
    let result_event = item
        .result
        .as_ref()
        .map_or_else(|| "?".into(), |r| format!("e{}", r.position.sequence));
    let status = item.status(run);
    let summary = preview(&item.arguments, true);
    let result_summary = item.result.as_ref().map(|r| {
        if r.output.is_empty() {
            "empty result".into()
        } else {
            preview(&r.output, false)
        }
    });
    clicks.push((lines.len(), ClickTarget::ToggleToolItem(index)));
    lines.push(Line::from(Span::styled(
        content::safe_line(&format!(
            "  {} R{run_label} #{} {} · {status} [e{}.{}→{result_event}] {summary}{}",
            if inner { "▾" } else { "▸" },
            item.ordinal,
            item.name,
            item.position.sequence,
            item.key.index,
            result_summary.map_or(String::new(), |s| format!(" → {s}")),
        )),
        if status == "failed" {
            theme::error()
        } else {
            Style::default().fg(COLOR_TOOL)
        },
    )));
    if let Some(spawn) = &item.worker {
        let requested = serde_json::from_str::<Value>(&item.arguments).unwrap_or(Value::Null);
        let name = requested
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown template");
        let task = requested
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or("assignment unavailable");
        lines.push(Line::from(Span::styled(
            clipped(&format!("    Worker {name} · requested: {task}"), width),
            theme::dim(),
        )));
        let evidence = tab.live.workers.data.evidence.get(&spawn.spawn_id);
        let (status, result) = match evidence {
            Some(Ok(evidence)) => (
                evidence.status(Utc::now()).to_string(),
                evidence.result.as_ref().map(|e| e.content.as_str()),
            ),
            Some(Err(error)) => (format!("transcript unavailable: {error}"), None),
            None => ("child evidence not loaded".into(), None),
        };
        let stale = if tab.live.workers.error.is_some() {
            " · stale read"
        } else {
            ""
        };
        clicks.push((lines.len(), ClickTarget::OpenWorker(index)));
        lines.push(Line::from(Span::styled(
            clipped(&format!("    Open child · {status}{stale}"), width),
            Style::default().fg(COLOR_ACCENT),
        )));
        if let Some(result) = result {
            lines.push(Line::from(Span::styled(
                clipped(
                    &format!("    Child result: {}", preview(result, false)),
                    width,
                ),
                theme::dim(),
            )));
        }
    }
    if inner {
        lines.push(Line::from(Span::styled(
            format!(
                "    call e{}.{} → result {result_event} (event order, not display order)",
                item.position.sequence, item.key.index
            ),
            theme::dim(),
        )));
        lines.push(Line::from(Span::styled(
            content::safe_line(&format!(
                "    request={} attempt={} execution={} agent={} model={} index={} call_id={}",
                run.key.request,
                run.key.attempt,
                run.key.execution.as_deref().unwrap_or("none"),
                run.key.agent,
                item.key.model_sequence,
                item.key.index,
                item.key.id,
            )),
            theme::dim(),
        )));
        lines.push(Line::from(Span::styled(
            format!("    Invocation: {}", item.position.timestamp),
            theme::dim(),
        )));
        lines.push(Line::from(Span::styled("    Arguments:", theme::dim())));
        payload(&mut lines, &mut sources, &item.arguments, debug);
        if let Some(result) = &item.result {
            lines.push(Line::from(Span::styled(
                content::safe_line(&format!(
                    "    Result: {:?} at {}{}",
                    result.outcome,
                    result.position.timestamp,
                    if result.output.is_empty() {
                        " (empty)"
                    } else {
                        ""
                    },
                )),
                theme::dim(),
            )));
            payload(&mut lines, &mut sources, &result.output, debug);
        } else {
            lines.push(Line::from(Span::styled(
                format!("    Result: {status}"),
                theme::dim(),
            )));
        }
    }
    (lines, clicks, sources)
}
