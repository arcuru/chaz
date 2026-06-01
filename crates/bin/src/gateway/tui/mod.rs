//! Terminal UI gateway. Elm-style architecture:
//! - `App` holds global UI state (mode, overlay, input, click regions, tab
//!   list) plus a `Vec<Tab>` where each `Tab` owns one session's state
//!   (entries, scroll, pending approval, session DB handle, etc.).
//! - `Action` is the update message.
//! - `view::ui` renders a frame from `&mut App`.
//! - `input::parse_chat_line` turns typed text into `ChatAction`s.
//!
//! Submodules:
//! - `input` — KeyEvent / MouseEvent handling, slash-command parsing
//! - `view`  — ratatui rendering

use chaz_core::backends::BackendManager;
use chaz_core::commands::{self, Command, CommandContext, CommandOutcome, SessionInfo};
use chaz_core::config::Config;
use chaz_core::gateway::{ApprovalExchange, Gateway};
use chaz_core::security::SecretStore;
use chaz_core::server::Server;
use chaz_core::session::{EntryType, Session, SessionEntry};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

mod input;
mod view;

/// Name used for the TUI's default/home session. Created on first TUI launch,
/// reopened on subsequent launches.
const TUI_DEFAULT_NAME: &str = "tui";

pub struct TuiGateway {
    config: Config,
    secrets: SecretStore,
}

impl TuiGateway {
    pub fn new(config: Config, secrets: SecretStore) -> Self {
        Self { config, secrets }
    }
}

/// Approval routed from the server through a per-tab forwarder, tagged with
/// the owning session DB ID so the TUI knows which tab to show the prompt on.
pub(super) type TaggedApproval = (String, ApprovalExchange);

enum Action {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// A session DB fired an on_write callback — payload is the
    /// session_db_id so we can refresh the right tab.
    SessionChanged(String),
    ApprovalRequest(TaggedApproval),
}

pub(super) enum TuiMode {
    Chat,
    SessionPicker,
}

pub(super) enum ChatAction {
    Dispatch(Command),
    OpenPicker,
    SendMessage(String),
}

pub(super) enum Overlay {
    Help {
        scroll: u16,
    },
    /// Modal input for renaming a session. Submitting an empty string clears
    /// the alias (matches `/name` with no arg).
    RenamePrompt {
        session_db_id: String,
        title: String,
        input: String,
        cursor: usize,
    },
}

