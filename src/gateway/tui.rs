use crate::backends::BackendManager;
use crate::config::Config;
use crate::gateway::{ApprovalDecision, ApprovalExchange, Gateway};
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use tokio_stream::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Terminal;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

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

/// Centralized TUI application state
struct App {
    input: String,
    cursor: usize,
    scroll_offset: u16,
    entries: Vec<SessionEntry>,
    pending_approval: Option<ApprovalExchange>,
    waiting: bool,
    agent_names: HashSet<String>,
    should_quit: bool,
}

impl App {
    fn new(agent_names: HashSet<String>) -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            scroll_offset: 0,
            entries: Vec::new(),
            pending_approval: None,
            waiting: false,
            agent_names,
            should_quit: false,
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

impl Gateway for TuiGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let transport_id = "tui".to_string();

        // Create approval channel
        let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalExchange>(8);

        // Get or create session DB
        let (_conv_id, session_db) = server
            .registry()
            .get_or_create_session_db(&transport_id)
            .await?;

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        // Register server callback (agent processing)
        server
            .register_session(
                &transport_id,
                &session_db,
                backend,
                None,
                Some(approval_tx.clone()),
            )
            .await?;

        // Register session change notification callback
        let (notify_tx, mut notify_rx) = mpsc::channel::<()>(16);
        session_db.on_local_write(move |_entry, _db, _instance| {
            let tx = notify_tx.clone();
            Box::pin(async move {
                let _ = tx.send(()).await;
                Ok(())
            })
        })?;

        // Collect agent names for display styling
        let agent_names: HashSet<String> = server
            .agents()
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        // Initialize app state with existing session entries
        let mut app = App::new(agent_names);
        {
            let session = Session::new(
                crate::types::ConversationId(transport_id.clone()),
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
                    handle_key(&mut app, key, &session_db, &transport_id).await;
                }
                Action::SessionChanged => {
                    let session = Session::new(
                        crate::types::ConversationId(transport_id.clone()),
                        session_db.clone(),
                    )
                    .await;
                    app.entries = session.entries().to_vec();
                    // Clear waiting if latest entry is from an agent
                    if let Some(latest) = app.entries.last() {
                        if app.agent_names.contains(&latest.sender) {
                            app.waiting = false;
                        }
                    }
                    app.scroll_offset = 0;
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

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    session_db: &eidetica::Database,
    transport_id: &str,
) {
    // Ctrl+C always quits
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    // Handle approval mode
    if let Some(exchange) = app.pending_approval.take() {
        let decision = match key.code {
            KeyCode::Char('y') => Some(ApprovalDecision::Approve),
            KeyCode::Char('n') => Some(ApprovalDecision::Deny),
            KeyCode::Char('a') => Some(ApprovalDecision::ApproveAll),
            _ => {
                // Not a valid approval key — put it back
                app.pending_approval = Some(exchange);
                return;
            }
        };
        if let Some(decision) = decision {
            let _ = exchange.decision_tx.send(decision);
        }
        return;
    }

    // Normal input mode
    match key.code {
        KeyCode::Enter => {
            if !app.input.is_empty() {
                let text = std::mem::take(&mut app.input);
                app.cursor = 0;
                if text == "/quit" || text == "/exit" {
                    app.should_quit = true;
                    return;
                }
                // Write to session DB — triggers server callback
                let mut session = Session::new(
                    crate::types::ConversationId(transport_id.to_string()),
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
        KeyCode::Up => {
            app.scroll_offset = app.scroll_offset.saturating_add(3);
        }
        KeyCode::Down => {
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
        }
        KeyCode::Esc => {
            app.should_quit = true;
        }
        _ => {}
    }
}

fn ui(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Min(1),    // messages
        Constraint::Length(3), // input box
    ])
    .split(f.area());

    // === Messages area ===
    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.entries {
        if entry.entry_type != EntryType::Message {
            continue;
        }

        let is_agent = app.agent_names.contains(&entry.sender);
        let sender_style = if is_agent {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        };

        // Sender line
        lines.push(Line::from(vec![Span::styled(
            format!("{}:", entry.sender),
            sender_style,
        )]));

        // Content lines
        for content_line in entry.content.lines() {
            lines.push(Line::from(format!("  {content_line}")));
        }

        // Blank separator
        lines.push(Line::from(""));
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
    let messages_height = chunks[0].height.saturating_sub(2); // subtract borders
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
        .block(Block::bordered().title(" Chaz TUI "));
    f.render_widget(messages, chunks[0]);

    // === Input area ===
    let input = Paragraph::new(app.input.as_str()).block(Block::bordered().title(" > "));
    f.render_widget(input, chunks[1]);

    // Position cursor in input box
    let cursor_x = chunks[1].x + app.cursor as u16 + 1; // +1 for border
    let cursor_y = chunks[1].y + 1; // +1 for border
    f.set_cursor_position((cursor_x, cursor_y));
}
