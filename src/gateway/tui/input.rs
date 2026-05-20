//! Input handling: key events → `ChatAction`, slash-command parsing,
//! help text, session-picker navigation. No async, no side effects beyond
//! mutating the shared `App` state.

use crate::commands::{Command, ExtensionsAction, parse_permission_token};
use crate::gateway::ApprovalDecision;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};

use super::{
    App, ChatAction, ClickTarget, Completion, Overlay, TuiMode, show_error, show_system_msg,
};

/// Grouped, ordered catalog of every built-in slash command. Single source of
/// truth shared by the help overlay (which renders the `#`-prefixed section
/// headers) and inline completion (which skips them). Templates ending in a
/// space take an argument; the help overlay and completion both insert the
/// template verbatim so the cursor lands ready for that argument.
pub(super) fn command_catalog() -> Vec<(&'static str, &'static str)> {
    vec![
        ("# Session", ""),
        ("/sessions", "open session picker"),
        ("/new", "create a new session"),
        ("/join ", "switch to session by name or DB ID"),
        ("/name ", "set (or clear) a session alias"),
        ("/rename ", "alias for /name"),
        ("/info", "show current session info"),
        ("/costs", "aggregate LLM usage + cost across all sessions"),
        ("/channels", "list Matrix rooms attached to this session"),
        ("/share", "generate shareable ticket for current session"),
        ("/sync ", "sync a remote session via ticket"),
        ("/compact", "summarize and compact conversation history"),
        ("/print", "dump the transcript"),
        ("# Living Agents", ""),
        ("/agents", "list agents attached to this session"),
        ("/agent add ", "attach an agent (display name or DB ID)"),
        ("/agent remove ", "detach an agent"),
        ("/agent host ", "set (or clear) the session's host agent"),
        (
            "/agent room",
            "chat-room status: roster, host, burst budget",
        ),
        ("/agent hosted", "list every Living Agent this peer hosts"),
        (
            "/agent new ",
            "create a Living Agent (see docs for k=v fields)",
        ),
        (
            "/agent set ",
            "edit an agent field; takes effect next message",
        ),
        ("/agent delete ", "unregister a Living Agent (DB preserved)"),
        ("/agent share ", "generate a share ticket for an agent's DB"),
        (
            "/agent import ",
            "request access to an agent DB via ticket [admin|write|read]",
        ),
        (
            "/agent invite ",
            "preseed another peer's pubkey (admin|write|read)",
        ),
        ("/agent revoke-peer ", "revoke a co-owner's access"),
        ("/pubkey", "show this peer's default pubkey"),
        ("# Memory banks", ""),
        ("/memory list", "list memory banks this peer hosts"),
        ("/memory new ", "create a new bank on this peer"),
        ("/memory delete ", "unregister a bank (DB preserved)"),
        (
            "/memory grant ",
            "grant an agent access to a bank (read|write)",
        ),
        ("/memory revoke ", "revoke an agent's access"),
        ("/memory share ", "generate a share ticket for a bank's DB"),
        (
            "/memory import ",
            "request access to a bank via ticket [admin|write|read]",
        ),
        ("# Sharing queue", ""),
        ("/sharing", "list databases this peer is sharing"),
        ("/sharing requests", "list pending bootstrap requests"),
        ("/sharing approve ", "approve a request by id"),
        ("/sharing reject ", "reject a request by id"),
        ("/unshare", "stop sharing the current session"),
        ("/agent unshare ", "stop sharing an agent DB"),
        ("/memory unshare ", "stop sharing a memory bank"),
        ("# Schedule", ""),
        ("/schedule list", "list an agent's schedules"),
        ("/schedule add ", "<id> <cron 6 fields> <agent> <task...>"),
        ("/schedule remove ", "remove a schedule by id"),
        ("# LLM config", ""),
        ("/model ", "show or set the model for this session"),
        ("/role ", "show, select, or define a role"),
        ("/backend ", "add a custom backend (<name> <url> <key>)"),
        ("/backends", "list known backends and models"),
        ("# TUI", ""),
        ("/clear", "clear display (entries still in DB)"),
        ("/raw", "dump raw entry data for debugging"),
        ("/debug", "toggle debug mode (Ctrl+D)"),
        ("/help", "this help"),
        ("/quit", "exit"),
    ]
}

/// True when accepting `tpl` would extend `input` — i.e. `input` is a strict
/// (case-insensitive) prefix of `tpl`, so there's more command left to insert.
/// When this is false the command is either fully typed or the user is typing
/// its arguments, so Tab/Enter should leave the text alone.
fn command_extends(input: &str, tpl: &str) -> bool {
    let (il, tl) = (input.to_lowercase(), tpl.to_lowercase());
    tl.starts_with(&il) && tl.len() > il.len()
}

