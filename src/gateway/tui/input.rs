//! Input handling: key events → `ChatAction`, slash-command parsing,
//! help text, session-picker navigation. No async, no side effects beyond
//! mutating the shared `App` state.

use crate::commands::{Command, parse_permission_token};
use crate::gateway::ApprovalDecision;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};

use super::{App, ChatAction, ClickTarget, Overlay, TuiMode, show_error, show_system_msg};

/// Outcome of routing a key through the active overlay.
pub(super) enum OverlayKey {
    /// No overlay is open — let the mode handler see this key.
    NotConsumed,
    /// Overlay handled the key; nothing further to do.
    Consumed,
    /// The rename overlay was submitted. The main loop persists the change
    /// (passing `None` clears the alias) and refreshes the picker list.
    RenameSubmit {
        session_db_id: String,
        name: Option<String>,
    },
}

/// Routes a key through the active overlay. Called from the top of
/// `handle_chat_key` / picker handling so overlays intercept input before the
/// underlying mode sees it.
pub(super) fn handle_overlay_key(app: &mut App, key: KeyEvent) -> OverlayKey {
    let Some(overlay) = app.overlay.as_mut() else {
        return OverlayKey::NotConsumed;
    };
    match overlay {
        Overlay::Help { scroll } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                app.overlay = None;
            }
            KeyCode::Up => *scroll = scroll.saturating_sub(1),
            KeyCode::Down => *scroll = scroll.saturating_add(1),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
            KeyCode::PageDown => *scroll = scroll.saturating_add(10),
            KeyCode::Home => *scroll = 0,
            _ => {}
        },
        Overlay::RenamePrompt {
            session_db_id,
            input,
            cursor,
            ..
        } => match key.code {
            KeyCode::Esc => {
                app.overlay = None;
            }
            KeyCode::Enter => {
                let trimmed = input.trim();
                let name = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                let session_db_id = std::mem::take(session_db_id);
                app.overlay = None;
                return OverlayKey::RenameSubmit {
                    session_db_id,
                    name,
                };
            }
            KeyCode::Char(c) => {
                input.insert(*cursor, c);
                *cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                if *cursor > 0 {
                    let prev = input[..*cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    input.drain(prev..*cursor);
                    *cursor = prev;
                }
            }
            KeyCode::Left => {
                if *cursor > 0 {
                    *cursor = input[..*cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            KeyCode::Right => {
                if *cursor < input.len() {
                    *cursor = input[*cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| *cursor + i)
                        .unwrap_or(input.len());
                }
            }
            KeyCode::Home => *cursor = 0,
            KeyCode::End => *cursor = input.len(),
            _ => {}
        },
    }
    OverlayKey::Consumed
}

/// Actions the mouse handler wants the main loop to take that it can't do on
/// its own because they need cross-module context (command dispatch, session
/// switching, etc.). None for the common no-op path.
pub(super) enum MouseOutcome {
    /// Open the currently selected session picker row — equivalent to
    /// pressing Enter.
    PickerOpenSelected,
    /// Activate tab at the given index.
    TabActivate(usize),
    /// Close tab at the given index.
    TabClose(usize),
}

pub(super) fn handle_mouse(app: &mut App, m: MouseEvent) -> Option<MouseOutcome> {
    // Wheel scrolls the overlay when one is up, otherwise the chat history.
    match m.kind {
        MouseEventKind::ScrollUp => {
            if let Some(Overlay::Help { scroll }) = app.overlay.as_mut() {
                *scroll = scroll.saturating_sub(3);
            } else {
                let off = &mut app.active_mut().scroll_offset;
                *off = off.saturating_add(3);
            }
            return None;
        }
        MouseEventKind::ScrollDown => {
            if let Some(Overlay::Help { scroll }) = app.overlay.as_mut() {
                *scroll = scroll.saturating_add(3);
            } else {
                let off = &mut app.active_mut().scroll_offset;
                *off = off.saturating_sub(3);
            }
            return None;
        }
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return None,
    }

    // Left-click — find the innermost hit region. `click_regions` is pushed in
    // outer-to-inner order during render (overlay backdrop first, rows next),
    // so iterate in reverse to prefer the most specific hit.
    let (col, row) = (m.column, m.row);
    let hit = app
        .click_regions
        .iter()
        .rev()
        .copied()
        .find(|r| r.hit(col, row));
    let hit = hit?;
    match hit.target {
        ClickTarget::OverlayDismiss => {
            app.overlay = None;
        }
        ClickTarget::HelpCommand(template) => {
            // Insert the template into the input box and close the overlay so
            // the user can edit and submit. Cursor goes to end.
            app.input = template.to_string();
            app.cursor = app.input.len();
            app.overlay = None;
        }
        ClickTarget::ApprovalApprove => apply_approval(app, ApprovalDecision::Approve),
        ClickTarget::ApprovalDeny => apply_approval(app, ApprovalDecision::Deny),
        ClickTarget::ApprovalApproveAll => apply_approval(app, ApprovalDecision::ApproveAll),
        ClickTarget::PickerSelect(i) => {
            // First click selects; second click on the same row opens. Keeps
            // the keyboard flow (Up/Down then Enter) intact.
            if app.picker_index == i && i < app.session_list.len() {
                return Some(MouseOutcome::PickerOpenSelected);
            }
            if i < app.session_list.len() {
                app.picker_index = i;
            }
        }
        ClickTarget::TabActivate(i) => return Some(MouseOutcome::TabActivate(i)),
        ClickTarget::TabClose(i) => return Some(MouseOutcome::TabClose(i)),
    }
    None
}

fn apply_approval(app: &mut App, decision: ApprovalDecision) {
    if let Some(exchange) = app.active_mut().pending_approval.take() {
        let _ = exchange.decision_tx.send(decision);
    }
}

pub(super) async fn handle_chat_key(app: &mut App, key: KeyEvent) -> Option<ChatAction> {
    if let Some(exchange) = app.active_mut().pending_approval.take() {
        let decision = match key.code {
            KeyCode::Char('y') => Some(ApprovalDecision::Approve),
            KeyCode::Char('n') => Some(ApprovalDecision::Deny),
            KeyCode::Char('a') => Some(ApprovalDecision::ApproveAll),
            _ => {
                app.active_mut().pending_approval = Some(exchange);
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
                return parse_chat_line(app, &text);
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
            let off = &mut app.active_mut().scroll_offset;
            *off = off.saturating_add(3);
        }
        KeyCode::Down => {
            let off = &mut app.active_mut().scroll_offset;
            *off = off.saturating_sub(3);
        }
        KeyCode::PageUp => {
            let off = &mut app.active_mut().scroll_offset;
            *off = off.saturating_add(20);
        }
        KeyCode::PageDown => {
            let off = &mut app.active_mut().scroll_offset;
            *off = off.saturating_sub(20);
        }
        KeyCode::Esc => {
            app.should_quit = true;
        }
        KeyCode::F(1) => {
            app.overlay = Some(Overlay::Help { scroll: 0 });
        }
        _ => {}
    }
    None
}

fn parse_chat_line(app: &mut App, text: &str) -> Option<ChatAction> {
    match text {
        "/quit" | "/exit" | "/q" => return Some(ChatAction::Dispatch(Command::Quit)),
        "/sessions" | "/s" => return Some(ChatAction::OpenPicker),
        "/share" => return Some(ChatAction::Dispatch(Command::Share)),
        "/unshare" => return Some(ChatAction::Dispatch(Command::SessionUnshare)),
        "/compact" => return Some(ChatAction::Dispatch(Command::Compact)),
        "/schedules" => return Some(ChatAction::Dispatch(Command::ListSchedules)),
        "/info" => return Some(ChatAction::Dispatch(Command::Info)),
        "/print" => return Some(ChatAction::Dispatch(Command::Print)),
        "/backends" => return Some(ChatAction::Dispatch(Command::ListBackends)),
        "/new" => return Some(ChatAction::Dispatch(Command::NewSession)),
        "/name" | "/rename" => return Some(ChatAction::Dispatch(Command::ClearSessionName)),
        "/role" => return Some(ChatAction::Dispatch(Command::Role(None))),
        "/model" => return Some(ChatAction::Dispatch(Command::Model(None))),
        "/channels" => return Some(ChatAction::Dispatch(Command::ListChannels)),
        "/agents" => return Some(ChatAction::Dispatch(Command::AgentsList)),
        "/pubkey" => return Some(ChatAction::Dispatch(Command::Pubkey)),
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
    if let Some(arg) = text.strip_prefix("/agent unshare ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentUnshare(r)));
        }
        show_error(app, "Usage: /agent unshare <name|db_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/agent import ") {
        let trimmed = arg.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let ticket = parts.next().unwrap_or("").trim().to_string();
        let perm_tok = parts.next().unwrap_or("").trim();
        if ticket.is_empty() {
            show_error(
                app,
                "Usage: /agent import <ticket> [admin|write|read]".to_string(),
            );
            return None;
        }
        // Default for /agent import is write — co-ownership with edit
        // privileges. Admin and Read are explicit opt-ins.
        let permission = match perm_tok {
            "" => crate::commands::CoOwnerPermission::Write,
            other => match parse_permission_token(other) {
                Some(p) => p,
                None => {
                    show_error(
                        app,
                        format!(
                            "Unknown permission '{other}' — use admin, write, or read (default: write)"
                        ),
                    );
                    return None;
                }
            },
        };
        return Some(ChatAction::Dispatch(Command::AgentImport {
            ticket,
            permission,
        }));
    }
    if let Some(arg) = text.strip_prefix("/agent delete ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentDelete(r)));
        }
        show_error(app, "Usage: /agent delete <name|db_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/agent invite ") {
        let trimmed = arg.trim();
        let mut parts = trimmed.splitn(3, char::is_whitespace);
        let agent_ref = parts.next().unwrap_or("").trim();
        let pubkey = parts.next().unwrap_or("").trim();
        let perm_tok = parts.next().unwrap_or("").trim();
        if agent_ref.is_empty() || pubkey.is_empty() {
            show_error(
                app,
                "Usage: /agent invite <ref> <pubkey> [admin|write|read]".to_string(),
            );
            return None;
        }
        let permission = match parse_permission_token(perm_tok) {
            Some(p) => p,
            None => {
                show_error(
                    app,
                    format!(
                        "Unknown permission '{perm_tok}' — use admin, write, or read (default: admin)"
                    ),
                );
                return None;
            }
        };
        return Some(ChatAction::Dispatch(Command::AgentInvite {
            agent_ref: agent_ref.to_string(),
            pubkey: pubkey.to_string(),
            permission,
        }));
    }
    if let Some(arg) = text.strip_prefix("/agent revoke-peer ") {
        let trimmed = arg.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let agent_ref = parts.next().unwrap_or("").trim();
        let pubkey = parts.next().unwrap_or("").trim();
        if agent_ref.is_empty() || pubkey.is_empty() {
            show_error(app, "Usage: /agent revoke-peer <ref> <pubkey>".to_string());
            return None;
        }
        return Some(ChatAction::Dispatch(Command::AgentRevokePeer {
            agent_ref: agent_ref.to_string(),
            pubkey: pubkey.to_string(),
        }));
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
    if let Some(arg) = text.strip_prefix("/agent persona show ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentPersonaShow(r)));
        }
        show_error(app, "Usage: /agent persona show <name|db_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/agent persona bump ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::AgentPersonaBump(r)));
        }
        show_error(app, "Usage: /agent persona bump <name|db_id>".to_string());
        return None;
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
    if let Some(arg) = text.strip_prefix("/memory unshare ") {
        let r = arg.trim().to_string();
        if !r.is_empty() {
            return Some(ChatAction::Dispatch(Command::MemoryUnshare(r)));
        }
        show_error(app, "Usage: /memory unshare <bank>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/memory import ") {
        let trimmed = arg.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let ticket = parts.next().unwrap_or("").trim().to_string();
        let perm_tok = parts.next().unwrap_or("").trim();
        if ticket.is_empty() {
            show_error(
                app,
                "Usage: /memory import <ticket> [admin|write|read]".to_string(),
            );
            return None;
        }
        let permission = match perm_tok {
            "" => crate::commands::CoOwnerPermission::Write,
            other => match parse_permission_token(other) {
                Some(p) => p,
                None => {
                    show_error(
                        app,
                        format!(
                            "Unknown permission '{other}' — use admin, write, or read (default: write)"
                        ),
                    );
                    return None;
                }
            },
        };
        return Some(ChatAction::Dispatch(Command::MemoryImport {
            ticket,
            permission,
        }));
    }

    // Bootstrap-queue surface (Co-owned Stage 11). Single namespace
    // covering pending requests across every kind of resource.
    if text == "/sharing" || text == "/sharing status" {
        return Some(ChatAction::Dispatch(Command::SharingStatus));
    }
    if text == "/sharing requests" || text == "/sharing list" {
        return Some(ChatAction::Dispatch(Command::SharingRequests));
    }
    if let Some(arg) = text.strip_prefix("/sharing approve ") {
        let id = arg.trim().to_string();
        if !id.is_empty() {
            return Some(ChatAction::Dispatch(Command::SharingApprove(id)));
        }
        show_error(app, "Usage: /sharing approve <request_id>".to_string());
        return None;
    }
    if let Some(arg) = text.strip_prefix("/sharing reject ") {
        let id = arg.trim().to_string();
        if !id.is_empty() {
            return Some(ChatAction::Dispatch(Command::SharingReject(id)));
        }
        show_error(app, "Usage: /sharing reject <request_id>".to_string());
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
    if let Some(arg) = text
        .strip_prefix("/name ")
        .or_else(|| text.strip_prefix("/rename "))
    {
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
            let tab = app.active_mut();
            tab.entries.clear();
            tab.scroll_offset = 0;
            return None;
        }
        "/debug" => {
            app.debug_mode = !app.debug_mode;
            return None;
        }
        "/raw" => {
            let mut raw = String::new();
            for (i, entry) in app.active().entries.iter().enumerate() {
                let ts = entry.timestamp.format("%H:%M:%S%.3f");
                let typ = format!("{:?}", entry.entry_type);
                let t = crate::util::truncate_chars(&entry.content, 80);
                let content_preview = if t.len() < entry.content.len() {
                    format!("{t}...")
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
            app.overlay = Some(Overlay::Help { scroll: 0 });
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
        KeyCode::Char('r') => {
            if let Some(info) = app.session_list.get(app.picker_index) {
                let initial = info.name.clone().unwrap_or_default();
                let cursor = initial.len();
                let title = match &info.name {
                    Some(n) => format!("Rename \"{n}\""),
                    None => format!(
                        "Name session {}",
                        info.session_db_id
                            .rsplit(':')
                            .next()
                            .unwrap_or(&info.session_db_id)
                            .chars()
                            .take(8)
                            .collect::<String>()
                    ),
                };
                app.overlay = Some(Overlay::RenamePrompt {
                    session_db_id: info.session_db_id.clone(),
                    title,
                    input: initial,
                    cursor,
                });
            }
            None
        }
        KeyCode::Esc => {
            app.mode = TuiMode::Chat;
            None
        }
        _ => None,
    }
}
