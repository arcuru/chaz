//! A local composer, not a placeholder database. Creation and observation stay
//! in the ordered operation worker; only its matching ticket may adopt the draft.
use super::*;

pub(super) struct View {
    pub ticket: u64,
    pub input: String,
    pub cursor: usize,
    pub status: String,
}

pub(super) fn begin(app: &mut App, ticket: u64) {
    app.creation = Some(View {
        ticket,
        input: String::new(),
        cursor: 0,
        status: "Creating session…".into(),
    });
    app.mode = TuiMode::NewSession;
    app.overlay = None;
    app.selection.clear();
}

pub(super) fn draw(f: &mut ratatui::Frame, app: &App) {
    let Some(view) = &app.creation else { return };
    view::ui_loading_draft(
        f,
        &view.input,
        view.cursor,
        "New session",
        &format!("{}{}", view.status, app.operations.indicator()),
        "Draft not submitted · Ctrl+P hub · Ctrl+C quit",
    );
}

pub(super) fn handle_key(app: &mut App, key: KeyEvent) {
    if !matches!(
        input::handle_overlay_key(app, key),
        input::OverlayKey::NotConsumed
    ) {
        return;
    }
    match key.code {
        KeyCode::F(1) => app.overlay = Some(Overlay::Help { scroll: 0 }),
        KeyCode::Esc => app.mode = TuiMode::SessionPicker,
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.mode = TuiMode::SessionPicker;
        }
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Hiding is not cancellation. A started creation may have committed.
            // Only a settled failed view may be explicitly discarded.
            if !app.operations.is_creating_session() {
                app.creation = None;
            }
            app.mode = TuiMode::SessionPicker;
        }
        _ if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(view) = app.creation.as_mut() {
                input::edit_text(&mut view.input, &mut view.cursor, key);
            }
        }
        _ => {}
    }
}

pub(super) fn matches(app: &App, ticket: u64) -> bool {
    app.creation.as_ref().is_some_and(|v| v.ticket == ticket)
}

pub(super) fn failed(app: &mut App, ticket: u64, error: &str) {
    if let Some(view) = app.creation.as_mut().filter(|v| v.ticket == ticket) {
        view.status = format!(
            "Session not ready: {error}\nCheck the session hub before retrying; no automatic retry. Ctrl+W discards this draft after the action settles."
        );
    }
}

pub(super) fn opened(app: &mut App, ticket: u64, tab: Tab) {
    if !matches(app, ticket) {
        return;
    }
    let view = app.creation.take().unwrap();
    let id = tab.session_db_id.clone();
    let focus = app.mode == TuiMode::NewSession;
    // Background completion must retain the destination and its draft even if
    // the user has moved to another session, but must not move their focus.
    if app.tab_index_for(&id).is_none() {
        app.insert_tab(tab);
    }
    if focus {
        app.set_active_tab(app.tab_index_for(&id).unwrap());
        app.mode = TuiMode::Chat;
        (app.input, app.cursor) = (view.input, view.cursor);
        app.completion = None;
        app.completion_dismissed = false;
        input::recompute_completion(app);
    } else {
        // The first installed tab is already the selected index, even at the
        // hub. Keep its composer in the focused slot, not the parked-draft map.
        if app.current().is_some_and(|t| t.session_db_id == id) {
            (app.input, app.cursor) = (view.input, view.cursor);
            app.completion = None;
        } else {
            app.drafts.insert(id, (view.input, view.cursor));
        }
        app.operations.notice = Some("New session ready in its tab; draft retained".into());
    }
}
