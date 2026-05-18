//! Single-shot CLI gateway. Sends one prompt and prints the agent's reply
//! on stdout.
//!
//! Session behavior:
//! - Default: each invocation creates a fresh ephemeral session. Suited to
//!   one-shot batch and schedule use where context accumulation would be
//!   harmful.
//! - `--session NAME`: find-or-create a named session and reuse it across
//!   invocations. Suited to scripted multi-turn workflows.
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

pub struct CliGateway {
    config: Config,
    secrets: SecretStore,
    prompt: String,
    /// Optional named session to reuse. When `None`, a fresh ephemeral
    /// session is created per invocation.
    session_name: Option<String>,
}

impl CliGateway {
    pub fn new(
        config: Config,
        secrets: SecretStore,
        prompt: String,
        session_name: Option<String>,
    ) -> Self {
        Self {
            config,
            secrets,
            prompt,
            session_name,
        }
    }
}

/// Resolve the session to run against:
/// - `Some(name)` → find-or-create a session with that name (reused across runs).
/// - `None`       → create a fresh ephemeral session (no name, no reuse).
async fn resolve_cli_session(
    server: &Server,
    session_name: Option<&str>,
) -> anyhow::Result<(crate::types::ConversationId, eidetica::Database)> {
    if let Some(name) = session_name {
        if let Some(id) = server.registry().find_by_name(name).await? {
            match server.registry().open_session(&id).await {
                Ok(r) => return Ok(r),
                Err(e) => warn!(id, "Named CLI session '{name}' unreadable, recreating: {e}"),
            }
        }
        let (conv_id, db) = server.registry().create_session(Some(name)).await?;
        let session_db_id = db.root_id().to_string();
        if let Err(e) = server
            .registry()
            .set_session_name(&session_db_id, name.to_string())
            .await
        {
            warn!("Failed to name CLI session '{name}': {e}");
        }
        Ok((conv_id, db))
    } else {
        // Ephemeral: create a fresh session every invocation, leave it
        // unnamed. Tagged with source "cli" for debug visibility in session
        // listings.
        server.registry().create_session(Some("cli")).await
    }
}

impl Gateway for CliGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let (conv_id, session_db) =
            resolve_cli_session(&server, self.session_name.as_deref()).await?;

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());

        // Approval channel = None → auto-deny any tool needing approval.
        server
            .register_session(&session_db, backend, None, None)
            .await?;

        // Watch for the agent's response via on_write. The server already
        // installed its own callback during register_session; this is an additional
        // listener for response detection.
        let (notify_tx, mut notify_rx) = mpsc::channel::<()>(8);
        session_db
            .on_write(move |_event, _db| {
                let tx = notify_tx.clone();
                Box::pin(async move {
                    let _ = tx.send(()).await;
                    Ok(())
                })
            })?
            .detach();

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
                metadata: None,
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