/// Commands to show in the popup for the current `input`. Two modes, so the
/// command + description stays visible while you type:
///
/// * **completion** — every catalog template that `input` is a prefix of
///   (you're still picking / extending a command). Returned as-is.
/// * **reference** — if nothing is left to complete, the single most-specific
///   template that is a prefix of `input` (you've typed the command and are
///   now filling in its arguments). Keeps that one row visible.
///
/// Empty only when `input` isn't a slash command, or matches nothing at all.
pub(super) fn matching_commands(input: &str) -> Vec<(&'static str, &'static str)> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    let il = input.to_lowercase();
    let catalog = command_catalog();

    let completions: Vec<(&'static str, &'static str)> = catalog
        .iter()
        .filter(|(tpl, _)| !tpl.starts_with('#'))
        .filter(|(tpl, _)| tpl.to_lowercase().starts_with(&il))
        .copied()
        .collect();
    if !completions.is_empty() {
        return completions;
    }

    // No completion — keep the command being argument-filled on screen by
    // showing the longest template that is a prefix of the input.
    catalog
        .iter()
        .filter(|(tpl, _)| !tpl.starts_with('#'))
        .filter(|(tpl, _)| il.starts_with(&tpl.to_lowercase()))
        .max_by_key(|(tpl, _)| tpl.len())
        .map(|m| vec![*m])
        .unwrap_or_default()
}

/// Recompute `app.completion` from the current input. Opens the popup when the
/// input starts with `/` and at least one catalog command prefix-matches
/// (case-insensitively), unless the user dismissed it for this input. Selection
/// is preserved across recomputes when the highlighted template still matches,
/// otherwise it resets to the top.
pub(super) fn recompute_completion(app: &mut App) {
    if app.completion_dismissed {
        app.completion = None;
        return;
    }
    let matches = matching_commands(app.input.as_str());
    if matches.is_empty() {
        app.completion = None;
        return;
    }
    let prev = app
        .completion
        .as_ref()
        .and_then(|c| c.matches.get(c.selected).map(|(t, _)| *t));
    let selected = prev
        .and_then(|t| matches.iter().position(|(m, _)| *m == t))
        .unwrap_or(0);
    app.completion = Some(Completion { matches, selected });
}

