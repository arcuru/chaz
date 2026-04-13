use crate::backends::BackendManager;
use crate::config::Config;
use crate::defaults::DEFAULT_CONFIG;
use crate::gateway::{ChatRequest, ChatResponse};
use crate::role::get_role;
use crate::types::ConversationId;

use std::io::{self, Write};
use tokio::sync::{mpsc, oneshot};

pub struct TuiGateway {
    config: Config,
}

impl TuiGateway {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self, event_tx: mpsc::Sender<ChatRequest>) -> anyhow::Result<()> {
        let conversation_id = ConversationId("tui".to_string());

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

            let (response_tx, response_rx) = oneshot::channel();
            event_tx
                .send(ChatRequest {
                    conversation_id: conversation_id.clone(),
                    sender: "user".to_string(),
                    body: line,
                    model_override: None,
                    role_override: role_override.clone(),
                    backend,
                    response_tx,
                })
                .await?;

            match response_rx.await? {
                ChatResponse::Message { body, .. } => {
                    println!("\n{}\n", body);
                }
                ChatResponse::Error { error } => {
                    eprintln!("\nError: {}\n", error);
                }
            }
        }

        Ok(())
    }
}
