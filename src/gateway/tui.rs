use crate::backends::BackendManager;
use crate::config::Config;
use crate::gateway::{ApprovalDecision, ApprovalExchange, Gateway};
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

pub struct TuiGateway {
    config: Config,
    secrets: SecretStore,
}

impl TuiGateway {
    pub fn new(config: Config, secrets: SecretStore) -> Self {
        Self { config, secrets }
    }
}

/// Actions processed by the event loop
enum Action {
    Key(KeyEvent),
    SessionChanged,
    ApprovalRequest(ApprovalExchange),
}

/// Which screen the TUI is showing
enum TuiMode {
    Chat,
    SessionPicker,
}

/// A session listing entry for the picker
struct SessionInfo {
    transport_id: String,
    agent_name: Option<String>,
    entry_count: usize,
    last_message: Option<String>,
}

/// Centralized TUI application state
struct App {
    mode: TuiMode,
    // Chat state
    input: String,
    cursor: usize,
    scroll_offset: u16,
    entries: Vec<SessionEntry>,
    pending_approval: Option<ApprovalExchange>,
    waiting: bool,
    agent_names: HashSet<String>,
    should_quit: bool,
    debug_mode: bool,
    // Current session
    transport_id: String,
    current_agent: String,
    // Session picker state
    session_list: Vec<SessionInfo>,
    picker_index: usize,
}

impl App {
    fn new(agent_names: HashSet<String>, transport_id: String) -> Self {
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
            transport_id,
            current_agent: String::new(),
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
    transport_id: &str,
    session_db: &eidetica::Database,
    backend: BackendManager,
    approval_tx: mpsc::Sender<ApprovalExchange>,
    notify_tx: mpsc::Sender<()>,
) -> anyhow::Result<()> {
    server
        .register_session(transport_id, session_db, backend, None, Some(approval_tx))
        .await?;

    // Register TUI notification callback — fires on any session write
    session_db.on_local_write(move |_entry, _db, _instance| {
        let tx = notify_tx.clone();
        Box::pin(async move {
            let _ = tx.send(()).await;
            Ok(())
        })
    })?;

    Ok(())
}

/// Load session list from registry, with entry counts and last message previews.
async fn load_session_list(server: &Server) -> Vec<SessionInfo> {
    let bindings = match server.registry().list_sessions().await {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };

    let mut sessions = Vec::new();
    for binding in bindings {
        // Try to load entry count and last message
        let (entry_count, last_message) = match server
            .registry()
            .get_or_create_session_db(&binding.transport_id)
            .await
        {
            Ok((conv_id, db)) => {
                let session = Session::new(conv_id, db).await;
                let count = session.entries().len();
                let last = session
                    .entries()
                    .iter()
                    .rev()
                    .find(|e| e.entry_type == EntryType::Message)
                    .map(|e| {
                        let preview = e.content.lines().next().unwrap_or("");
                        if preview.len() > 60 {
                            format!("{}: {}…", e.sender, &preview[..60])
                        } else {
                            format!("{}: {}", e.sender, preview)
                        }
                    });
                (count, last)
            }
            Err(_) => (0, None),
        };

        sessions.push(SessionInfo {
            transport_id: binding.transport_id,
            agent_name: binding.agent_name,
            entry_count,
            last_message,
        });
    }

    sessions
}

impl Gateway for TuiGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let default_transport = "tui".to_string();

        // Channels shared across session switches
        let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalExchange>(8);
        let (notify_tx, mut notify_rx) = mpsc::channel::<()>(16);

        // Set up default session
        let (_conv_id, mut session_db) = server
            .registry()
            .get_or_create_session_db(&default_transport)
            .await?;

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        setup_session(
            &server,
            &default_transport,
            &session_db,
            backend.clone(),
            approval_tx.clone(),
            notify_tx.clone(),
        )
        .await?;

        // Collect agent names for display styling
        let agent_names: HashSet<String> = server
            .agents()
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        // Initialize app state
        let mut app = App::new(agent_names, default_transport.clone());
        {
            let agent = server
                .registry()
                .resolve_agent(&default_transport, None)
                .await;
            app.current_agent = agent.name.clone();
            let session = Session::new(
                crate::types::ConversationId(default_transport.clone()),
                session_db.clone(),
            )
            .await;
            app.entries = session.entries().to_vec();
        }

