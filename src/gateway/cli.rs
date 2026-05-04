//! Single-shot CLI gateway. Sends one prompt to a reused "cli" session
//! and prints the agent's reply on stdout.
//!
//! No interactive approval — tools requiring approval are auto-denied
//! (see [`SecurityContext::request_approval`] when `approval_callback` is None).

use crate::backends::BackendManager;
use crate::config::Config;
use crate::gateway::Gateway;
use crate::security::SecretStore;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

/// Name used for the CLI's reused session. Created on first invocation,
/// reopened on subsequent ones.
const CLI_DEFAULT_NAME: &str = "cli";

pub struct CliGateway {
    config: Config,
    secrets: SecretStore,
    prompt: String,
}

impl CliGateway {
    pub fn new(config: Config, secrets: SecretStore, prompt: String) -> Self {
        Self {
            config,
            secrets,
            prompt,
        }
    }
}

/// Find the session named "cli", or create one and name it.
async fn default_cli_session(
    server: &Server,
) -> anyhow::Result<(crate::types::ConversationId, eidetica::Database)> {
    if let Some(id) = server.registry().find_by_name(CLI_DEFAULT_NAME).await? {
        match server.registry().open_session(&id).await {
            Ok(r) => return Ok(r),
            Err(e) => warn!(id, "Default CLI session unreadable, recreating: {e}"),
        }
    }
    let (conv_id, db) = server.registry().create_session(Some("cli")).await?;
    let session_db_id = db.root_id().to_string();
    if let Err(e) = server
        .registry()
        .set_session_name(&session_db_id, CLI_DEFAULT_NAME.to_string())
        .await
    {
        warn!("Failed to name default CLI session: {e}");
    }
    Ok((conv_id, db))
}

impl Gateway for CliGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let (conv_id, session_db) = default_cli_session(&server).await?;

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        // Approval channel = None → auto-deny any tool needing approval.
        server
            .register_session(&session_db, backend, None, None)
            .await?;

        // Watch for the agent's response via on_local_write. The server already
        // installed its own callback during register_session; this is an additional
        // listener for response detection.
        let (notify_tx, mut notify_rx) = mpsc::channel::<()>(8);
        session_db.on_local_write(move |_entry, _db, _instance| {
            let tx = notify_tx.clone();
            Box::pin(async move {
                let _ = tx.send(()).await;
                Ok(())
            })
        })?;

        let agent_names: HashSet<String> = server
            .agents()
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        // Send the prompt as a user message.
        let mut session = Session::new(conv_id.clone(), session_db.clone()).await;
        session
            .add_entry(SessionEntry {
                sender: "user".to_string(),
                content: self.prompt.clone(),
                timestamp: chrono::Utc::now(),
                entry_type: EntryType::Message,
            })
            .await;

        // Wait for an agent reply (Message from a sender in agent_names) or an Error.
        while notify_rx.recv().await.is_some() {
            let session = Session::new(conv_id.clone(), session_db.clone()).await;
            if let Some(latest) = session.latest_entry() {
                match latest.entry_type {
                    EntryType::Message if agent_names.contains(&latest.sender) => {
                        println!("{}", latest.content);
                        return Ok(());
                    }
                    EntryType::Error if agent_names.contains(&latest.sender) => {
                        eprintln!("{}", latest.content);
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        anyhow::bail!("CLI session ended before agent responded")
    }
}
