use crate::backends::BackendManager;
use crate::config::Config;
use crate::gateway::{ApprovalDecision, ApprovalExchange, Gateway};
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, SessionEntry};

use std::io::{self, Write};
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

impl Gateway for TuiGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let transport_id = "tui".to_string();

        println!("Chaz TUI \u{2014} type /quit to exit\n");

        // Create approval channel
        let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalExchange>(8);

        // Get or create session DB
        let (_conv_id, session_db) = server
            .registry()
            .get_or_create_session_db(&transport_id)
            .await?;

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        // Register session with server
        server
            .register_session(
                &transport_id,
                &session_db,
                backend.clone(),
                None,
                Some(approval_tx.clone()),
            )
            .await?;

        // Take the response receiver
        let mut response_rx = server.take_response_rx().await;

        loop {
            print!("> ");
            io::stdout().flush()?;

            let line = tokio::task::spawn_blocking(|| {
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                Ok::<_, io::Error>(input)
            })
            .await??;

            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            if line == "/quit" || line == "/exit" {
                break;
            }

            // Write user entry to session DB — triggers server callback
            let mut session = crate::session::Session::new(
                crate::types::ConversationId(transport_id.clone()),
                session_db.clone(),
            )
            .await;
            session
                .add_entry(SessionEntry {
                    sender: "user".to_string(),
                    content: line,
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                })
                .await;

            // Wait for response, handling approval requests concurrently
            let response = loop {
                tokio::select! {
                    Some(exchange) = approval_rx.recv() => {
                        let decision = prompt_approval(&exchange);
                        let _ = exchange.decision_tx.send(decision);
                    }
                    Some(delivery) = response_rx.recv() => {
                        if delivery.transport_id == transport_id {
                            break delivery;
                        }
                    }
                }
            };

            println!("\n{}\n", response.body);
        }

        Ok(())
    }
}

/// Prompt the user for approval of a tool call (blocking stdin read).
fn prompt_approval(exchange: &ApprovalExchange) -> ApprovalDecision {
    let info = &exchange.info;
    eprintln!(
        "\n--- Tool approval required ---\n  Tool: {}\n  Risk: {}\n  Args: {}\n",
        info.name, info.risk_level, info.arguments_display
    );
    eprint!("Approve? [y]es / [n]o / [a]ll: ");
    io::stderr().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return ApprovalDecision::Deny;
    }

    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Approve,
        "a" | "all" => ApprovalDecision::ApproveAll,
        _ => ApprovalDecision::Deny,
    }
}
