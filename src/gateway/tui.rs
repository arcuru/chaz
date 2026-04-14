use crate::backends::BackendManager;
use crate::config::Config;
use crate::defaults::DEFAULT_CONFIG;
use crate::gateway::{ApprovalDecision, ApprovalExchange, ChatRequest, ChatResponse, Gateway};
use crate::role::get_role;

use std::io::{self, Write};
use tokio::sync::{mpsc, oneshot};

pub struct TuiGateway {
    config: Config,
}

impl TuiGateway {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl Gateway for TuiGateway {
    async fn run(self, event_tx: mpsc::Sender<ChatRequest>) -> anyhow::Result<()> {
        let transport_id = "tui".to_string();

        // Resolve role from config (no transport-specific overrides for TUI)
        let role_override = get_role(
            self.config.role.clone(),
            self.config.roles.clone(),
            DEFAULT_CONFIG.roles.clone(),
        );

        println!("Chaz TUI \u{2014} type /quit to exit\n");

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

            let backend = BackendManager::new(&self.config.backends);

            // Create approval channel for this request
            let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalExchange>(8);

            let (response_tx, response_rx) = oneshot::channel();
            event_tx
                .send(ChatRequest {
                    transport_id: transport_id.clone(),
                    sender: "user".to_string(),
                    body: line,
                    model_override: None,
                    role_override: role_override.clone(),
                    backend,
                    response_tx,
                    backfill_history: None,
                    approval_tx: Some(approval_tx),
                })
                .await?;

            // Handle both approval requests and final response concurrently.
            // Pin the response future so we can poll it across select iterations.
            let mut response_fut = Box::pin(response_rx);
            let response = loop {
                tokio::select! {
                    // Handle approval requests from the runtime
                    Some(exchange) = approval_rx.recv() => {
                        let decision = prompt_approval(&exchange);
                        let _ = exchange.decision_tx.send(decision);
                    }
                    // Wait for the final response
                    result = &mut response_fut => {
                        break result;
                    }
                }
            };

            match response {
                Ok(ChatResponse::Message { body, .. }) => {
                    println!("\n{}\n", body);
                }
                Ok(ChatResponse::Error { error }) => {
                    eprintln!("\nError: {}\n", error);
                }
                Ok(ChatResponse::Skipped) => {}
                Err(e) => {
                    eprintln!("\nChannel error: {}\n", e);
                }
            }
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
