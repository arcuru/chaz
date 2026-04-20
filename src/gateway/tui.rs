use crate::backends::BackendManager;
use crate::commands::{self, Command, CommandContext, CommandOutcome, SessionInfo};
use crate::config::Config;
use crate::gateway::{ApprovalDecision, ApprovalExchange, Gateway};
use crate::role::get_role_names;
use crate::scheduler::Scheduler;
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Terminal;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

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
    SessionChanged,
    ApprovalRequest(ApprovalExchange),
}

enum TuiMode {
    Chat,
    SessionPicker,
}

enum ChatAction {
    Dispatch(Command),
    OpenPicker,
    SendMessage(String),
}

struct App {
    mode: TuiMode,
    input: String,
    cursor: usize,
    scroll_offset: u16,
    entries: Vec<SessionEntry>,
    pending_approval: Option<ApprovalExchange>,
    waiting: bool,
    agent_names: HashSet<String>,
    should_quit: bool,
    debug_mode: bool,
    session_db_id: String,
    current_agent: String,
    session_name: Option<String>,
    session_list: Vec<SessionInfo>,
    picker_index: usize,
}

impl App {
    fn new(agent_names: HashSet<String>, session_db_id: String) -> Self {
        Self {
            mode: TuiMode::Chat,
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
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
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
            terminal.draw(|f| ui(f, &app))?;

            let action = tokio::select! {
                Some(Ok(event)) = events.next() => {
                    match event {
                        Event::Key(key) => Action::Key(key),
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
                    } else {
                        match app.mode {
                            TuiMode::Chat => {
                                if let Some(chat_action) =
                                    handle_chat_key(&mut app, key, &session_db).await
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
                                if let Some(selected) = handle_picker_key(&mut app, key) {
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

fn show_system_msg(app: &mut App, content: String) {
    app.entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Message,
    });
}

fn show_error(app: &mut App, content: String) {
    app.entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Error,
    });
}

async fn handle_chat_key(
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
    if text == "/agent" || text == "/agent list" {
        return Some(ChatAction::Dispatch(Command::AgentsList));
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
        "Commands:",
        "  /sessions, /s         — open session picker",
        "  /new                  — create a new session (use picker 'n' key)",
        "  /join <id>            — switch to session by name or DB ID",
        "  /name [<alias>]       — set (or clear) a session alias",
        "  /info                 — show current session info",
        "  /channels             — list Matrix rooms attached to this session",
        "  /share                — generate shareable ticket for current session",
        "  /sync <ticket>        — sync a remote session via ticket",
        "  /compact              — summarize and compact conversation history",
        "  /print                — dump the transcript",
        "  /model [<model>]      — show or set the model for this session",
        "  /role [<name> [<prompt>]] — show, select, or define a role",
        "  /backend <name> <url> <key> — add a custom backend for this session",
        "  /backends             — list known backends and models",
        "  /schedules            — list configured schedules",
        "  /run <name>           — trigger a schedule immediately",
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

fn handle_picker_key(app: &mut App, key: KeyEvent) -> Option<String> {
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

fn ui(f: &mut ratatui::Frame, app: &App) {
    match app.mode {
        TuiMode::Chat => ui_chat(f, app),
        TuiMode::SessionPicker => ui_picker(f, app),
    }
}

fn ui_chat(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .split(f.area());

    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.entries {
        let debug_prefix = if app.debug_mode {
            let ts = entry.timestamp.format("%H:%M:%S");
            let typ = format!("{:?}", entry.entry_type);
            format!("[{ts} {typ:<10}] ")
        } else {
            String::new()
        };
        let dim = Style::default().fg(Color::DarkGray);

        match entry.entry_type {
            EntryType::Message | EntryType::Directive => {
                let is_agent = app.agent_names.contains(&entry.sender);
                let is_system = entry.sender == "system";
                let sender_style = if is_system {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if is_agent {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                };

                let type_label = if entry.entry_type == EntryType::Directive {
                    " (directive)"
                } else {
                    ""
                };
                let label = format!("{}{}{}:", debug_prefix, entry.sender, type_label);

                lines.push(Line::from(vec![Span::styled(label, sender_style)]));

                for content_line in entry.content.lines() {
                    lines.push(Line::from(format!("  {content_line}")));
                }
                lines.push(Line::from(""));
            }
            EntryType::Ack => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}{} thinking...", entry.sender),
                    dim,
                )]));
            }
            EntryType::ToolCall => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}  > {}", entry.content),
                    dim,
                )]));
            }
            EntryType::ToolResult => {
                let max_len = if app.debug_mode { 500 } else { 120 };
                let display = if entry.content.len() > max_len {
                    format!("{}...", &entry.content[..max_len])
                } else {
                    entry.content.clone()
                };
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}  < {display}"),
                    dim,
                )]));
            }
            EntryType::Error => {
                lines.push(Line::from(vec![Span::styled(
                    format!("{debug_prefix}  ERROR {}: {}", entry.sender, entry.content),
                    Style::default().fg(Color::Red),
                )]));
                lines.push(Line::from(""));
            }
            EntryType::Summary => {
                let label = format!("{debug_prefix}--- context summary ---");
                lines.push(Line::from(vec![Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )]));
                for content_line in entry.content.lines() {
                    lines.push(Line::from(vec![Span::styled(
                        format!("  {content_line}"),
                        Style::default().fg(Color::Magenta),
                    )]));
                }
                lines.push(Line::from(""));
            }
        }
    }

    if let Some(ref exchange) = app.pending_approval {
        let info = &exchange.info;
        lines.push(Line::from(vec![Span::styled(
            "--- Tool Approval Required ---",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(format!("  Tool: {}", info.name)));
        lines.push(Line::from(format!("  Risk: {}", info.risk_level)));
        lines.push(Line::from(format!("  Args: {}", info.arguments_display)));
        lines.push(Line::from(vec![
            Span::styled("  [y]", Style::default().fg(Color::Green)),
            Span::raw("es  "),
            Span::styled("[n]", Style::default().fg(Color::Red)),
            Span::raw("o  "),
            Span::styled("[a]", Style::default().fg(Color::Yellow)),
            Span::raw("ll"),
        ]));
        lines.push(Line::from(""));
    }

    if app.waiting {
        lines.push(Line::from(vec![Span::styled(
            "  thinking...",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    let messages_height = chunks[0].height.saturating_sub(2);
    let content_height = lines.len() as u16;
    let scroll = if content_height > messages_height {
        content_height
            .saturating_sub(messages_height)
            .saturating_sub(app.scroll_offset)
    } else {
        0
    };

    let messages = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .block(Block::bordered().title(" Chaz "));
    f.render_widget(messages, chunks[0]);

    let msg_count = app
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Message)
        .count();
    let debug_indicator = if app.debug_mode { " | DEBUG" } else { "" };
    let session_label = match &app.session_name {
        Some(name) => format!("{} ({})", name, app.session_db_id),
        None => app.session_db_id.clone(),
    };
    let status_text = format!(
        " {} | agent: {} | messages: {}{} | /help",
        session_label, app.current_agent, msg_count, debug_indicator
    );
    let status =
        Paragraph::new(status_text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(status, chunks[1]);

    let input = Paragraph::new(app.input.as_str()).block(Block::bordered().title(" > "));
    f.render_widget(input, chunks[2]);

    let cursor_x = chunks[2].x + app.cursor as u16 + 1;
    let cursor_y = chunks[2].y + 1;
    f.set_cursor_position((cursor_x, cursor_y));
}

fn ui_picker(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    if app.session_list.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  No sessions found. Press 'n' to create one.",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        for (i, info) in app.session_list.iter().enumerate() {
            let is_selected = i == app.picker_index;
            let is_current = info.session_db_id == app.session_db_id;

            let marker = if is_selected { "> " } else { "  " };
            let current_marker = if is_current { " *" } else { "" };

            let agent_str = info.agent_name.as_deref().unwrap_or("default");
            let name_str = info
                .name
                .as_ref()
                .map(|n| format!(" \"{n}\""))
                .unwrap_or_default();

            let header = format!(
                "{}{}{}{} ({}, {} entries)",
                marker, info.session_db_id, name_str, current_marker, agent_str, info.entry_count
            );

            let style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if is_current {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            };

            lines.push(Line::from(vec![Span::styled(header, style)]));

            if let Some(ref preview) = info.last_message {
                lines.push(Line::from(vec![Span::styled(
                    format!("    {preview}"),
                    Style::default().fg(Color::DarkGray),
                )]));
            }

            lines.push(Line::from(""));
        }
    }

    let list = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title(" Sessions "));
    f.render_widget(list, chunks[0]);

    let help =
        Paragraph::new(" [Up/Down] navigate | [Enter] select | [n] new session | [Esc] cancel")
            .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(help, chunks[1]);
}