/// Inline slash-command completion popup state. Present only while the input
/// starts with `/` and at least one catalog command prefix-matches it (and the
/// user hasn't dismissed it with Esc for the current input).
pub(super) struct Completion {
    /// `(template, description)` pairs from the command catalog whose template
    /// prefix-matches the current input, case-insensitively.
    pub matches: Vec<(&'static str, &'static str)>,
    /// Index into `matches` of the highlighted row.
    pub selected: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ClickTarget {
    OverlayDismiss,
    HelpCommand(&'static str),
    /// Accept completion row `i` into the input box.
    CompletionSelect(usize),
    ApprovalApprove,
    ApprovalDeny,
    ApprovalApproveAll,
    /// Select session-list row `i` (display index is `i + 1` — the New
    /// session row is row 0).
    PickerSelect(usize),
    /// The virtual "New session" row at the top of the picker.
    PickerNew,
    /// Activate tab at the given index.
    TabActivate(usize),
    /// Close tab at the given index.
    TabClose(usize),
    /// Flip the per-entry expand override on the active tab's entry at the
    /// given index. Inverts against `App::expand_all`, so the click always
    /// produces the opposite of whatever's currently rendered.
    ToggleEntryExpanded(usize),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ClickRegion {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub target: ClickTarget,
}

impl ClickRegion {
    pub fn hit(&self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.x + self.w && row >= self.y && row < self.y + self.h
    }
}

/// Per-session state. Each `Tab` wraps one eidetica session database plus the
/// UI state specific to viewing it (scroll position, pending approval, etc.).
pub(super) struct Tab {
    pub session_db_id: String,
    pub session_db: eidetica::Database,
    pub entries: Vec<SessionEntry>,
    pub scroll_offset: u16,
    pub pending_approval: Option<ApprovalExchange>,
    pub waiting: bool,
    pub current_agent: String,
    pub session_name: Option<String>,
    /// Per-entry expand override (entry index → "opposite of `App::expand_all`").
    /// Empty by default; click on an entry's icon toggles its presence here.
    pub expanded_entries: HashSet<usize>,
}

/// Short, human-distinguishable form of a session DB id, used for tab
/// titles, the status bar, and the picker. Session ids share a long common
/// leading prefix, so the first characters are useless for telling sessions
/// apart — show the *trailing* characters (the part that actually differs),
/// marked with a leading `…` so it's clear it's truncated.
pub(super) fn short_session_id(s: &str) -> String {
    let tail = s.rsplit(':').next().unwrap_or(s);
    let n = tail.chars().count();
    if n <= 8 {
        tail.to_string()
    } else {
        let suffix: String = tail.chars().skip(n - 8).collect();
        format!("…{suffix}")
    }
}

impl Tab {
    /// Title shown on the tab bar — session name if set, else a short id.
    pub fn title(&self) -> String {
        match &self.session_name {
            Some(name) => name.clone(),
            None => short_session_id(&self.session_db_id),
        }
    }
}

pub(super) struct App {
    pub(super) mode: TuiMode,
    pub(super) overlay: Option<Overlay>,
    pub(super) click_regions: Vec<ClickRegion>,
    pub(super) input: String,
    pub(super) cursor: usize,
    /// Active slash-command completion popup, if any. Recomputed on every
    /// input edit (see `input::recompute_completion`).
    pub(super) completion: Option<Completion>,
    /// Set when the user dismisses the popup with Esc; suppresses re-opening
    /// until the input is edited again.
    pub(super) completion_dismissed: bool,
    pub(super) tabs: Vec<Tab>,
    pub(super) active_tab: usize,
    pub(super) agent_names: HashSet<String>,
    pub(super) should_quit: bool,
    pub(super) debug_mode: bool,
    /// When true, tool calls / tool results / directives render their full
    /// content. When false (default), they collapse to a one-line summary.
    /// Toggled by Ctrl+T or `/expand`.
    pub(super) expand_all: bool,
    pub(super) session_list: Vec<SessionInfo>,
    /// Lazy cache of `session_list`. The cold-list walk
    /// (`Command::ListSessions`) opens every session DB and folds entries —
    /// fine on a session with 7 catalogs, less so at scale. Set to `true`
    /// after a fresh fetch and patched-in-place when a watched session
    /// fires `on_write`; invalidated wholesale when a session is created.
    /// Sessions not in tabs are assumed stable from this process's
    /// perspective; remote sync writes won't invalidate the cache and the
    /// user can re-open the picker to refresh.
    pub(super) session_list_fresh: bool,
    pub(super) picker_index: usize,
}

impl App {
    fn new(agent_names: HashSet<String>, initial_tab: Tab) -> Self {
        Self {
            mode: TuiMode::Chat,
            overlay: None,
            click_regions: Vec::new(),
            input: String::new(),
            cursor: 0,
            completion: None,
            completion_dismissed: false,
            tabs: vec![initial_tab],
            active_tab: 0,
            agent_names,
            should_quit: false,
            debug_mode: false,
            expand_all: false,
            session_list: Vec::new(),
            session_list_fresh: false,
            picker_index: 0,
        }
    }

    pub(super) fn active(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub(super) fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    /// Find a tab hosting the given session DB id, if any.
    pub(super) fn tab_index_for(&self, session_db_id: &str) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| t.session_db_id == session_db_id)
    }

    /// Number of selectable rows in the session picker: a virtual "New
    /// session" row at index 0, then one row per known session.
    pub(super) fn picker_len(&self) -> usize {
        self.session_list.len() + 1
    }

    /// Resolve the highlighted picker row to a dispatch token: the
    /// `"__new__"` sentinel for the top row, otherwise the session's db id.
    pub(super) fn picker_selection(&self) -> String {
        match self.picker_index.checked_sub(1) {
            None => "__new__".to_string(),
            Some(i) => self
                .session_list
                .get(i)
                .map(|s| s.session_db_id.clone())
                .unwrap_or_else(|| "__new__".to_string()),
        }
    }

    /// Point the picker cursor at `session_db_id` (offset past the New
    /// session row). Falls back to the first session, or the New row when
    /// there are no sessions.
    pub(super) fn focus_picker_on(&mut self, session_db_id: &str) {
        self.picker_index = self
            .session_list
            .iter()
            .position(|s| s.session_db_id == session_db_id)
            .map(|p| p + 1)
            .unwrap_or(if self.session_list.is_empty() { 0 } else { 1 });
    }
}

fn init_terminal() -> anyhow::Result<Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

/// Register a session DB with the server and wire up per-tab notify and
/// approval forwarding. The raw approval channel given to the server is
/// per-session; a spawned forwarder tags each approval with the session_db_id
/// and pushes into the shared TUI approval channel.
async fn setup_session(
    server: &Server,
    session_db: &eidetica::Database,
    backend: BackendManager,
    approval_tx: mpsc::Sender<TaggedApproval>,
    notify_tx: mpsc::Sender<String>,
) -> anyhow::Result<()> {
    let session_db_id = session_db.root_id().to_string();

    // Per-session raw approval channel → tagged forward to shared channel.
    let (raw_tx, mut raw_rx) = mpsc::channel::<ApprovalExchange>(8);
    let forwarder_id = session_db_id.clone();
    let forwarder_tx = approval_tx.clone();
    tokio::spawn(async move {
        while let Some(ex) = raw_rx.recv().await {
            if forwarder_tx.send((forwarder_id.clone(), ex)).await.is_err() {
                break;
            }
        }
    });

    server
        .register_session(session_db, backend, None, Some(raw_tx))
        .await?;

    let notify_id = session_db_id;
    session_db
        .on_write(move |_event, _db| {
            let tx = notify_tx.clone();
            let id = notify_id.clone();
            Box::pin(async move {
                let _ = tx.send(id).await;
                Ok(())
            })
        })?
        .detach();

    Ok(())
}

/// Find an existing session named "tui", or create one and name it.
async fn default_tui_session(
    server: &Server,
) -> anyhow::Result<(chaz_core::types::ConversationId, eidetica::Database)> {
    if let Some(id) = server.registry().find_by_name(TUI_DEFAULT_NAME).await? {
        match server.registry().open_session(&id).await {
            Ok(r) => return Ok(r),
            Err(e) => tracing::warn!(id, "Default TUI session unreadable, recreating: {e}"),
        }
    }
    let (conv_id, db) = server.registry().create_session(Some("tui")).await?;
    let session_db_id = db.root_id().to_string();
    if let Err(e) = server
        .registry()
        .set_session_name(&session_db_id, TUI_DEFAULT_NAME.to_string())
        .await
    {
        tracing::warn!("Failed to name default TUI session: {e}");
    }
    Ok((conv_id, db))
}

/// Build a `Tab` for an already-registered session DB.
async fn build_tab(server: &Server, session_db: eidetica::Database, session_db_id: String) -> Tab {
    let agent = server
        .registry()
        .resolve_agent(&session_db_id, None, server.agent_index())
        .await;
    let session = Session::new(
        chaz_core::types::ConversationId(session_db_id.clone()),
        session_db.clone(),
    )
    .await;
    let session_name = session.read_meta().await.name;
    let entries = session.entries().to_vec();
    Tab {
        session_db_id,
        session_db,
        entries,
        scroll_offset: 0,
        pending_approval: None,
        waiting: false,
        current_agent: agent.name.clone(),
        session_name,
        expanded_entries: HashSet::new(),
    }
}

impl Gateway for TuiGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let (approval_tx, mut approval_rx) = mpsc::channel::<TaggedApproval>(8);
        let (notify_tx, mut notify_rx) = mpsc::channel::<String>(64);

        let (_conv_id, session_db) = default_tui_session(&server).await?;
        let session_db_id = session_db.root_id().to_string();

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        setup_session(
            &server,
            &session_db,
            backend.clone(),
            approval_tx.clone(),
            notify_tx.clone(),
        )
        .await?;

        let agent_names: HashSet<String> = server
            .agents()
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        let initial_tab = build_tab(&server, session_db, session_db_id).await;
        let mut app = App::new(agent_names, initial_tab);

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            original_hook(info);
        }));

        let mut terminal = init_terminal()?;
        let mut events = EventStream::new();

        // When prior sessions exist, open straight into the picker so the
        // user picks one (or the New session row) instead of always landing
        // in the default session. A fresh install — only the just-created
        // empty default session — still goes directly to chat.
        {
            let (sid, sdb, agent, sname) = {
                let t = app.active();
                (
                    t.session_db_id.clone(),
                    t.session_db.clone(),
                    t.current_agent.clone(),
                    t.session_name.clone(),
                )
            };
            let ctx = CommandContext {
                server: &server,
                secrets: &self.secrets,
                backend: &backend,
                session_db_id: &sid,
                session_db: &sdb,
                current_agent: &agent,
                session_name: sname.as_deref(),
            };
            if let CommandOutcome::SessionsList(list) =
                commands::dispatch(Command::ListSessions, &ctx).await
            {
                let has_known = list.len() > 1 || list.iter().any(|s| s.entry_count > 0);
                if has_known {
                    app.session_list = list;
                    app.session_list_fresh = true;
                    // Always land on the "New session" row when the picker
                    // first opens.
                    app.picker_index = 0;
                    app.mode = TuiMode::SessionPicker;
                }
            }
        }

        loop {
            terminal.draw(|f| view::ui(f, &mut app))?;

            let action = tokio::select! {
                Some(Ok(event)) = events.next() => {
                    match event {
                        Event::Key(key) => Action::Key(key),
                        Event::Mouse(m) => Action::Mouse(m),
                        _ => continue,
                    }
                }
                Some(id) = notify_rx.recv() => Action::SessionChanged(id),
                Some(msg) = approval_rx.recv() => Action::ApprovalRequest(msg),
            };

            match action {
                Action::Key(key) => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.should_quit = true;
                    } else if key.code == KeyCode::Char('d')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.debug_mode = !app.debug_mode;
                    } else if key.code == KeyCode::Char('t')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.expand_all = !app.expand_all;
                    } else if key.code == KeyCode::Char('w')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        close_active_tab(&mut app);
                    } else if key.code == KeyCode::Char('p')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+P toggles the session picker. In chat mode it
                        // opens it; in picker mode it dismisses back to chat.
                        match app.mode {
                            TuiMode::Chat => {
                                handle_chat_action(
                                    ChatAction::OpenPicker,
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                )
                                .await;
                            }
                            TuiMode::SessionPicker => {
                                app.mode = TuiMode::Chat;
                            }
                        }
                    } else if key.code == KeyCode::PageUp
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, -1);
                    } else if key.code == KeyCode::PageDown
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, 1);
                    } else {
                        match input::handle_overlay_key(&mut app, key) {
                            input::OverlayKey::Consumed => continue,
                            input::OverlayKey::RenameSubmit {
                                session_db_id,
                                name,
                            } => {
                                apply_picker_rename(
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    session_db_id,
                                    name,
                                )
                                .await;
                                continue;
                            }
                            input::OverlayKey::NotConsumed => {}
                        }
                        match app.mode {
                            TuiMode::Chat => {
                                if let Some(chat_action) =
                                    input::handle_chat_key(&mut app, key).await
                                {
                                    handle_chat_action(
                                        chat_action,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        &approval_tx,
                                        &notify_tx,
                                    )
                                    .await;
                                }
                            }
                            TuiMode::SessionPicker => {
                                if let Some(selected) = input::handle_picker_key(&mut app, key) {
                                    dispatch_picker_selection(
                                        selected,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        &approval_tx,
                                        &notify_tx,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                Action::Mouse(m) => {
                    if let Some(outcome) = input::handle_mouse(&mut app, m) {
                        match outcome {
                            input::MouseOutcome::PickerOpenSelected => {
                                let selected = app.picker_selection();
                                dispatch_picker_selection(
                                    selected,
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                )
                                .await;
                            }
                            input::MouseOutcome::TabActivate(i) => {
                                if i < app.tabs.len() {
                                    app.active_tab = i;
                                }
                            }
                            input::MouseOutcome::TabClose(i) => {
                                close_tab_at(&mut app, i);
                            }
                        }
                    }
                }
                Action::SessionChanged(id) => {
                    if let Some(idx) = app.tab_index_for(&id) {
                        let (db_id, db) = {
                            let tab = &app.tabs[idx];
                            (tab.session_db_id.clone(), tab.session_db.clone())
                        };
                        let session =
                            Session::new(chaz_core::types::ConversationId(db_id.clone()), db).await;
                        let entries = session.entries().to_vec();
                        let meta = session.read_meta().await;

                        // Decide waiting state from the fresh entries before
                        // moving them into the tab.
                        let clear_waiting = entries.last().is_some_and(|latest| {
                            app.agent_names.contains(&latest.sender)
                                && latest.entry_type == EntryType::Message
                        });

                        let tab = &mut app.tabs[idx];
                        tab.entries = entries;
                        tab.session_name = meta.name.clone();
                        if clear_waiting {
                            tab.waiting = false;
                        }

                        // Keep the picker cache in lock-step with this tab's
                        // entries so the next picker open doesn't show stale
                        // counts / cost / name.
                        if let Some(row) = app
                            .session_list
                            .iter_mut()
                            .find(|s| s.session_db_id == db_id)
                        {
                            let entries_ref = &app.tabs[idx].entries;
                            row.entry_count = entries_ref.len();
                            row.name = meta.name.clone();
                            row.agent_name = meta.agent_name.clone();
                            row.last_message =
                                chaz_core::session::summarize_last_message(entries_ref);
                            let (cost, reported, calls) =
                                chaz_core::session::sum_session_cost(entries_ref);
                            row.total_cost_usd = cost;
                            row.cost_reported = reported;
                            row.llm_call_count = calls;
                        }
                    }
                }
                Action::ApprovalRequest((id, exchange)) => {
                    if let Some(idx) = app.tab_index_for(&id) {
                        app.tabs[idx].pending_approval = Some(exchange);
                    } else {
                        // Tab was closed but an approval snuck through — deny
                        // so the runtime doesn't hang waiting.
                        let _ = exchange
                            .decision_tx
                            .send(chaz_core::gateway::ApprovalDecision::Deny);
                    }
                }
            }

            if app.should_quit {
                break;
            }
        }

        restore_terminal();
        Ok(())
    }
}

/// Shift the active tab by `delta` (wraps around).
fn cycle_tab(app: &mut App, delta: i32) {
    if app.tabs.is_empty() {
        return;
    }
    let n = app.tabs.len() as i32;
    let i = (app.active_tab as i32 + delta).rem_euclid(n);
    app.active_tab = i as usize;
}

fn close_active_tab(app: &mut App) {
    close_tab_at(app, app.active_tab);
}

fn close_tab_at(app: &mut App, i: usize) {
    // Refuse to close the last tab — TUI always shows at least one session.
    if app.tabs.len() <= 1 || i >= app.tabs.len() {
        return;
    }
    app.tabs.remove(i);
    if app.active_tab >= app.tabs.len() {
        app.active_tab = app.tabs.len() - 1;
    } else if i < app.active_tab {
        app.active_tab -= 1;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_chat_action(
    action: ChatAction,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) {
    match action {
        ChatAction::SendMessage(text) => {
            let tab = app.active_mut();
            let session_db = tab.session_db.clone();
            let session_db_id = tab.session_db_id.clone();
            let mut session =
                Session::new(chaz_core::types::ConversationId(session_db_id), session_db).await;
            session
                .add_entry(SessionEntry {
                    sender: "user".to_string(),
                    content: text,
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                    metadata: None,
                })
                .await;
            tab.waiting = true;
        }
        ChatAction::OpenPicker => {
            let tab = app.active();
            let session_db_id = tab.session_db_id.clone();
            // Warm cache short-circuit: skip the full walk when the cached
            // list is still valid. Action::SessionChanged patches in-tab
            // rows in place, and Command::NewSession invalidates wholesale,
            // so a fresh-flagged cache mirrors what a cold fetch would
            // produce. The cost rollup on each row was computed during the
            // prior cold fetch.
            if app.session_list_fresh && !app.session_list.is_empty() {
                app.picker_index = 0;
                app.mode = TuiMode::SessionPicker;
                return;
            }
            let session_db = tab.session_db.clone();
            let current_agent = tab.current_agent.clone();
            let session_name = tab.session_name.clone();
            let ctx = CommandContext {
                server,
                secrets,
                backend,
                session_db_id: &session_db_id,
                session_db: &session_db,
                current_agent: &current_agent,
                session_name: session_name.as_deref(),
            };
            match commands::dispatch(Command::ListSessions, &ctx).await {
                CommandOutcome::SessionsList(list) => {
                    app.session_list = list;
                    app.session_list_fresh = true;
                    app.picker_index = 0;
                    app.mode = TuiMode::SessionPicker;
                }
                other => {
                    render_outcome(app, other, server, backend, approval_tx, notify_tx).await;
                }
            }
        }
        ChatAction::Dispatch(cmd) => {
            // Commands that mutate the catalog membership invalidate the
            // picker cache. NameSession / ClearSessionName change a row's
            // name but Action::SessionChanged patches it in place from the
            // session DB's on_write fire, so no wholesale invalidation
            // needed there.
            if matches!(cmd, Command::NewSession) {
                app.session_list_fresh = false;
            }
            let tab = app.active();
            let session_db_id = tab.session_db_id.clone();
            let session_db = tab.session_db.clone();
            let current_agent = tab.current_agent.clone();
            let session_name = tab.session_name.clone();
            let ctx = CommandContext {
                server,
                secrets,
                backend,
                session_db_id: &session_db_id,
                session_db: &session_db,
                current_agent: &current_agent,
                session_name: session_name.as_deref(),
            };
            let outcome = commands::dispatch(cmd, &ctx).await;
            render_outcome(app, outcome, server, backend, approval_tx, notify_tx).await;
        }
    }
}

/// Persist a rename initiated from the session picker's [r] keybinding, then
/// refresh the picker list so the new alias is visible immediately. The
/// rename targets `session_db_id` directly, which may or may not be the
/// active tab — that's why it bypasses the `/name` Command path (which keys
/// off the active session).
#[allow(clippy::too_many_arguments)]
async fn apply_picker_rename(
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    session_db_id: String,
    name: Option<String>,
) {
    let result = match &name {
        Some(n) => {
            server
                .registry()
                .set_session_name(&session_db_id, n.clone())
                .await
        }
        None => server.registry().clear_session_name(&session_db_id).await,
    };

    if let Err(e) = result {
        show_error(app, format!("Rename failed: {e}"));
        // Stay in the picker so the user can try again.
        return;
    }

    // Keep the active tab's cached name in sync so its title and status bar
    // update without waiting for a session reopen.
    if let Some(idx) = app.tab_index_for(&session_db_id) {
        app.tabs[idx].session_name = name.clone();
    }

    // Refresh the picker list so the row reflects the new alias and the
    // selection stays anchored on the renamed session.
    let tab = app.active();
    let active_db_id = tab.session_db_id.clone();
    let active_db = tab.session_db.clone();
    let current_agent = tab.current_agent.clone();
    let active_name = tab.session_name.clone();
    let ctx = CommandContext {
        server,
        secrets,
        backend,
        session_db_id: &active_db_id,
        session_db: &active_db,
        current_agent: &current_agent,
        session_name: active_name.as_deref(),
    };
    if let CommandOutcome::SessionsList(list) =
        commands::dispatch(Command::ListSessions, &ctx).await
    {
        app.session_list = list;
        app.session_list_fresh = true;
        app.focus_picker_on(&session_db_id);
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_picker_selection(
    selected: String,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) {
    // If the user picked an already-open session, just activate its tab
    // instead of re-registering it.
    if let Some(idx) = app.tab_index_for(&selected) {
        app.active_tab = idx;
        app.mode = TuiMode::Chat;
        return;
    }

    let tab = app.active();
    let session_db_id = tab.session_db_id.clone();
    let session_db = tab.session_db.clone();
    let current_agent = tab.current_agent.clone();
    let session_name = tab.session_name.clone();
    let ctx = CommandContext {
        server,
        secrets,
        backend,
        session_db_id: &session_db_id,
        session_db: &session_db,
        current_agent: &current_agent,
        session_name: session_name.as_deref(),
    };
    let cmd = if selected == "__new__" {
        Command::NewSession
    } else {
        Command::SwitchSession(selected)
    };
    // Creating a session from the picker grows the catalog, so the warm
    // cache is now stale — invalidate it (mirrors the `/new` chat path) or
    // the next `/sessions` would show the cached list without this session.
    let invalidates_cache = matches!(cmd, Command::NewSession);
    let outcome = commands::dispatch(cmd, &ctx).await;
    if invalidates_cache {
        app.session_list_fresh = false;
    }
    render_outcome(app, outcome, server, backend, approval_tx, notify_tx).await;
    app.mode = TuiMode::Chat;
}

async fn render_outcome(
    app: &mut App,
    outcome: CommandOutcome,
    server: &Server,
    backend: &BackendManager,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) {
    match outcome {
        CommandOutcome::Text(t) => show_system_msg(app, t),
        CommandOutcome::Error(e) => show_error(app, e),
        CommandOutcome::SessionsList(list) => {
            if list.is_empty() {
                show_system_msg(app, "No sessions found.".to_string());
            } else {
                let mut msg = String::from("Sessions:\n");
                for info in &list {
                    let agent = info.agent_name.as_deref().unwrap_or("default");
                    let name = info
                        .name
                        .as_deref()
                        .map(|n| format!(" \"{n}\""))
                        .unwrap_or_default();
                    let age = info
                        .created_at
                        .map(|t| t.format("%Y-%m-%d").to_string())
                        .unwrap_or_else(|| "—".to_string());
                    let cost = if info.cost_reported {
                        format!(", ${:.4}", info.total_cost_usd)
                    } else {
                        String::new()
                    };
                    msg.push_str(&format!(
                        "\n  {}{} [{}] ({}, {} entries, {}{cost})",
                        info.session_db_id,
                        name,
                        info.gateway.as_str(),
                        agent,
                        info.entry_count,
                        age
                    ));
                    if let Some(preview) = &info.last_message {
                        msg.push_str(&format!("\n    {preview}"));
                    }
                }
                show_system_msg(app, msg);
            }
        }
        CommandOutcome::SessionSwitched(switch) => {
            let chaz_core::commands::SessionSwitch {
                session_db_id,
                conv_id,
                db,
                agent_name,
                session_name,
            } = *switch;
            // If the session is already open in some tab, switch to it.
            if let Some(idx) = app.tab_index_for(&session_db_id) {
                app.active_tab = idx;
                return;
            }
            if let Err(e) = setup_session(
                server,
                &db,
                backend.clone(),
                approval_tx.clone(),
                notify_tx.clone(),
            )
            .await
            {
                show_error(app, format!("Failed to register session: {e}"));
                return;
            }
            let session = Session::new(conv_id, db.clone()).await;
            let entries = session.entries().to_vec();
            app.tabs.push(Tab {
                session_db_id,
                session_db: db,
                entries,
                scroll_offset: 0,
                pending_approval: None,
                waiting: false,
                current_agent: agent_name,
                session_name,
                expanded_entries: HashSet::new(),
            });
            app.active_tab = app.tabs.len() - 1;
        }
        CommandOutcome::Quit => {
            app.should_quit = true;
        }
    }
}

pub(super) fn show_system_msg(app: &mut App, content: String) {
    app.active_mut().entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
    });
}

pub(super) fn show_error(app: &mut App, content: String) {
    app.active_mut().entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Error,
        metadata: None,
    });
}
