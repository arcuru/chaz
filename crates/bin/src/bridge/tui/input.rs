//! Input handling: key events → `ChatAction`, slash-command parsing,
//! help text, session-picker navigation. No async, no side effects beyond
//! mutating the shared `App` state.

use chaz_core::bridge::ApprovalDecision;
use chaz_core::commands::Parsed;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::{
    App, ChatAction, ClickTarget, Completion, ModelPickerScope, Overlay, SettingsFocus,
    SettingsPicker, SettingsPickerIntent, SettingsPrompt, SettingsPromptIntent, SettingsScope,
    TuiMode, show_error, show_system_msg,
};

/// Grouped, ordered catalog of every built-in slash command. Single source of
/// truth shared by the help overlay (which renders the `#`-prefixed section
/// headers) and inline completion (which skips them). Templates ending in a
/// space take an argument; the help overlay and completion both insert the
/// template verbatim so the cursor lands ready for that argument.
///
/// This is the TUI's **presentation layer**, not a mirror of
/// [`chaz_core::commands::parse`]: it adds section grouping, human descriptions,
/// the view-local verbs `parse` deliberately omits (`/clear`, `/settings`, …),
/// and the extension verbs `parse` routes generically (`/memory …`). It is
/// therefore not deduplicated into the grammar. The one invariant that *must*
/// hold — every row claiming to be a shared built-in still resolves to one
/// rather than silently rotting into the `Command::Extension` fallback — is
/// pinned by the `catalog_rows_match_shared_grammar` test below.
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
        (
            "/agent reload",
            "re-read yaml + reconcile agent config(s) [ref]",
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
        (
            "/agent rehost ",
            "reassign home peer [--agent] [--clear] <ref> [pubkey]",
        ),
        ("/agent home-status", "list home_pubkey per agent + session"),
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
        ("/models", "open the Models settings page"),
        (
            "/model ",
            "show, or set <id> | <agent> <id> | <agent> clear",
        ),
        ("/role ", "show, select, or define a role"),
        ("/backend ", "add a custom backend (<name> <url> <key>)"),
        ("/backends", "list known backends and models"),
        ("# TUI", ""),
        (
            "/settings",
            "open Session Settings (Peer Settings from session list)",
        ),
        ("/clear", "clear display (entries still in DB)"),
        ("/raw", "dump raw entry data for debugging"),
        ("/debug", "toggle debug mode (Ctrl+D)"),
        ("/expand", "toggle expand/collapse tool calls (Ctrl+T)"),
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
    /// Apply the currently selected model picker row — equivalent to
    /// pressing Enter in the model picker.
    ModelPickerOpenSelected,
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
        ClickTarget::ToggleEntryExpanded(i) => {
            let set = &mut app.active_mut().expanded_entries;
            if !set.remove(&i) {
                set.insert(i);
            }
        }
        ClickTarget::ModelPickerSelect(i) => {
            // Same dance as the session picker: first click selects, second
            // click on the same row commits. `i` indexes the filtered
            // (post-search) list — view sets it that way.
            if i < app.model_picker_filtered.len() {
                if app.model_picker_index == i {
                    return Some(MouseOutcome::ModelPickerOpenSelected);
                }
                app.model_picker_index = i;
            }
        }
        ClickTarget::SettingsSidebarItem(i) => {
            // Mouse-switch a Settings category. Pins focus to the sidebar
            // so the new category's arrow-key behavior is predictable.
            if let TuiMode::Settings(scope) = app.mode {
                let n = app.settings_category_count(scope);
                if i < n {
                    app.set_settings_index(scope, i);
                    app.settings_focus = SettingsFocus::Sidebar;
                    app.settings_status = None;
                }
            }
        }
        ClickTarget::SettingsDetailRow(i) => {
            // Click inside the active category's inner list. Focuses the
            // detail pane and moves the appropriate per-category cursor.
            if let TuiMode::Settings(scope) = app.mode {
                let cat = app.settings_index(scope);
                set_settings_detail_cursor(app, scope, cat, i);
                app.settings_focus = SettingsFocus::Detail;
                app.settings_status = None;
            }
        }
    }
    None
}