/// Insert the highlighted completion row into the input box (cursor to end)
/// and recompute — so accepting `/agent ` immediately reveals its subcommands.
/// No-op when the selected row wouldn't extend the input (it's a reference row
/// for a command whose arguments the user is already typing), so Tab there
/// doesn't wipe what they've written.
fn accept_completion(app: &mut App) {
    let Some(tpl) = app
        .completion
        .as_ref()
        .and_then(|c| c.matches.get(c.selected).map(|(t, _)| *t))
    else {
        return;
    };
    if !command_extends(&app.input, tpl) {
        return;
    }
    app.input = tpl.to_string();
    app.cursor = app.input.len();
    recompute_completion(app);
}

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
            app.completion_dismissed = false;
            recompute_completion(app);
        }
        ClickTarget::CompletionSelect(i) => {
            let n = app.completion.as_ref().map_or(0, |c| c.matches.len());
            if i < n {
                if let Some(c) = app.completion.as_mut() {
                    c.selected = i;
                }
                accept_completion(app);
            }
        }
        ClickTarget::ApprovalApprove => apply_approval(app, ApprovalDecision::Approve),
        ClickTarget::ApprovalDeny => apply_approval(app, ApprovalDecision::Deny),
        ClickTarget::ApprovalApproveAll => apply_approval(app, ApprovalDecision::ApproveAll),
        ClickTarget::PickerSelect(i) => {
            // Session row `i` is picker display index `i + 1` (row 0 is the
            // New session row). First click selects; second click on the
            // same row opens — mirrors the Up/Down then Enter keyboard flow.
            if i < app.session_list.len() {
                let display = i + 1;
                if app.picker_index == display {
                    return Some(MouseOutcome::PickerOpenSelected);
                }
                app.picker_index = display;
            }
        }
        ClickTarget::PickerNew => {
            if app.picker_index == 0 {
                return Some(MouseOutcome::PickerOpenSelected);
            }
            app.picker_index = 0;
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
            // With the popup open, Enter completes the highlighted command
            // while there's still more of it to type. Once it's fully typed
            // (or you're filling in arguments) it falls through and submits,
            // so a complete command still runs on one Enter.
            if let Some(c) = app.completion.as_ref()
                && let Some((tpl, _)) = c.matches.get(c.selected)
                && command_extends(&app.input, tpl)
            {
                accept_completion(app);
                return None;
            }
            if !app.input.is_empty() {
                let text = std::mem::take(&mut app.input);
                app.cursor = 0;
                app.completion = None;
                app.completion_dismissed = false;
                return parse_chat_line(app, &text);
            }
        }
        KeyCode::Tab => {
            // Open the popup if it isn't already (user typed `/agent ` then
            // paused), then insert the highlighted command. Selection is
            // moved with the arrow keys.
            if app.completion.is_none() {
                recompute_completion(app);
            }
            if app.completion.is_some() {
                accept_completion(app);
            }
        }
        KeyCode::BackTab => {
            // Shift+Tab moves the selection up, mirroring Up.
            if let Some(c) = app.completion.as_mut() {
                let n = c.matches.len();
                if n > 0 {
                    c.selected = (c.selected + n - 1) % n;
                }
            }
        }
        KeyCode::Char(c) => {
            app.input.insert(app.cursor, c);
            app.cursor += c.len_utf8();
            app.completion_dismissed = false;
            recompute_completion(app);
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
                app.completion_dismissed = false;
                recompute_completion(app);
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
            // When the completion popup is open, arrows move the selection;
            // otherwise they scroll the chat history as before.
            if let Some(c) = app.completion.as_mut() {
                let n = c.matches.len();
                if n > 0 {
                    c.selected = (c.selected + n - 1) % n;
                }
            } else {
                let off = &mut app.active_mut().scroll_offset;
                *off = off.saturating_add(3);
            }
        }
        KeyCode::Down => {
            if let Some(c) = app.completion.as_mut() {
                let n = c.matches.len();
                if n > 0 {
                    c.selected = (c.selected + 1) % n;
                }
            } else {
                let off = &mut app.active_mut().scroll_offset;
                *off = off.saturating_sub(3);
            }
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
            // Esc first dismisses the completion popup (keeping the typed
            // text); only a second Esc with no popup quits the TUI.
            if app.completion.is_some() {
                app.completion = None;
                app.completion_dismissed = true;
            } else {
                app.should_quit = true;
            }
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
        "/info" => return Some(ChatAction::Dispatch(Command::Info)),
        "/costs" => return Some(ChatAction::Dispatch(Command::ListCosts)),
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
    if text == "/agent room" {
        return Some(ChatAction::Dispatch(Command::AgentRoom));
    }

    // `/memory …` is wholly owned by the memory extension — every
    // subcommand (list/new/delete/grant/revoke/share/unshare/import/
    // attach/detach/config) routes through `Command::Extension`, dispatched
    // to `extensions::memory::MemoryCommand`. Completion hints for these
    // subcommands live in the help table above.

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

    // --- /extensions: per-session framework control ---
    if text == "/extensions" || text == "/extensions list" {
        return Some(ChatAction::Dispatch(Command::Extensions(
            ExtensionsAction::List,
        )));
    }
    if let Some(arg) = text.strip_prefix("/extensions add ") {
        let name = arg.trim().to_string();
        if name.is_empty() {
            show_error(app, "Usage: /extensions add <name>".into());
            return None;
        }
        return Some(ChatAction::Dispatch(Command::Extensions(
            ExtensionsAction::Add(name),
        )));
    }
    if let Some(arg) = text.strip_prefix("/extensions remove ") {
        let name = arg.trim().to_string();
        if name.is_empty() {
            show_error(app, "Usage: /extensions remove <name>".into());
            return None;
        }
        return Some(ChatAction::Dispatch(Command::Extensions(
            ExtensionsAction::Remove(name),
        )));
    }
    if let Some(arg) = text.strip_prefix("/extensions settings ") {
        let name = arg.trim().to_string();
        if name.is_empty() {
            show_error(app, "Usage: /extensions settings <name>".into());
            return None;
        }
        return Some(ChatAction::Dispatch(Command::Extensions(
            ExtensionsAction::Settings(name),
        )));
    }
    if let Some(arg) = text.strip_prefix("/extensions set ") {
        let mut parts = arg.trim().splitn(3, char::is_whitespace);
        let name = parts.next().unwrap_or("").trim();
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if name.is_empty() || key.is_empty() || value.is_empty() {
            show_error(app, "Usage: /extensions set <name> <key> <value>".into());
            return None;
        }
        return Some(ChatAction::Dispatch(Command::Extensions(
            ExtensionsAction::Set {
                name: name.to_string(),
                key: key.to_string(),
                value: value.to_string(),
            },
        )));
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

    if let Some(stripped) = text.strip_prefix('/') {
        // Unknown built-in — route to extension command dispatch.
        // `dispatch` will produce a `CommandOutcome::Error` if no
        // extension registered this name.
        let (name, args) = match stripped.split_once(char::is_whitespace) {
            Some((n, a)) => (n.to_string(), a.trim().to_string()),
            None => (stripped.to_string(), String::new()),
        };
        if name.is_empty() {
            show_error(app, "Empty command".to_string());
            return None;
        }
        return Some(ChatAction::Dispatch(Command::Extension { name, args }));
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
            if app.picker_index + 1 < app.picker_len() {
                app.picker_index += 1;
            }
            None
        }
        KeyCode::Enter => Some(app.picker_selection()),
        KeyCode::Char('n') => Some("__new__".to_string()),
        KeyCode::Char('r') => {
            // Row 0 is "New session" — nothing to rename there.
            if let Some(info) = app
                .picker_index
                .checked_sub(1)
                .and_then(|i| app.session_list.get(i))
            {
                let initial = info.name.clone().unwrap_or_default();
                let cursor = initial.len();
                let title = match &info.name {
                    Some(n) => format!("Rename \"{n}\""),
                    None => format!(
                        "Name session {}",
                        super::short_session_id(&info.session_db_id)
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

#[cfg(test)]
mod tests {
    use super::{command_catalog, command_extends, matching_commands};
    use std::collections::HashSet;

    #[test]
    fn catalog_templates_are_well_formed() {
        let mut seen: HashSet<&str> = HashSet::new();
        for (tpl, desc) in command_catalog() {
            if let Some(h) = tpl.strip_prefix('#') {
                assert!(!h.trim().is_empty(), "empty section header");
                assert!(desc.is_empty(), "header {tpl:?} should have no description");
                continue;
            }
            assert!(tpl.starts_with('/'), "command {tpl:?} must start with '/'");
            assert!(!desc.is_empty(), "command {tpl:?} missing description");
            assert!(
                tpl.trim() == tpl || tpl.ends_with(' '),
                "bad spacing in {tpl:?}"
            );
            assert!(seen.insert(tpl), "duplicate catalog template {tpl:?}");
        }
    }

    #[test]
    fn matching_requires_slash_prefix() {
        assert!(matching_commands("hello").is_empty());
        assert!(matching_commands("").is_empty());
    }

    #[test]
    fn matching_is_prefix_and_case_insensitive() {
        let m = matching_commands("/ag");
        assert!(m.iter().any(|(t, _)| *t == "/agents"));
        assert!(m.iter().any(|(t, _)| *t == "/agent add "));
        assert!(m.iter().all(|(t, _)| t.to_lowercase().starts_with("/ag")));
        // No headers ever leak into completion results.
        assert!(m.iter().all(|(t, _)| !t.starts_with('#')));
        // Case-insensitive against the catalog.
        assert!(!matching_commands("/AGENTS").is_empty());
    }

    #[test]
    fn matching_narrows_to_subcommands() {
        let m = matching_commands("/agent ");
        assert!(m.iter().any(|(t, _)| *t == "/agent add "));
        assert!(m.iter().any(|(t, _)| *t == "/agent remove "));
        // "/agents" is not a "/agent " subcommand.
        assert!(m.iter().all(|(t, _)| *t != "/agents"));
    }

    #[test]
    fn fully_typed_command_stays_visible() {
        // A complete command keeps its row + description on screen.
        let m = matching_commands("/quit");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, "/quit");
        // A shorter prefix still lists it for completion.
        assert!(matching_commands("/q").iter().any(|(t, _)| *t == "/quit"));
    }

    #[test]
    fn command_stays_visible_while_typing_arguments() {
        // Past the template, typing an argument: the command + its
        // description stays as a single reference row.
        let m = matching_commands("/agent add my-bot");
        assert_eq!(
            m.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            ["/agent add "]
        );

        // Most-specific template wins over a shorter prefix.
        let m = matching_commands("/sharing approve abc123");
        assert_eq!(
            m.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            ["/sharing approve "]
        );
    }

    #[test]
    fn extends_only_while_command_incomplete() {
        // Strict prefix → Tab/Enter should complete it.
        assert!(command_extends("/q", "/quit"));
        assert!(command_extends("/agent a", "/agent add "));
        // Fully typed or typing args → leave the text alone.
        assert!(!command_extends("/quit", "/quit"));
        assert!(!command_extends("/agent add foo", "/agent add "));
    }
}
