//! Input handling: key events → `ChatAction`, slash-command parsing,
//! help text, session-picker navigation. No async, no side effects beyond
//! mutating the shared `App` state.

use crate::commands::Command;
use crate::gateway::ApprovalDecision;

use crossterm::event::{KeyCode, KeyEvent};

use super::{show_error, show_system_msg, App, ChatAction, TuiMode};

pub(super) async fn handle_chat_key(
    app: &mut App,
    key: KeyEvent,
    session_db: &eidetica::Database,
) -> Option<ChatAction> {
    if let Some(exchange) = app.pending_approval.take() {
        let decision = match key.code {
            KeyCode::Char('y') => Some(ApprovalDecision::Approve),
            KeyCode::Char('n') => Some(ApprovalDecision::Deny),
            KeyCode::Char('a') => Some(ApprovalDecision::ApproveAll),
            _ => {
                app.pending_approval = Some(exchange);
                return None;
            }
        };
        if let Some(decision) = decision {
            let _ = exchange.decision_tx.send(decision);
        }
        return None;
    }

    match key.code {
        KeyCode::Enter => {
            if !app.input.is_empty() {
                let text = std::mem::take(&mut app.input);
                app.cursor = 0;
                return parse_chat_line(app, &text, session_db);
            }
        }
        KeyCode::Char(c) => {
            app.input.insert(app.cursor, c);
            app.cursor += c.len_utf8();
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let prev = app.input[..app.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                app.input.drain(prev..app.cursor);
                app.cursor = prev;
            }
        }
        KeyCode::Left => {
            if app.cursor > 0 {
                app.cursor = app.input[..app.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
        }
        KeyCode::Right => {
            if app.cursor < app.input.len() {
                app.cursor = app.input[app.cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| app.cursor + i)
                    .unwrap_or(app.input.len());
            }
        }
        KeyCode::Home => {
            app.cursor = 0;
        }
        KeyCode::End => {
            app.cursor = app.input.len();
        }
        KeyCode::Up => {
            app.scroll_offset = app.scroll_offset.saturating_add(3);
        }
        KeyCode::Down => {
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
        }
        KeyCode::PageUp => {
            app.scroll_offset = app.scroll_offset.saturating_add(20);
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(20);
        }
        KeyCode::Esc => {
            app.should_quit = true;
        }
        _ => {}
    }
    None
}

fn parse_chat_line(
    app: &mut App,
    text: &str,
    session_db: &eidetica::Database,
) -> Option<ChatAction> {
    match text {
        "/quit" | "/exit" | "/q" => return Some(ChatAction::Dispatch(Command::Quit)),
        "/sessions" | "/s" => return Some(ChatAction::OpenPicker),
        "/share" => return Some(ChatAction::Dispatch(Command::Share)),
        "/compact" => return Some(ChatAction::Dispatch(Command::Compact)),
        "/schedules" => return Some(ChatAction::Dispatch(Command::ListSchedules)),
        "/info" => return Some(ChatAction::Dispatch(Command::Info)),
        "/print" => return Some(ChatAction::Dispatch(Command::Print)),
        "/backends" => return Some(ChatAction::Dispatch(Command::ListBackends)),
        "/new" => return Some(ChatAction::Dispatch(Command::NewSession)),
        "/name" => return Some(ChatAction::Dispatch(Command::ClearSessionName)),
        "/role" => return Some(ChatAction::Dispatch(Command::Role(None))),
        "/model" => return Some(ChatAction::Dispatch(Command::Model(None))),
        "/channels" => return Some(ChatAction::Dispatch(Command::ListChannels)),
        "/agents" => return Some(ChatAction::Dispatch(Command::AgentsList)),
        _ => {}
    }

    if let Some(arg) = text.strip_prefix("/agent add ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentAdd(r)));
        }
        show_error(app, "Usage: /agent add <name|db_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/agent remove ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentRemove(r)));
        }
        show_error(app, "Usage: /agent remove <name|db_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/agent new ") {
        let trimmed = arg.trim();
        let mut parts = trimmed.split_whitespace();
        let name = match parts.next() {
            Some(n) => n.to_string(),
            None => {
                show_error(app, "Usage: /agent new <name> [k=v...]".to_string());
                return None;
            }
        };
        let mut overrides: Vec<(String, String)> = Vec::new();
        for tok in parts {
            match tok.split_once('=') {
                Some((k, v)) if !k.is_empty() => overrides.push((k.to_string(), v.to_string())),
                _ => {
                    show_error(
                        app,
                        format!("Invalid /agent new override '{tok}' — use key=value"),
                    );
                    return None;
                }
            }
        }
        return Some(ChatAction::Dispatch(Command::AgentNew { name, overrides }));
    }
    if let Some(arg) = text.strip_prefix("/agent share ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentShare(r)));
        }
        show_error(app, "Usage: /agent share <name|db_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/agent import ") {
        let t = arg.trim().to_string();
        if !t.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentImport(t)));
        }
        show_error(app, "Usage: /agent import <ticket>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/agent delete ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentDelete(r)));
        }
        show_error(app, "Usage: /agent delete <name|db_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/agent set ") {
        let trimmed = arg.trim();
        let mut parts = trimmed.splitn(3, char::is_whitespace);
        let agent_ref = parts.next().unwrap_or("").trim();
        let field = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if agent_ref.is_empty() || field.is_empty() || value.is_empty() {
            show_error(
                app,
                "Usage: /agent set <name|db_id> <field> <value>".to_string(),
            );
            return None;
        }
        return Some(ChatAction::Dispatch(Command::AgentSet {
            agent_ref: agent_ref.to_string(),
            field: field.to_string(),
            value: value.to_string(),
        }));
    }
    if text == "/agent hosted" {
        return Some(ChatAction::Dispatch(Command::AgentHosted));
    }
    if let Some(arg) = text.strip_prefix("/agent host ") {
        let r = arg.trim();
        return Some(ChatAction::Dispatch(Command::AgentSetHost(
            if r.is_empty() {
                None
            } else {
                Some(r.to_string())
            },
        )));
    }
    if text == "/agent host" {
        return Some(ChatAction::Dispatch(Command::AgentSetHost(None)));
    }
    if text == "/agent" || text == "/agent list" {
        return Some(ChatAction::Dispatch(Command::AgentsList));
    }

    // --- /memory: bank CRUD (Stage 9.D) ---
    if text == "/memory" || text == "/memory list" {
        return Some(ChatAction::Dispatch(Command::MemoryList));
    }
    if let Some(arg) = text.strip_prefix("/memory new ") {
        let trimmed = arg.trim();
        // First token is the name; remainder (optional) is description.
        let (name, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((n, r)) => (n.trim(), Some(r.trim().to_string())),
            None => (trimmed, None),
        };
        if name.is_empty() {
            show_error(
                app,
                "Usage: /memory new <name> [description...]".to_string(),
            );
            return None;
        }
        return Some(ChatAction::Dispatch(Command::MemoryNew {
            name: name.to_string(),
            description: rest.filter(|s| !s.is_empty()),
        }));
    }
    if let Some(arg) = text.strip_prefix("/memory delete ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::MemoryDelete(r)));
        }
        show_error(app, "Usage: /memory delete <name|db_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/memory grant ") {
        let trimmed = arg.trim();
        let mut parts = trimmed.splitn(3, char::is_whitespace);
        let bank = parts.next().unwrap_or("").trim();
        let agent = parts.next().unwrap_or("").trim();
        let perm = parts.next().unwrap_or("").trim();
        let permission = match perm.to_ascii_lowercase().as_str() {
            "read" | "r" => crate::agent_db::BankPermission::Read,
            "write" | "w" => crate::agent_db::BankPermission::Write,
            _ => {
                show_error(
                    app,
                    "Usage: /memory grant <bank> <agent> <read|write>".to_string(),
                );
                return None;
            }
        };
        if bank.is_empty() || agent.is_empty() {
            show_error(
                app,
                "Usage: /memory grant <bank> <agent> <read|write>".to_string(),
            );
            return None;
        }
        return Some(ChatAction::Dispatch(Command::MemoryGrant {
            bank_ref: bank.to_string(),
            agent_ref: agent.to_string(),
            permission,
        }));
    }
    if let Some(arg) = text.strip_prefix("/memory revoke ") {
        let trimmed = arg.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let bank = parts.next().unwrap_or("").trim();
        let agent = parts.next().unwrap_or("").trim();
        if bank.is_empty() || agent.is_empty() {
            show_error(app, "Usage: /memory revoke <bank> <agent>".to_string());
            return None;
        }
        return Some(ChatAction::Dispatch(Command::MemoryRevoke {
            bank_ref: bank.to_string(),
            agent_ref: agent.to_string(),
        }));
    }
    if let Some(arg) = text.strip_prefix("/memory share ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::MemoryShare(r)));
        }
        show_error(app, "Usage: /memory share <bank>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/memory import ") {
        let t = arg.trim().to_string();
        if !t.is_empty() {
            return Some(ChatAction::Dispatch(Command::MemoryImport(t)));
        }
        show_error(app, "Usage: /memory import <ticket>".to_string());
        return None;
    }

    if text == "/heartbeat" || text == "/heartbeat list" {
        return Some(ChatAction::Dispatch(Command::HeartbeatList));
    }
    if let Some(arg) = text.strip_prefix("/heartbeat remove ") {
        let id = arg.trim().to_string();
        if !id.is_empty() {
            return Some(ChatAction::Dispatch(Command::HeartbeatRemove(id)));
        }
        show_error(app, "Usage: /heartbeat remove <id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/heartbeat add ") {
        // Syntax: /heartbeat add <id> <cron_6_fields> <agent_ref> <task...>
        // Cron is 6 whitespace tokens (sec min hour dom mon dow) because that's
        // what the `cron` crate expects. Splitting on whitespace keeps parsing
        // simple; callers that want spaces in the task just type them.
        let mut parts = arg.split_whitespace();
        let id = parts.next();
        let c1 = parts.next();
        let c2 = parts.next();
        let c3 = parts.next();
        let c4 = parts.next();
        let c5 = parts.next();
        let c6 = parts.next();
        let agent_ref = parts.next();
        let task: String = parts.collect::<Vec<_>>().join(" ");
        match (id, c1, c2, c3, c4, c5, c6, agent_ref) {
            (Some(id), Some(a), Some(b), Some(c), Some(d), Some(e), Some(f), Some(ar))
                if !task.is_empty() =>
            {
                let cron = format!("{a} {b} {c} {d} {e} {f}");
                return Some(ChatAction::Dispatch(Command::HeartbeatAdd {
                    id: id.to_string(),
                    cron,
                    agent_ref: ar.to_string(),
                    task,
                }));
            }
            _ => {
                show_error(
                    app,
                    "Usage: /heartbeat add <id> <sec> <min> <hour> <dom> <mon> <dow> <agent> <task...>"
                        .to_string(),
                );
                return None;
            }
        }
    }

    if let Some(arg) = text.strip_prefix("/join ") {
        let id = arg.trim().to_string();
        if !id.is_empty() {
            return Some(ChatAction::Dispatch(Command::SwitchSession(id)));
        }
        return None;
    }
    if let Some(arg) = text.strip_prefix("/name ") {
        let name = arg.trim().to_string();
        if !name.is_empty() {
            return Some(ChatAction::Dispatch(Command::NameSession(name)));
        }
        return None;
    }
    if let Some(arg) = text.strip_prefix("/sync ") {
        let ticket = arg.trim().to_string();
        if !ticket.is_empty() {
            return Some(ChatAction::Dispatch(Command::Sync(ticket)));
        }
        return None;
    }
    if let Some(arg) = text.strip_prefix("/run ") {
        let name = arg.trim().to_string();
        if !name.is_empty() {
            return Some(ChatAction::Dispatch(Command::TriggerSchedule(name)));
        }
        return None;
    }
    if let Some(arg) = text.strip_prefix("/model ") {
        let model = arg.trim().to_string();
        if !model.is_empty() {
            return Some(ChatAction::Dispatch(Command::Model(Some(model))));
        }
        return None;
    }
    if let Some(arg) = text.strip_prefix("/role ") {
        let rest = arg.trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").trim().to_string();
        let prompt = parts.next().map(|s| s.trim().to_string());
        if !name.is_empty() {
            return Some(ChatAction::Dispatch(Command::Role(Some((name, prompt)))));
        }
        return None;
    }
    if let Some(arg) = text.strip_prefix("/backend ") {
        let mut parts = arg.split_whitespace();
        if let (Some(name), Some(url), Some(key)) = (parts.next(), parts.next(), parts.next()) {
            return Some(ChatAction::Dispatch(Command::SetBackend {
                name: name.to_string(),
                url: url.to_string(),
                api_key: key.to_string(),
            }));
        }
        show_error(
            app,
            "Usage: /backend <name> <api_base> <api_key>".to_string(),
        );
        return None;
    }

    match text {
        "/clear" => {
            app.entries.clear();
            app.scroll_offset = 0;
            return None;
        }
        "/debug" => {
            app.debug_mode = !app.debug_mode;
            return None;
        }
        "/raw" => {
            let mut raw = String::new();
            for (i, entry) in app.entries.iter().enumerate() {
                let ts = entry.timestamp.format("%H:%M:%S%.3f");
                let typ = format!("{:?}", entry.entry_type);
                let content_preview = if entry.content.len() > 80 {
                    format!("{}...", &entry.content[..80])
                } else {
                    entry.content.replace('\n', "\\n")
                };
                raw.push_str(&format!(
                    "#{i:3} [{ts}] {typ:<12} {:<15} {content_preview}\n",
                    entry.sender
                ));
            }
            show_system_msg(app, raw);
            return None;
        }
        "/help" | "/?" => {
            show_system_msg(app, help_text(session_db));
            return None;
        }
        _ => {}
    }

    if text.starts_with('/') {
        show_error(
            app,
            format!("Unknown command: {text}. Type /help for available commands."),
        );
        return None;
    }

    Some(ChatAction::SendMessage(text.to_string()))
}

fn help_text(_session_db: &eidetica::Database) -> String {
    [
        "Session:",
        "  /sessions, /s         — open session picker",
        "  /new                  — create a new session (picker 'n' key)",
        "  /join <id>            — switch to session by name or DB ID",
        "  /name [<alias>]       — set (or clear) a session alias",
        "  /info                 — show current session info",
        "  /channels             — list Matrix rooms attached to this session",
        "  /share                — generate shareable ticket for current session",
        "  /sync <ticket>        — sync a remote session via ticket",
        "  /compact              — summarize and compact conversation history",
        "  /print                — dump the transcript",
        "",
        "Living Agents:",
        "  /agents               — list agents attached to this session",
        "  /agent list           — same as /agents",
        "  /agent add <ref>      — attach an agent (display name or DB ID)",
        "  /agent remove <ref>   — detach an agent",
        "  /agent host [<ref>]   — set (or clear) the session's host agent",
        "  /agent hosted         — list every Living Agent this peer hosts",
        "  /agent new <name> [k=v...] — create a Living Agent (role|model|tools|can_spawn|allowed_callers|autonomous|max_iterations|tool_profile|max_context_tokens)",
        "  /agent set <ref> <field> <value> — edit an agent field (same set as /agent new); takes effect next message",
        "  /agent delete <ref>   — unregister a Living Agent (DB preserved for archive)",
        "  /agent share <ref>    — generate a share ticket for an agent's DB",
        "  /agent import <ticket>— sync + register an agent DB from a ticket",
        "",
        "Memory banks:",
        "  /memory list          — list memory banks this peer hosts",
        "  /memory new <name> [description...] — create a new bank on this peer",
        "  /memory delete <ref>  — unregister a bank (DB preserved)",
        "  /memory grant <bank> <agent> <read|write> — grant an agent access to a bank",
        "  /memory revoke <bank> <agent> — revoke an agent's access",
        "  /memory share <bank>  — generate a share ticket for a bank's DB",
        "  /memory import <ticket>— sync + register a bank DB from a ticket",
        "",
        "Heartbeat:",
        "  /heartbeat list       — list heartbeat rules on this session",
        "  /heartbeat add <id> <cron> <agent> <task>",
        "  /heartbeat remove <id>",
        "",
        "LLM config:",
        "  /model [<model>]      — show or set the model for this session",
        "  /role [<name> [<prompt>]] — show, select, or define a role",
        "  /backend <name> <url> <key> — add a custom backend for this session",
        "  /backends             — list known backends and models",
        "",
        "Scheduler:",
        "  /schedules            — list configured schedules",
        "  /run <name>           — trigger a schedule immediately",
        "",
        "TUI:",
        "  /clear                — clear display (entries still in DB)",
        "  /raw                  — dump raw entry data for debugging",
        "  /debug                — toggle debug mode (Ctrl+D)",
        "  /help, /?             — this help",
        "  /quit, /exit, /q      — exit",
        "",
        "Keys:",
        "  Ctrl+D                — toggle debug mode (shows timestamps, types)",
        "  Ctrl+C                — quit",
        "  Up/Down, PageUp/Dn    — scroll messages",
    ]
    .join("\n")
}

pub(super) fn handle_picker_key(app: &mut App, key: KeyEvent) -> Option<String> {
    match key.code {
        KeyCode::Up => {
            if app.picker_index > 0 {
                app.picker_index -= 1;
            }
            None
        }
        KeyCode::Down => {
            if app.picker_index + 1 < app.session_list.len() {
                app.picker_index += 1;
            }
            None
        }
        KeyCode::Enter => app
            .session_list
            .get(app.picker_index)
            .map(|info| info.session_db_id.clone()),
        KeyCode::Char('n') => Some("__new__".to_string()),
        KeyCode::Esc => {
            app.mode = TuiMode::Chat;
            None
        }
        _ => None,
    }
}