/// Move the per-category inner cursor to `row`, clamped to the live list
/// length. Used by mouse clicks on detail rows; the keyboard path uses
/// `bump_inner_cursor` instead because it always moves by ±1.
fn set_settings_detail_cursor(app: &mut App, scope: SettingsScope, cat: usize, row: usize) {
    let Some(len) = settings_inner_list_len(app, scope, cat) else {
        return;
    };
    if len == 0 {
        return;
    }
    let clamped = row.min(len - 1);
    use super::{PeerSettingsCategory, SessionSettingsCategory};
    match scope {
        SettingsScope::Peer => match PeerSettingsCategory::ALL.get(cat) {
            Some(PeerSettingsCategory::Agents) => app.peer_agents_cursor = clamped,
            Some(PeerSettingsCategory::Defaults) => app.peer_defaults_cursor = clamped,
            Some(PeerSettingsCategory::Mcp) => app.peer_mcp_cursor = clamped,
            _ => {}
        },
        SettingsScope::Session => match SessionSettingsCategory::ALL.get(cat) {
            Some(SessionSettingsCategory::Agents) => app.session_agents_cursor = clamped,
            Some(SessionSettingsCategory::Models) => app.session_models_cursor = clamped,
            _ => {}
        },
    }
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
    // View-local commands are intercepted before the shared parser. They
    // either have no `Command` equivalent or mean something richer in the TUI
    // (an interactive overlay or a local toggle rather than a one-shot session
    // op). Where a verb collides with a shared one — `/sessions` here vs.
    // `Command::ListSessions` — this first look shadows the shared mapping for
    // the TUI only.
    match text {
        "/sessions" | "/s" => return Some(ChatAction::OpenPicker),
        "/models" => return Some(ChatAction::OpenModelsSettings),
        "/settings" => return Some(ChatAction::OpenSettings(SettingsScope::Session)),
        "/clear" => {
            let tab = app.active_mut();
            tab.entries.clear();
            tab.scroll_offset = 0;
            tab.expanded_entries.clear();
            return None;
        }
        "/debug" => {
            app.debug_mode = !app.debug_mode;
            return None;
        }
        "/expand" => {
            app.expand_all = !app.expand_all;
            return None;
        }
        "/raw" => {
            let mut raw = String::new();
            for (i, entry) in app.active().entries.iter().enumerate() {
                let ts = entry.timestamp.format("%H:%M:%S%.3f");
                let typ = format!("{:?}", entry.entry_type);
                let t = chaz_core::util::truncate_chars(&entry.content, 80);
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

    // Everything else routes through the shared, transport-neutral grammar.
    // `/memory …` and any other unknown `/foo` come back as
    // `Command::Extension`, which the dispatcher routes to the extension hub.
    match chaz_core::commands::parse(text) {
        Parsed::Command(cmd) => Some(ChatAction::Dispatch(cmd)),
        Parsed::Usage(msg) => {
            show_error(app, msg);
            None
        }
        Parsed::NotCommand => Some(ChatAction::SendMessage(text.to_string())),
    }
}

/// What a key in the model picker meant.
pub(super) enum ModelPickerKey {
    /// User confirmed a selection — caller switches to the chosen model.
    Select(String),
    /// User asked to refetch the catalog (skip cache).
    Refresh,
    /// Navigation / typing / dismiss / unhandled — nothing for the caller
    /// to do beyond the in-place mutations already applied to `app`.
    None,
}

/// Key handler for `TuiMode::ModelPicker`. Typing is fuzzy-search input;
/// arrow keys navigate the filtered list; Ctrl+R refreshes; Enter
/// commits; Esc dismisses. The picker is opened from chat mode via
/// the `/models` slash command (no global keybinding — Ctrl+M is
/// ambiguous with Enter on terminals without keyboard-enhancement
/// support, which makes a key binding unreliable through tmux + ssh).
pub(super) fn handle_model_picker_key(app: &mut App, key: KeyEvent) -> ModelPickerKey {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        // Refresh — Ctrl+R, disabled while a fetch is already running so
        // we don't pile up duplicate requests.
        KeyCode::Char('r') if ctrl && !app.model_picker_loading => ModelPickerKey::Refresh,
        // Clear search — Ctrl+U mirrors readline.
        KeyCode::Char('u') if ctrl => {
            app.model_search.clear();
            app.recompute_model_filter();
            ModelPickerKey::None
        }

        KeyCode::Up => {
            if app.model_picker_index > 0 {
                app.model_picker_index -= 1;
            }
            ModelPickerKey::None
        }
        KeyCode::Down => {
            if app.model_picker_index + 1 < app.model_picker_filtered.len() {
                app.model_picker_index += 1;
            }
            ModelPickerKey::None
        }
        KeyCode::PageUp => {
            app.model_picker_index = app.model_picker_index.saturating_sub(10);
            ModelPickerKey::None
        }
        KeyCode::PageDown => {
            let last = app.model_picker_filtered.len().saturating_sub(1);
            app.model_picker_index = (app.model_picker_index + 10).min(last);
            ModelPickerKey::None
        }
        KeyCode::Home => {
            app.model_picker_index = 0;
            ModelPickerKey::None
        }
        KeyCode::End => {
            app.model_picker_index = app.model_picker_filtered.len().saturating_sub(1);
            ModelPickerKey::None
        }

        KeyCode::Enter => app
            .model_picker_selection()
            .map(ModelPickerKey::Select)
            .unwrap_or(ModelPickerKey::None),
        KeyCode::Esc => {
            // Bounce back to whoever opened the picker — chat by default;
            // Session Settings when invoked from there.
            app.mode = app.model_picker_caller;
            ModelPickerKey::None
        }

        KeyCode::Backspace => {
            if app.model_search.pop().is_some() {
                app.recompute_model_filter();
            }
            ModelPickerKey::None
        }
        // Typed character — append to search query. Skip control/alt
        // modifiers so e.g. Ctrl+T doesn't smuggle a 't' into the query.
        KeyCode::Char(c) if !ctrl && !alt => {
            app.model_search.push(c);
            app.recompute_model_filter();
            ModelPickerKey::None
        }

        _ => ModelPickerKey::None,
    }
}

/// Result of routing a key through the Settings page. Most operations
/// mutate `App` in place and return `None`; the variants here capture the
/// few actions that need a roundtrip through the main loop (async DB
/// reads, command dispatch).
pub(super) enum SettingsKey {
    None,
    /// User pressed Enter to open the model picker. When `None`, the
    /// main loop derives the scope from the cursor (Session→Models path).
    /// `Some(scope)` is used when the scope is known ahead of time
    /// (e.g. Peer→Agents → AgentGlobal).
    OpenModelPicker(Option<ModelPickerScope>),
    /// Dispatch a backend command on behalf of the Settings page. Used
    /// for direct-action keys like `[d]` remove on the session-agents
    /// list — no prompt, just fire and refresh.
    DispatchCommand(chaz_core::commands::Command),
    /// User submitted a bottom-strip prompt with the given intent and
    /// payload (already trimmed). Main loop turns this into the right
    /// `Command::…` and dispatches.
    PromptSubmit {
        intent: SettingsPromptIntent,
        value: String,
    },
    /// Reload one agent's declarative fields from chaz yaml. Triggered by
    /// `[r]` on the Peer→Agents detail. Payload is the agent display
    /// name. Main loop re-reads the config file, builds an `Agent` from
    /// the matching yaml entry, and upserts into the registry.
    ReloadPeerAgent {
        name: String,
    },
    /// Replace the persisted peer-level `default_agents` list. Triggered
    /// by [d] / Ctrl+↑↓ / submitted [a] prompt on Peer→Defaults. Main
    /// loop applies via Server::set_default_agents and persists to
    /// `chaz_peer`.
    WritePeerDefaults(Vec<String>),
}

/// Key handler for `TuiMode::Settings`. Navigation is focus-aware:
///   - Sidebar focus (default): `↑`/`↓` cycle categories, `→` / `Enter`
///     dive into the active category's inner list (only when it owns one),
///     `Esc` exits Settings.
///   - Detail focus: `↑`/`↓` move the inner cursor, `←` returns focus to
///     the sidebar, `Esc` exits Settings.
///
/// `Tab`/`BackTab` always cycle categories and snap focus back to the
/// sidebar — a stable escape hatch from any inner-list state.
pub(super) fn handle_settings_key(
    app: &mut App,
    key: KeyEvent,
    scope: SettingsScope,
) -> SettingsKey {
    // When a bottom-strip prompt is active, route keys to it instead of
    // category navigation. Submit returns a PromptSubmit; Esc cancels.
    if app.settings_prompt.is_some() {
        return handle_settings_prompt_key(app, key);
    }
    // Same gate for the picker — mutually exclusive with the prompt.
    if app.settings_picker.is_some() {
        return handle_settings_picker_key(app, key);
    }

    let n = app.settings_category_count(scope);
    if n == 0 {
        return SettingsKey::None;
    }
    let cur = app.settings_index(scope);
    let inner_list_len = settings_inner_list_len(app, scope, cur);

    // Per-category direct-action keys ([a]/[d] on the Session Agents
    // list, [r] on the Peer Agents list). Check before falling through
    // to general navigation so typing one of these doesn't move the
    // sidebar.
    if matches!(scope, SettingsScope::Session)
        && matches!(
            super::SessionSettingsCategory::ALL.get(cur),
            Some(super::SessionSettingsCategory::Agents)
        )
    {
        match key.code {
            KeyCode::Char('a') => {
                // Source candidates from the peer-registry list — already
                // refreshed at the top of every Settings frame in
                // `view::ui`. Filter out anything attached to this session
                // so the picker only shows actionable adds.
                let attached: std::collections::HashSet<String> = app
                    .session_settings_snapshot
                    .as_ref()
                    .map(|s| s.agents.iter().map(|a| a.display_name.clone()).collect())
                    .unwrap_or_default();
                let candidates: Vec<String> = app
                    .peer_agents_names
                    .iter()
                    .filter(|n| !attached.contains(*n))
                    .cloned()
                    .collect();
                app.settings_prompt = None;
                app.settings_picker = Some(SettingsPicker {
                    label: "add agent".to_string(),
                    filter: String::new(),
                    cursor: 0,
                    candidates,
                    selected: 0,
                    intent: SettingsPickerIntent::AddSessionAgent,
                });
                return SettingsKey::None;
            }
            KeyCode::Char('d') => {
                if let Some(name) = app
                    .session_settings_snapshot
                    .as_ref()
                    .and_then(|s| s.agents.get(app.session_agents_cursor))
                    .map(|a| a.display_name.clone())
                {
                    return SettingsKey::DispatchCommand(
                        chaz_core::commands::Command::AgentRemove(name),
                    );
                }
                return SettingsKey::None;
            }
            _ => {}
        }
    }
    if matches!(scope, SettingsScope::Peer)
        && matches!(
            super::PeerSettingsCategory::ALL.get(cur),
            Some(super::PeerSettingsCategory::Agents)
        )
        && let KeyCode::Char('r') = key.code
    {
        if let Some(name) = app.peer_agents_names.get(app.peer_agents_cursor).cloned() {
            return SettingsKey::ReloadPeerAgent { name };
        }
        return SettingsKey::None;
    }
    if matches!(scope, SettingsScope::Peer)
        && matches!(
            super::PeerSettingsCategory::ALL.get(cur),
            Some(super::PeerSettingsCategory::Defaults)
        )
    {
        let len = app.peer_defaults.len();
        let cursor = app.peer_defaults_cursor.min(len.saturating_sub(1));
        match key.code {
            KeyCode::Char('a') => {
                app.settings_prompt = Some(SettingsPrompt {
                    label: "add default agent".to_string(),
                    input: String::new(),
                    cursor: 0,
                    intent: SettingsPromptIntent::AddPeerDefault,
                });
                return SettingsKey::None;
            }
            KeyCode::Char('d') if len > 0 => {
                let mut next = app.peer_defaults.clone();
                next.remove(cursor);
                return SettingsKey::WritePeerDefaults(next);
            }
            // Ctrl+Up / Ctrl+Down reorder the selected row. No-op at the
            // ends — the routing host is first, so users almost always
            // want to bump items toward the top.
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) && cursor > 0 => {
                let mut next = app.peer_defaults.clone();
                next.swap(cursor, cursor - 1);
                app.peer_defaults_cursor = cursor - 1;
                return SettingsKey::WritePeerDefaults(next);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) && cursor + 1 < len => {
                let mut next = app.peer_defaults.clone();
                next.swap(cursor, cursor + 1);
                app.peer_defaults_cursor = cursor + 1;
                return SettingsKey::WritePeerDefaults(next);
            }
            _ => {}
        }
    }

    // Any navigation key clears a leftover one-shot status message
    // (`settings_status`). Action keys above set it; nav keys below
    // sweep it. Done before the per-key dispatch so the state machine
    // is uniform.
    if matches!(
        key.code,
        KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Enter
            | KeyCode::Esc
            | KeyCode::Char(_)
    ) {
        app.settings_status = None;
    }

    let focus = app.settings_focus;

    match key.code {
        KeyCode::Esc => {
            app.close_settings();
            SettingsKey::None
        }
        // Tab / BackTab always cycle categories and force focus back to
        // the sidebar, so the user can always get unstuck regardless of
        // which pane currently owns arrow keys.
        KeyCode::Tab => {
            app.set_settings_index(scope, (cur + 1) % n);
            app.settings_focus = SettingsFocus::Sidebar;
            SettingsKey::None
        }
        KeyCode::BackTab => {
            app.set_settings_index(scope, (cur + n - 1) % n);
            app.settings_focus = SettingsFocus::Sidebar;
            SettingsKey::None
        }
        KeyCode::Down => match focus {
            SettingsFocus::Sidebar => {
                app.set_settings_index(scope, (cur + 1) % n);
                SettingsKey::None
            }
            SettingsFocus::Detail => {
                if let Some(len) = inner_list_len {
                    bump_inner_cursor(app, scope, cur, 1, len);
                }
                SettingsKey::None
            }
        },
        KeyCode::Up => match focus {
            SettingsFocus::Sidebar => {
                app.set_settings_index(scope, (cur + n - 1) % n);
                SettingsKey::None
            }
            SettingsFocus::Detail => {
                if let Some(len) = inner_list_len {
                    bump_inner_cursor(app, scope, cur, -1, len);
                }
                SettingsKey::None
            }
        },
        // Right enters the detail pane when the active category owns an
        // inner list with at least one row. No-op otherwise — there's
        // nothing for arrows to land on.
        KeyCode::Right => {
            if matches!(focus, SettingsFocus::Sidebar) && inner_list_len.is_some_and(|len| len > 0)
            {
                app.settings_focus = SettingsFocus::Detail;
            }
            SettingsKey::None
        }
        // Left pops focus back to the sidebar from the detail pane.
        KeyCode::Left => {
            if matches!(focus, SettingsFocus::Detail) {
                app.settings_focus = SettingsFocus::Sidebar;
            }
            SettingsKey::None
        }
        KeyCode::Home => {
            app.set_settings_index(scope, 0);
            app.settings_focus = SettingsFocus::Sidebar;
            SettingsKey::None
        }
        KeyCode::End => {
            app.set_settings_index(scope, n - 1);
            app.settings_focus = SettingsFocus::Sidebar;
            SettingsKey::None
        }
        // Number keys 1..=9 jump straight to that category (only when the
        // index exists). Saves a stab at Tab when you know where you want
        // to be. Snaps focus back to the sidebar so the new category
        // starts in its default state.
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = (c as usize) - ('1' as usize);
            if idx < n {
                app.set_settings_index(scope, idx);
                app.settings_focus = SettingsFocus::Sidebar;
            }
            SettingsKey::None
        }
        KeyCode::Enter => {
            // Session→Models: Enter opens the picker regardless of focus —
            // it's the page's primary action.
            if matches!(scope, SettingsScope::Session)
                && matches!(
                    super::SessionSettingsCategory::ALL.get(cur),
                    Some(super::SessionSettingsCategory::Models)
                )
            {
                return SettingsKey::OpenModelPicker(None);
            }
            // Peer→Agents in detail focus: Enter opens the picker for
            // agent-global model (DB-level `AgentDbConfig.model`).
            if matches!(scope, SettingsScope::Peer)
                && matches!(
                    super::PeerSettingsCategory::ALL.get(cur),
                    Some(super::PeerSettingsCategory::Agents)
                )
                && matches!(focus, SettingsFocus::Detail)
                && let Some(name) = app.peer_agents_names.get(app.peer_agents_cursor).cloned()
            {
                return SettingsKey::OpenModelPicker(Some(ModelPickerScope::AgentGlobal(name)));
            }
            // Otherwise Enter on the sidebar dives into the detail pane
            // when one exists — same effect as Right.
            if matches!(focus, SettingsFocus::Sidebar) && inner_list_len.is_some_and(|len| len > 0)
            {
                app.settings_focus = SettingsFocus::Detail;
            }
            SettingsKey::None
        }
        _ => SettingsKey::None,
    }
}