        // Set up terminal with panic hook
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            original_hook(info);
        }));

        let mut terminal = init_terminal()?;
        let mut events = EventStream::new();

        // Event loop
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
                    // Global key bindings
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
                                let switch = handle_chat_key(&mut app, key, &session_db).await;

                                if let Some(cmd) = switch {
                                    match cmd {
                                        ChatCommand::OpenPicker => {
                                            app.session_list = load_session_list(&server).await;
                                            // Pre-select current session
                                            app.picker_index = app
                                                .session_list
                                                .iter()
                                                .position(|s| s.transport_id == app.transport_id)
                                                .unwrap_or(0);
                                            app.mode = TuiMode::SessionPicker;
                                        }
                                        ChatCommand::SwitchSession(tid) => {
                                            match switch_session(
                                                &server,
                                                &tid,
                                                &backend,
                                                &approval_tx,
                                                &notify_tx,
                                            )
                                            .await
                                            {
                                                Ok((db, entries, agent_name)) => {
                                                    session_db = db;
                                                    app.transport_id = tid;
                                                    app.entries = entries;
                                                    app.current_agent = agent_name;
                                                    app.scroll_offset = 0;
                                                    app.waiting = false;
                                                }
                                                Err(e) => {
                                                    show_error(
                                                        &mut app,
                                                        format!("Failed to switch session: {e}"),
                                                    );
                                                }
                                            }
                                        }
                                        ChatCommand::ShareSession => {
                                            match generate_share_ticket(&server, &session_db).await
                                            {
                                                Ok(ticket) => {
                                                    show_system_msg(&mut app, format!(
                                                        "Share this ticket to sync the current session:\n\n{ticket}"
                                                    ));
                                                }
                                                Err(e) => {
                                                    show_error(
                                                        &mut app,
                                                        format!("Failed to generate ticket: {e}"),
                                                    );
                                                }
                                            }
                                        }
                                        ChatCommand::SyncTicket(ticket_str) => {
                                            match sync_from_ticket(&server, &ticket_str).await {
                                                Ok(msg) => {
                                                    show_system_msg(&mut app, msg);
                                                }
                                                Err(e) => {
                                                    show_error(
                                                        &mut app,
                                                        format!("Sync failed: {e}"),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            TuiMode::SessionPicker => {
                                if let Some(selected) = handle_picker_key(&mut app, key) {
                                    match switch_session(
                                        &server,
                                        &selected,
                                        &backend,
                                        &approval_tx,
                                        &notify_tx,
                                    )
                                    .await
                                    {
                                        Ok((db, entries, agent_name)) => {
                                            session_db = db;
                                            app.transport_id = selected;
                                            app.entries = entries;
                                            app.current_agent = agent_name;
                                            app.scroll_offset = 0;
                                            app.waiting = false;
                                        }
                                        Err(e) => {
                                            show_error(
                                                &mut app,
                                                format!("Failed to switch session: {e}"),
                                            );
                                        }
                                    }
                                    app.mode = TuiMode::Chat;
                                }
                            }
                        }
                    }
                }
                Action::SessionChanged => {
                    // Reload current session entries
                    if let TuiMode::Chat = app.mode {
                        let session = Session::new(
                            crate::types::ConversationId(app.transport_id.clone()),
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

/// Commands returned by chat key handler that require session-level action.
enum ChatCommand {
    OpenPicker,
    SwitchSession(String),
    ShareSession,
    SyncTicket(String),
}

/// Switch to a different session. Registers with server and returns the new DB, entries, and agent name.
async fn switch_session(
    server: &Server,
    transport_id: &str,
    backend: &BackendManager,
    approval_tx: &mpsc::Sender<ApprovalExchange>,
    notify_tx: &mpsc::Sender<()>,
) -> anyhow::Result<(eidetica::Database, Vec<SessionEntry>, String)> {
    let (conv_id, session_db) = server
        .registry()
        .get_or_create_session_db(transport_id)
        .await?;

    setup_session(
        server,
        transport_id,
        &session_db,
        backend.clone(),
        approval_tx.clone(),
        notify_tx.clone(),
    )
    .await?;

    let agent = server.registry().resolve_agent(transport_id, None).await;
    let session = Session::new(conv_id, session_db.clone()).await;
    let entries = session.entries().to_vec();
    Ok((session_db, entries, agent.name))
}

/// Generate a shareable DatabaseTicket for the current session.
async fn generate_share_ticket(
    server: &Server,
    session_db: &eidetica::Database,
) -> anyhow::Result<String> {
    let instance = server.registry().instance();
    let sync = instance
        .sync()
        .ok_or_else(|| anyhow::anyhow!("Sync not enabled"))?;

    let mut ticket = eidetica::sync::DatabaseTicket::new(session_db.root_id().clone());

    // Add all known server addresses as hints
    if let Ok(addresses) = sync.get_all_server_addresses().await {
        for (transport_type, address) in addresses {
            ticket.add_address(eidetica::sync::Address::new(transport_type, address));
        }
    }

    Ok(ticket.to_string())
}

/// Sync a remote session via a DatabaseTicket URL.
async fn sync_from_ticket(server: &Server, ticket_str: &str) -> anyhow::Result<String> {
    let instance = server.registry().instance();
    let sync = instance
        .sync()
        .ok_or_else(|| anyhow::anyhow!("Sync not enabled"))?;

    let ticket: eidetica::sync::DatabaseTicket = ticket_str
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid ticket: {e}"))?;

    let db_id = ticket.database_id().clone();
    sync.sync_with_ticket(&ticket).await?;

    Ok(format!(
        "Synced database {}. Use /sessions to find and open it.",
        db_id
    ))
}

/// Show a system message in the TUI.
fn show_system_msg(app: &mut App, content: String) {
    app.entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Message,
    });
}

/// Show an error in the TUI.
fn show_error(app: &mut App, content: String) {
    app.entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Error,
    });
}

/// Handle a key event in chat mode. Returns a ChatCommand if the event loop
/// needs to take session-level action (switching, opening picker).
async fn handle_chat_key(
    app: &mut App,
    key: KeyEvent,
    session_db: &eidetica::Database,
) -> Option<ChatCommand> {
    // Handle approval mode
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

                // Handle commands
                match text.as_str() {
                    "/quit" | "/exit" | "/q" => {
                        app.should_quit = true;
                        return None;
                    }
                    "/sessions" | "/s" => {
                        return Some(ChatCommand::OpenPicker);
                    }
                    "/clear" => {
                        app.entries.clear();
                        app.scroll_offset = 0;
                        return None;
                    }
                    _ if text.starts_with("/join ") => {
                        let tid = text.strip_prefix("/join ").unwrap().trim().to_string();
                        if !tid.is_empty() {
                            return Some(ChatCommand::SwitchSession(tid));
                        }
                        return None;
                    }
                    "/new" => {
                        let tid = format!("tui:{}", uuid::Uuid::new_v4());
                        return Some(ChatCommand::SwitchSession(tid));
                    }
                    "/share" => {
                        return Some(ChatCommand::ShareSession);
                    }
                    _ if text.starts_with("/sync ") => {
                        let ticket = text.strip_prefix("/sync ").unwrap().trim().to_string();
                        if !ticket.is_empty() {
                            return Some(ChatCommand::SyncTicket(ticket));
                        }
                        return None;
                    }
                    "/help" | "/?" => {
                        app.entries.push(SessionEntry {
                            sender: "system".to_string(),
                            content: [
                                "Commands:",
                                "  /sessions, /s  — open session picker",
                                "  /new           — create a new session",
                                "  /join <id>     — switch to session by transport ID",
                                "  /info          — show current session info",
                                "  /share         — generate shareable ticket for current session",
                                "  /sync <ticket> — sync a remote session via ticket",
                                "  /clear         — clear display (entries still in DB)",
                                "  /raw           — dump raw entry data for debugging",
                                "  /debug         — toggle debug mode (Ctrl+D)",
                                "  /quit, /q      — exit",
                                "",
                                "Keys:",
                                "  Ctrl+D         — toggle debug mode (shows timestamps, types)",
                                "  Ctrl+C         — quit",
                                "  Up/Down        — scroll messages",
                            ]
                            .join("\n"),
                            timestamp: chrono::Utc::now(),
                            entry_type: EntryType::Message,
                        });
                        return None;
                    }
                    "/info" => {
                        let msg_count = app
                            .entries
                            .iter()
                            .filter(|e| e.entry_type == EntryType::Message)
                            .count();
                        let tool_count = app
                            .entries
                            .iter()
                            .filter(|e| e.entry_type == EntryType::ToolCall)
                            .count();
                        let directive_count = app
                            .entries
                            .iter()
                            .filter(|e| e.entry_type == EntryType::Directive)
                            .count();
                        let error_count = app
                            .entries
                            .iter()
                            .filter(|e| e.entry_type == EntryType::Error)
                            .count();
                        app.entries.push(SessionEntry {
                            sender: "system".to_string(),
                            content: format!(
                                "Session: {}\nTotal entries: {}\nMessages: {} | Directives: {} | Tool calls: {} | Errors: {}\nDebug mode: {}",
                                app.transport_id,
                                app.entries.len(),
                                msg_count,
                                directive_count,
                                tool_count,
                                error_count,
                                if app.debug_mode { "on" } else { "off" },
                            ),
                            timestamp: chrono::Utc::now(),
                            entry_type: EntryType::Message,
                        });
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
                        app.entries.push(SessionEntry {
                            sender: "system".to_string(),
                            content: raw,
                            timestamp: chrono::Utc::now(),
                            entry_type: EntryType::Message,
                        });
                        return None;
                    }
                    "/debug" => {
                        app.debug_mode = !app.debug_mode;
                        return None;
                    }
                    _ => {}
                }

                // Regular message — write to session DB
                let mut session = Session::new(
                    crate::types::ConversationId(app.transport_id.clone()),
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

/// Handle a key event in session picker mode.
/// Returns Some(transport_id) if a session was selected.
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
            .map(|info| info.transport_id.clone()),
        KeyCode::Char('n') => {
            // Create new session with a UUID transport ID
            let tid = format!("tui:{}", uuid::Uuid::new_v4());
            Some(tid)
        }
        KeyCode::Esc => {
            // Cancel — return to chat without switching
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
        Constraint::Min(1),    // messages
        Constraint::Length(1), // status bar
        Constraint::Length(3), // input box
    ])
    .split(f.area());

    // === Messages area ===
    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.entries {
        // Debug prefix: timestamp + entry type
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
        }
    }

    // Tool approval prompt
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

    // Thinking indicator
    if app.waiting {
        lines.push(Line::from(vec![Span::styled(
            "  thinking...",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    // Compute scroll to pin to bottom
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

    // === Status bar ===
    let msg_count = app
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Message)
        .count();
    let debug_indicator = if app.debug_mode { " | DEBUG" } else { "" };
    let status_text = format!(
        " {} | agent: {} | messages: {}{} | /help",
        app.transport_id, app.current_agent, msg_count, debug_indicator
    );
    let status =
        Paragraph::new(status_text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(status, chunks[1]);

    // === Input area ===
    let input = Paragraph::new(app.input.as_str()).block(Block::bordered().title(" > "));
    f.render_widget(input, chunks[2]);

    // Position cursor in input box
    let cursor_x = chunks[2].x + app.cursor as u16 + 1;
    let cursor_y = chunks[2].y + 1;
    f.set_cursor_position((cursor_x, cursor_y));
}

fn ui_picker(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Min(1),    // session list
        Constraint::Length(1), // help bar
    ])
    .split(f.area());

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
            let is_current = info.transport_id == app.transport_id;

            let marker = if is_selected { "> " } else { "  " };
            let current_marker = if is_current { " *" } else { "" };

            let agent_str = info.agent_name.as_deref().unwrap_or("default");

            let header = format!(
                "{}{}{} ({}, {} entries)",
                marker, info.transport_id, current_marker, agent_str, info.entry_count
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

            // Show last message preview
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
