//! Single-shot command bridge. Dispatches one `/command` through the shared
//! command layer, prints the outcome on stdout, and exits.
//!
//! This is the headless twin of the TUI's command line. Every `/command` the
//! TUI and the Matrix bridge accept resolves through the same
//! [`shared_commands::parse`] + [`shared_commands::dispatch`] pair, so a
//! script reaches the whole grammar — `/sharing requests`, `/agent invite`,
//! `/agent share` — without a terminal or a Matrix room in the loop. That
//! matters most for bring-up: a bridge's access to an agent DB is granted by
//! commands, and needing a human at a TUI to run them is what kept bridge
//! setup out of scripts and tests.
//!
//! Session handling matches the CLI bridge: a fresh ephemeral session per
//! invocation, or a named one reused across invocations via `--session`. Most
//! commands here are peer-scoped and ignore it, but session-scoped verbs
//! (`/info`, `/share`) need something to point at.
//!
//! No approval callback is installed, matching `--print`: a command that would
//! need interactive approval is auto-denied rather than hanging.

use chaz_core::backends::BackendManager;
use chaz_core::bridge::Bridge;
use chaz_core::commands::{
    self as shared_commands, CommandContext, CommandOutcome, Parsed, SessionInfo,
};
use chaz_core::config::Config;
use chaz_core::security::SecretStore;
use chaz_core::server::Server;

use std::sync::Arc;

pub struct CommandBridge {
    config: Config,
    secrets: SecretStore,
    /// The raw command line, including its leading `/`.
    command: String,
    session_name: Option<String>,
}

impl CommandBridge {
    pub fn new(
        config: Config,
        secrets: SecretStore,
        command: String,
        session_name: Option<String>,
    ) -> Self {
        Self {
            config,
            secrets,
            command,
            session_name,
        }
    }
}

/// Render a `SessionsList` outcome as one line per session. The interactive
/// frontends render this into a picker; a script wants something greppable.
fn render_sessions(list: &[SessionInfo]) -> String {
    if list.is_empty() {
        return "No sessions found.".to_string();
    }
    let mut out = String::new();
    for info in list {
        let name = info.name.as_deref().unwrap_or("-");
        let agent = info.agent_name.as_deref().unwrap_or("default");
        out.push_str(&format!(
            "{}\t{}\t{}\t{:?}\t{} entries\n",
            info.session_db_id, name, agent, info.bridge, info.entry_count
        ));
    }
    out
}

impl Bridge for CommandBridge {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        // Reject non-commands up front rather than silently treating them as a
        // prompt — `chaz cmd "hello"` is a mistake, and running it as an LLM
        // turn would be a surprising and billable way to report that.
        let cmd = match shared_commands::parse(&self.command) {
            Parsed::Command(c) => c,
            Parsed::Usage(hint) => anyhow::bail!("{hint}"),
            Parsed::NotCommand => anyhow::bail!(
                "not a command: {:?} — expected a leading '/' (try '/help' in the TUI for the grammar)",
                self.command
            ),
        };

        let (_conv_id, session_db) =
            super::cli::resolve_cli_session(&server, self.session_name.as_deref()).await?;
        let session_db_id = session_db.root_id().to_string();

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());
        let meta = chaz_core::session::read_meta_from_db(&session_db).await;
        let agent = server
            .registry()
            .resolve_agent(&session_db_id, None, server.agent_index())
            .await;

        let ctx = CommandContext {
            server: &server,
            secrets: &self.secrets,
            backend: &backend,
            session_db_id: &session_db_id,
            session_db: &session_db,
            current_agent: &agent.name,
            session_name: meta.name.as_deref(),
        };

        // Exit status is the only thing a calling script can branch on, so an
        // `Error` outcome has to leave through `Err` rather than being printed
        // as if it were a result.
        match shared_commands::dispatch(cmd, &ctx).await {
            CommandOutcome::Text(t) => {
                println!("{t}");
                Ok(())
            }
            CommandOutcome::Error(e) => anyhow::bail!("{e}"),
            CommandOutcome::SessionsList(list) => {
                print!("{}", render_sessions(&list));
                Ok(())
            }
            // Both are control-flow signals for a long-lived frontend. A
            // one-shot run has nothing to switch to and nothing to quit, so
            // they resolve as a successful no-op.
            CommandOutcome::SessionSwitched(s) => {
                println!("{}", s.session_db_id);
                Ok(())
            }
            CommandOutcome::Quit => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chaz_core::session::{BridgeKind, SessionStatus};

    fn info(name: Option<&str>, agent: Option<&str>) -> SessionInfo {
        SessionInfo {
            session_db_id: "bafyabc".to_string(),
            agent_name: agent.map(str::to_string),
            name: name.map(str::to_string),
            entry_count: 3,
            last_message: None,
            bridge: BridgeKind::Cli,
            created_at: None,
            status: SessionStatus::default(),
            total_cost_usd: 0.0,
            cost_reported: false,
            llm_call_count: 0,
            loaded: true,
        }
    }

    #[test]
    fn empty_session_list_is_a_sentence_not_a_blank() {
        assert_eq!(render_sessions(&[]), "No sessions found.");
    }

    #[test]
    fn sessions_render_one_tab_separated_row_each() {
        let out = render_sessions(&[info(Some("work"), Some("chaz")), info(None, None)]);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].starts_with("bafyabc\twork\tchaz\t"));
        // Absent name and agent have to render as placeholders rather than
        // empty fields, or the columns shift and `cut -f3` reads the wrong one.
        assert!(rows[1].starts_with("bafyabc\t-\tdefault\t"));
    }

    #[test]
    fn every_row_has_the_same_field_count() {
        let out = render_sessions(&[info(Some("a"), Some("b")), info(None, None)]);
        let counts: Vec<usize> = out.lines().map(|l| l.matches('\t').count()).collect();
        assert_eq!(counts, vec![4, 4]);
    }
}