/// Route a key to the active bottom-strip prompt. Mirrors the rename
/// overlay's input handling — typing inserts at cursor, Backspace deletes
/// the previous char, arrows move the cursor, Enter submits (trimmed),
/// Esc cancels and clears the prompt.
/// Route a key to the active `settings_picker`. Returns
/// `SettingsKey::PromptSubmit` on Enter with a selected candidate (reusing
/// the existing prompt-submit dispatch path), `None` for all other keys.
fn handle_settings_picker_key(app: &mut App, key: KeyEvent) -> SettingsKey {
    let Some(picker) = app.settings_picker.as_mut() else {
        return SettingsKey::None;
    };
    match key.code {
        KeyCode::Esc => {
            app.settings_picker = None;
        }
        KeyCode::Enter => {
            let chosen = picker.selected_name().map(|s| s.to_string());
            let intent = picker.intent;
            app.settings_picker = None;
            if let Some(value) = chosen {
                let prompt_intent = match intent {
                    SettingsPickerIntent::AddSessionAgent => SettingsPromptIntent::AddSessionAgent,
                };
                return SettingsKey::PromptSubmit {
                    intent: prompt_intent,
                    value,
                };
            }
        }
        KeyCode::Up => {
            if picker.selected > 0 {
                picker.selected -= 1;
            }
        }
        KeyCode::Down => {
            let filtered_len = picker.filtered().len();
            if filtered_len > 0 && picker.selected + 1 < filtered_len {
                picker.selected += 1;
            }
        }
        KeyCode::Char(c) => {
            picker.filter.insert(picker.cursor, c);
            picker.cursor += c.len_utf8();
            picker.selected = 0;
        }
        KeyCode::Backspace => {
            if picker.cursor > 0 {
                let prev = picker.filter[..picker.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                picker.filter.drain(prev..picker.cursor);
                picker.cursor = prev;
                picker.selected = 0;
            }
        }
        KeyCode::Left => {
            if picker.cursor > 0 {
                picker.cursor = picker.filter[..picker.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
        }
        KeyCode::Right => {
            if picker.cursor < picker.filter.len() {
                picker.cursor = picker.filter[picker.cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| picker.cursor + i)
                    .unwrap_or(picker.filter.len());
            }
        }
        KeyCode::Home => picker.cursor = 0,
        KeyCode::End => picker.cursor = picker.filter.len(),
        _ => {}
    }
    SettingsKey::None
}

fn handle_settings_prompt_key(app: &mut App, key: KeyEvent) -> SettingsKey {
    let Some(prompt) = app.settings_prompt.as_mut() else {
        return SettingsKey::None;
    };
    match key.code {
        KeyCode::Esc => {
            app.settings_prompt = None;
        }
        KeyCode::Enter => {
            let value = prompt.input.trim().to_string();
            let intent = prompt.intent;
            app.settings_prompt = None;
            if !value.is_empty() {
                return SettingsKey::PromptSubmit { intent, value };
            }
        }
        KeyCode::Char(c) => {
            prompt.input.insert(prompt.cursor, c);
            prompt.cursor += c.len_utf8();
        }
        KeyCode::Backspace => {
            if prompt.cursor > 0 {
                let prev = prompt.input[..prompt.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                prompt.input.drain(prev..prompt.cursor);
                prompt.cursor = prev;
            }
        }
        KeyCode::Left => {
            if prompt.cursor > 0 {
                prompt.cursor = prompt.input[..prompt.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
        }
        KeyCode::Right => {
            if prompt.cursor < prompt.input.len() {
                prompt.cursor = prompt.input[prompt.cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| prompt.cursor + i)
                    .unwrap_or(prompt.input.len());
            }
        }
        KeyCode::Home => prompt.cursor = 0,
        KeyCode::End => prompt.cursor = prompt.input.len(),
        _ => {}
    }
    SettingsKey::None
}

/// Returns the length of the inner-list owned by the active category, or
/// `None` when no list is present (the category is static content or a
/// placeholder). Drives whether `↑`/`↓` navigate the right pane or the
/// sidebar.
fn settings_inner_list_len(app: &App, scope: SettingsScope, category_idx: usize) -> Option<usize> {
    use super::{PeerSettingsCategory, SessionSettingsCategory};
    match scope {
        SettingsScope::Peer => match PeerSettingsCategory::ALL.get(category_idx)? {
            PeerSettingsCategory::Agents => Some(peer_agent_count(app)),
            PeerSettingsCategory::Defaults => Some(app.peer_defaults.len()),
            PeerSettingsCategory::Mcp => Some(app.peer_mcp_servers.len()),
            _ => None,
        },
        SettingsScope::Session => match SessionSettingsCategory::ALL.get(category_idx)? {
            SessionSettingsCategory::Agents => app
                .session_settings_snapshot
                .as_ref()
                .map(|s| s.agents.len()),
            SessionSettingsCategory::Models => app
                .session_settings_snapshot
                .as_ref()
                .map(|s| 1 + s.agents.len()),
            _ => None,
        },
    }
}

fn peer_agent_count(app: &App) -> usize {
    // Refreshed at the top of every render frame while Peer Settings is
    // up — see `view::ui`. Reading from this cache keeps the input
    // handler and the renderer indexing the same list.
    app.peer_agents_names.len()
}

fn bump_inner_cursor(
    app: &mut App,
    scope: SettingsScope,
    category_idx: usize,
    delta: i32,
    len: usize,
) {
    if len == 0 {
        return;
    }
    use super::{PeerSettingsCategory, SessionSettingsCategory};
    let cursor_ref: &mut usize = match scope {
        SettingsScope::Peer => match PeerSettingsCategory::ALL.get(category_idx) {
            Some(PeerSettingsCategory::Agents) => &mut app.peer_agents_cursor,
            Some(PeerSettingsCategory::Defaults) => &mut app.peer_defaults_cursor,
            Some(PeerSettingsCategory::Mcp) => &mut app.peer_mcp_cursor,
            _ => return,
        },
        SettingsScope::Session => match SessionSettingsCategory::ALL.get(category_idx) {
            Some(SessionSettingsCategory::Agents) => &mut app.session_agents_cursor,
            Some(SessionSettingsCategory::Models) => &mut app.session_models_cursor,
            _ => return,
        },
    };
    let cur = (*cursor_ref).min(len.saturating_sub(1));
    let n = len as i32;
    *cursor_ref = (cur as i32 + delta).rem_euclid(n) as usize;
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
        KeyCode::Char('s') => {
            // `s` opens Peer Settings — the session list view doubles as
            // the "peer landing page", so its settings surface is the peer
            // scope. Esc inside Settings returns here.
            app.open_settings(super::SettingsScope::Peer, TuiMode::SessionPicker);
            None
        }
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

    /// Drift guard between the presentational catalog and the shared grammar.
    ///
    /// The catalog is not a mirror of `chaz_core::commands::parse` (see its
    /// doc comment), but every row that *claims* to be a shared built-in must
    /// still parse to a concrete `Command`. If someone renames or removes a
    /// verb in `parse` without touching the catalog, that row silently falls to
    /// the `Command::Extension` fallback — completing for a command that no
    /// longer exists. This pins the relationship so such drift fails the build.
    ///
    /// Two small intent-allowlists carry the only state that lives nowhere
    /// else: verbs the TUI intercepts before `parse` (`parse_chat_line`), and
    /// verbs intentionally routed to the extension hub.
    #[test]
    fn catalog_rows_match_shared_grammar() {
        use chaz_core::commands::{Command, Parsed, parse};

        // Verbs `parse_chat_line` handles before reaching the shared grammar.
        const VIEW_LOCAL: &[&str] = &[
            "/sessions",
            "/models",
            "/settings",
            "/clear",
            "/raw",
            "/debug",
            "/expand",
            "/help",
        ];
        // Verbs deliberately dispatched as `Command::Extension`.
        const EXTENSION: &[&str] = &["/memory", "/schedule"];

        for (tpl, _) in command_catalog() {
            if tpl.starts_with('#') {
                continue;
            }
            // First whitespace-delimited segment, e.g. "/agent" or "/memory".
            let head = tpl.split_whitespace().next().unwrap_or(tpl);
            if VIEW_LOCAL.contains(&head) {
                continue;
            }
            // Arg-bearing templates end in a space; feed a dummy arg so the
            // argument arm matches instead of falling through.
            let probe = if tpl.ends_with(' ') {
                format!("{tpl}x")
            } else {
                tpl.to_string()
            };
            // `Usage` (a recognized verb with too few dummy args) counts as
            // "owned by the grammar" just like a concrete `Command` — only the
            // `Extension` fallback signals the verb isn't a real built-in.
            match parse(&probe) {
                Parsed::Command(Command::Extension { .. }) => assert!(
                    EXTENSION.contains(&head),
                    "catalog row {tpl:?} fell through to Command::Extension but isn't an \
                     allowlisted extension verb — did a built-in get renamed in parse?"
                ),
                Parsed::Command(_) | Parsed::Usage(_) => assert!(
                    !EXTENSION.contains(&head),
                    "catalog row {tpl:?} now resolves to a built-in but is listed as an \
                     extension verb — update the EXTENSION allowlist"
                ),
                Parsed::NotCommand => {
                    panic!("catalog row {tpl:?} probed as {probe:?} → NotCommand")
                }
            }
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
