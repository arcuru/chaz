//! Terminal UI gateway. Elm-style architecture:
//! - `App` holds state
//! - `Action` / `ChatAction` are the update messages
//! - `view::ui` renders a frame from `&App`
//! - `input::parse_chat_line` turns typed text into actions
//!
//! Submodules:
//! - `input` — KeyEvent handling, slash-command parsing, help text
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

enum Action {
    Key(KeyEvent),
    Mouse(MouseEvent),
    SessionChanged,
    ApprovalRequest(ApprovalExchange),
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

/// A modal layered over the current mode. `None` = no overlay. Event handling
/// in mod.rs short-circuits to overlay when `Some(_)`, so overlays intercept
/// keys/clicks regardless of whether the underlying mode is Chat or
/// SessionPicker. Dismiss with Esc or by clicking outside the popup rect.
pub(super) enum Overlay {
    Help { scroll: u16 },
}

/// A hit-testable rectangle recorded by the view during render. Each frame
/// clears `App.click_regions` and re-populates it, so the mouse handler can
/// resolve a click at (col, row) to a semantic action without the view having
/// to live beyond the draw call.
#[derive(Clone, Copy, Debug)]
pub(super) enum ClickTarget {
    /// Background of the overlay popup — click here dismisses the overlay.
    OverlayDismiss,
    /// Insert a command template into the input box and dismiss the overlay.
    HelpCommand(&'static str),
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

pub(super) struct App {
    pub(super) mode: TuiMode,
    pub(super) overlay: Option<Overlay>,
    pub(super) click_regions: Vec<ClickRegion>,
    pub(super) input: String,
    pub(super) cursor: usize,
    pub(super) scroll_offset: u16,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) pending_approval: Option<ApprovalExchange>,
    pub(super) waiting: bool,
    pub(super) agent_names: HashSet<String>,
    pub(super) should_quit: bool,
    pub(super) debug_mode: bool,
    pub(super) session_db_id: String,
    pub(super) current_agent: String,
    pub(super) session_name: Option<String>,
    pub(super) session_list: Vec<SessionInfo>,
    pub(super) picker_index: usize,
}

impl App {
    fn new(agent_names: HashSet<String>, session_db_id: String) -> Self {
        Self {
            mode: TuiMode::Chat,
            overlay: None,
            click_regions: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll_offset: 0,
            entries: Vec::new(),
            pending_approval: None,
            waiting: false,
            agent_names,
            should_quit: false,
            debug_mode: false,
            session_db_id,
            current_agent: String::new(),
            session_name: None,
            session_list: Vec::new(),
            picker_index: 0,
        }
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

/// Register a session DB for server processing and TUI notifications.
async fn setup_session(
    server: &Server,
    session_db: &eidetica::Database,
    backend: BackendManager,
    approval_tx: mpsc::Sender<ApprovalExchange>,
    notify_tx: mpsc::Sender<()>,
) -> anyhow::Result<()> {
    server
        .register_session(session_db, backend, None, Some(approval_tx))
        .await?;

    session_db.on_local_write(move |_entry, _db, _instance| {
        let tx = notify_tx.clone();
        Box::pin(async move {
            let _ = tx.send(()).await;
            Ok(())
        })
    })?;

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

impl Gateway for TuiGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalExchange>(8);
        let (notify_tx, mut notify_rx) = mpsc::channel::<()>(16);

        let (_conv_id, mut session_db) = default_tui_session(&server).await?;
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

        let mut app = App::new(agent_names, session_db_id.clone());
        {
            let agent = server
                .registry()
                .resolve_agent(&session_db_id, None, server.agent_index())
                .await;
            app.current_agent = agent.name.clone();
            let session = Session::new(
                crate::types::ConversationId(session_db_id.clone()),
                session_db.clone(),
            )
            .await;
            app.session_name = session.read_meta().await.name;
            app.entries = session.entries().to_vec();
        }

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
                Some(_) = notify_rx.recv() => Action::SessionChanged,
                Some(exchange) = approval_rx.recv() => Action::ApprovalRequest(exchange),
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
                    } else if input::handle_overlay_key(&mut app, key) {
                        // Overlay consumed the key.
                    } else {
                        match app.mode {
                            TuiMode::Chat => {
                                if let Some(chat_action) =
                                    input::handle_chat_key(&mut app, key).await
                                {
                                    match chat_action {
                                        ChatAction::SendMessage(text) => {
                                            let mut session = Session::new(
                                                crate::types::ConversationId(
                                                    app.session_db_id.clone(),
                                                ),
                                                session_db.clone(),
                                            )
                                            .await;
                                            session
                                                .add_entry(SessionEntry {
                                                    sender: "user".to_string(),
                                                    content: text,
                                                    timestamp: chrono::Utc::now(),
                                                    entry_type: EntryType::Message,
                                                })
                                                .await;
                                            app.waiting = true;
                                        }
                                        ChatAction::OpenPicker => {
                                            let ctx = CommandContext {
                                                server: &server,
                                                scheduler: self.scheduler.as_ref(),
                                                secrets: &self.secrets,
                                                backend: &backend,
                                                session_db_id: &app.session_db_id,
                                                session_db: &session_db,
                                                current_agent: &app.current_agent,
                                                session_name: app.session_name.as_deref(),
                                                config_roles: Some(config_role_names.clone()),
                                                default_role: default_role.as_deref(),
                                            };
                                            match commands::dispatch(Command::ListSessions, &ctx)
                                                .await
                                            {
                                                CommandOutcome::SessionsList(list) => {
                                                    app.session_list = list;
                                                    app.picker_index = app
                                                        .session_list
                                                        .iter()
                                                        .position(|s| {
                                                            s.session_db_id == app.session_db_id
                                                        })
                                                        .unwrap_or(0);
                                                    app.mode = TuiMode::SessionPicker;
                                                }
                                                other => {
                                                    render_outcome(
                                                        &mut app,
                                                        other,
                                                        &server,
                                                        &backend,
                                                        &approval_tx,
                                                        &notify_tx,
                                                        &mut session_db,
                                                    )
                                                    .await
                                                }
                                            }
                                        }
                                        ChatAction::Dispatch(cmd) => {
                                            let ctx = CommandContext {
                                                server: &server,
                                                scheduler: self.scheduler.as_ref(),
                                                secrets: &self.secrets,
                                                backend: &backend,
                                                session_db_id: &app.session_db_id,
                                                session_db: &session_db,
                                                current_agent: &app.current_agent,
                                                session_name: app.session_name.as_deref(),
                                                config_roles: Some(config_role_names.clone()),
                                                default_role: default_role.as_deref(),
                                            };
                                            let outcome = commands::dispatch(cmd, &ctx).await;
                                            render_outcome(
                                                &mut app,
                                                outcome,
                                                &server,
                                                &backend,
                                                &approval_tx,
                                                &notify_tx,
                                                &mut session_db,
                                            )
                                            .await;
                                        }
                                    }
                                }
                            }
                            TuiMode::SessionPicker => {
                                if let Some(selected) = input::handle_picker_key(&mut app, key) {
                                    let ctx = CommandContext {
                                        server: &server,
                                        scheduler: self.scheduler.as_ref(),
                                        secrets: &self.secrets,
                                        backend: &backend,
                                        session_db_id: &app.session_db_id,
                                        session_db: &session_db,
                                        current_agent: &app.current_agent,
                                        session_name: app.session_name.as_deref(),
                                        config_roles: Some(config_role_names.clone()),
                                        default_role: default_role.as_deref(),
                                    };
                                    let cmd = if selected == "__new__" {
                                        Command::NewSession
                                    } else {
                                        Command::SwitchSession(selected)
                                    };
                                    let outcome = commands::dispatch(cmd, &ctx).await;
                                    render_outcome(
                                        &mut app,
                                        outcome,
                                        &server,
                                        &backend,
                                        &approval_tx,
                                        &notify_tx,
                                        &mut session_db,
                                    )
                                    .await;
                                    app.mode = TuiMode::Chat;
                                }
                            }
                        }
                    }
                }
                Action::Mouse(m) => {
                    input::handle_mouse(&mut app, m);
                }
                Action::SessionChanged => {
                    if let TuiMode::Chat = app.mode {
                        let session = Session::new(
                            crate::types::ConversationId(app.session_db_id.clone()),
                            session_db.clone(),
                        )
                        .await;
                        app.entries = session.entries().to_vec();
                        if let Some(latest) = app.entries.last() {
                            if app.agent_names.contains(&latest.sender)
                                && latest.entry_type == EntryType::Message
                            {
                                app.waiting = false;
                            }
                        }
                    }
                }
                Action::ApprovalRequest(exchange) => {
                    app.pending_approval = Some(exchange);
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

async fn render_outcome(
    app: &mut App,
    outcome: CommandOutcome,
    server: &Server,
    backend: &BackendManager,
    approval_tx: &mpsc::Sender<ApprovalExchange>,
    notify_tx: &mpsc::Sender<()>,
    session_db: &mut eidetica::Database,
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
            *session_db = db.clone();
            let session = Session::new(conv_id, db).await;
            app.entries = session.entries().to_vec();
            app.session_db_id = session_db_id;
            app.current_agent = agent_name;
            app.session_name = session_name;
            app.scroll_offset = 0;
            app.waiting = false;
        }
        CommandOutcome::Quit => {
            app.should_quit = true;
        }
    }
}

pub(super) fn show_system_msg(app: &mut App, content: String) {
    app.entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Message,
    });
}

pub(super) fn show_error(app: &mut App, content: String) {
    app.entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Error,
    });
}
