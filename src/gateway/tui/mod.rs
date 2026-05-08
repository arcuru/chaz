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

use crate::backends::BackendManager;
use crate::commands::{self, Command, CommandContext, CommandOutcome, SessionInfo};
use crate::config::Config;
use crate::gateway::{ApprovalExchange, Gateway};
use crate::role::get_role_names;
use crate::scheduler::Scheduler;
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};

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
    scheduler: Option<Arc<Scheduler>>,
}

impl TuiGateway {
    pub fn new(config: Config, secrets: SecretStore) -> Self {
        Self {
            config,
            secrets,
            scheduler: None,
        }
    }

    pub fn with_scheduler(mut self, scheduler: Option<Arc<Scheduler>>) -> Self {
        self.scheduler = scheduler;
        self
    }
}

/// Approval routed from the server through a per-tab forwarder, tagged with
/// the owning session DB ID so the TUI knows which tab to show the prompt on.
pub(super) type TaggedApproval = (String, ApprovalExchange);

enum Action {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// A session DB fired an on_local_write callback — payload is the
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
    Help { scroll: u16 },
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ClickTarget {
    OverlayDismiss,
    HelpCommand(&'static str),
    ApprovalApprove,
    ApprovalDeny,
    ApprovalApproveAll,
    PickerSelect(usize),
    /// Activate tab at the given index.
    TabActivate(usize),
    /// Close tab at the given index.
    TabClose(usize),
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
}

impl Tab {
    /// Title shown on the tab bar — session name if set, else a short prefix
    /// of the DB ID.
    pub fn title(&self) -> String {
        match &self.session_name {
            Some(name) => name.clone(),
            None => {
                let s = &self.session_db_id;
                let tail: String = s.rsplit(':').next().unwrap_or(s).chars().take(8).collect();
                tail
            }
        }
    }
}

pub(super) struct App {
    pub(super) mode: TuiMode,
    pub(super) overlay: Option<Overlay>,
    pub(super) click_regions: Vec<ClickRegion>,
    pub(super) input: String,
    pub(super) cursor: usize,
    pub(super) tabs: Vec<Tab>,
    pub(super) active_tab: usize,
    pub(super) agent_names: HashSet<String>,
    pub(super) should_quit: bool,
    pub(super) debug_mode: bool,
    pub(super) session_list: Vec<SessionInfo>,
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
            tabs: vec![initial_tab],
            active_tab: 0,
            agent_names,
            should_quit: false,
            debug_mode: false,
            session_list: Vec::new(),
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
    session_db.on_write(move |_event, _db| {
        let tx = notify_tx.clone();
        let id = notify_id.clone();
        Box::pin(async move {
            let _ = tx.send(id).await;
            Ok(())
        })
    })?.detach();

    Ok(())
}

/// Find an existing session named "tui", or create one and name it.
async fn default_tui_session(
    server: &Server,
) -> anyhow::Result<(crate::types::ConversationId, eidetica::Database)> {
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
        crate::types::ConversationId(session_db_id.clone()),
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

        let config_role_names = get_role_names(self.config.roles.clone());
        let default_role = self.config.role.clone();

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
                    } else if key.code == KeyCode::Char('w')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        close_active_tab(&mut app);
                    } else if key.code == KeyCode::PageUp
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, -1);
                    } else if key.code == KeyCode::PageDown
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, 1);
                    } else if input::handle_overlay_key(&mut app, key) {
                        // Overlay consumed the key.
                    } else {
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
                                        self.scheduler.as_ref(),
                                        &approval_tx,
                                        &notify_tx,
                                        &config_role_names,
                                        default_role.as_deref(),
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
                                        self.scheduler.as_ref(),
                                        &approval_tx,
                                        &notify_tx,
                                        &config_role_names,
                                        default_role.as_deref(),
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
                                if let Some(selected) = app
                                    .session_list
                                    .get(app.picker_index)
                                    .map(|s| s.session_db_id.clone())
                                {
                                    dispatch_picker_selection(
                                        selected,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        self.scheduler.as_ref(),
                                        &approval_tx,
                                        &notify_tx,
                                        &config_role_names,
                                        default_role.as_deref(),
                                    )
                                    .await;
                                }
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
                        let tab = &mut app.tabs[idx];
                        let session = Session::new(
                            crate::types::ConversationId(tab.session_db_id.clone()),
                            tab.session_db.clone(),
                        )
                        .await;
                        tab.entries = session.entries().to_vec();
                        if let Some(latest) = tab.entries.last()
                            && app.agent_names.contains(&latest.sender)
                            && latest.entry_type == EntryType::Message
                        {
                            tab.waiting = false;
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
                            .send(crate::gateway::ApprovalDecision::Deny);
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
    scheduler: Option<&Arc<Scheduler>>,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
    config_role_names: &[String],
    default_role: Option<&str>,
) {
    match action {
        ChatAction::SendMessage(text) => {
            let tab = app.active_mut();
            let session_db = tab.session_db.clone();
            let session_db_id = tab.session_db_id.clone();
            let mut session =
                Session::new(crate::types::ConversationId(session_db_id), session_db).await;
            session
                .add_entry(SessionEntry {
                    sender: "user".to_string(),
                    content: text,
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                })
                .await;
            tab.waiting = true;
        }
        ChatAction::OpenPicker => {
            let tab = app.active();
            let session_db_id = tab.session_db_id.clone();
            let session_db = tab.session_db.clone();
            let current_agent = tab.current_agent.clone();
            let session_name = tab.session_name.clone();
            let ctx = CommandContext {
                server,
                scheduler,
                secrets,
                backend,
                session_db_id: &session_db_id,
                session_db: &session_db,
                current_agent: &current_agent,
                session_name: session_name.as_deref(),
                config_roles: Some(config_role_names.to_vec()),
                default_role,
            };
            match commands::dispatch(Command::ListSessions, &ctx).await {
                CommandOutcome::SessionsList(list) => {
                    app.picker_index = list
                        .iter()
                        .position(|s| s.session_db_id == session_db_id)
                        .unwrap_or(0);
                    app.session_list = list;
                    app.mode = TuiMode::SessionPicker;
                }
                other => {
                    render_outcome(app, other, server, backend, approval_tx, notify_tx).await;
                }
            }
        }
        ChatAction::Dispatch(cmd) => {
            let tab = app.active();
            let session_db_id = tab.session_db_id.clone();
            let session_db = tab.session_db.clone();
            let current_agent = tab.current_agent.clone();
            let session_name = tab.session_name.clone();
            let ctx = CommandContext {
                server,
                scheduler,
                secrets,
                backend,
                session_db_id: &session_db_id,
                session_db: &session_db,
                current_agent: &current_agent,
                session_name: session_name.as_deref(),
                config_roles: Some(config_role_names.to_vec()),
                default_role,
            };
            let outcome = commands::dispatch(cmd, &ctx).await;
            render_outcome(app, outcome, server, backend, approval_tx, notify_tx).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_picker_selection(
    selected: String,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    scheduler: Option<&Arc<Scheduler>>,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
    config_role_names: &[String],
    default_role: Option<&str>,
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
        scheduler,
        secrets,
        backend,
        session_db_id: &session_db_id,
        session_db: &session_db,
        current_agent: &current_agent,
        session_name: session_name.as_deref(),
        config_roles: Some(config_role_names.to_vec()),
        default_role,
    };
    let cmd = if selected == "__new__" {
        Command::NewSession
    } else {
        Command::SwitchSession(selected)
    };
    let outcome = commands::dispatch(cmd, &ctx).await;
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
                    msg.push_str(&format!(
                        "\n  {}{} ({}, {} entries)",
                        info.session_db_id, name, agent, info.entry_count
                    ));
                    if let Some(preview) = &info.last_message {
                        msg.push_str(&format!("\n    {preview}"));
                    }
                }
                show_system_msg(app, msg);
            }
        }
        CommandOutcome::SessionSwitched(switch) => {
            let crate::commands::SessionSwitch {
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
    });
}

pub(super) fn show_error(app: &mut App, content: String) {
    app.active_mut().entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Error,
    });
}
