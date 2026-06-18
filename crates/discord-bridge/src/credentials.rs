//! The credential blob a Discord bridge stores in its own `BridgeDb`.
//!
//! chaz-core's [`BridgeDb`](chaz_core::bridge_db::BridgeDb) round-trips this as
//! an opaque `Serialize`/`DeserializeOwned` value — the daemon never reads it,
//! and core imposes no schema. This is the bridge's private shape: exactly what
//! `serenity` needs to connect and gate, and nothing the shared world depends
//! on. The only thing that crosses into a shared DB is the
//! [`LoginRef`](chaz_core::agent_db::LoginRef) pointer, seeded separately.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Everything a Discord bridge needs to sign in and run a single login, stored
/// encrypted in the bridge's own settings DB under that login's `login_id`.
/// `Debug` is hand-written to redact `bot_token`; deriving it would print the
/// resolved secret on any `{:?}`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordCredentials {
    /// Resolved bot token (the `${ENV}` reference / `DISCORD_TOKEN` fallback in
    /// the bridge config has already been expanded at seed time).
    pub bot_token: String,
    /// Discord user ids permitted to talk to the bot. Empty means "allow
    /// everyone" (bots and the bot's own messages are always ignored).
    #[serde(default)]
    pub allowed_users: HashSet<u64>,
}

impl std::fmt::Debug for DiscordCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordCredentials")
            .field("bot_token", &"<redacted>")
            .field("allowed_users", &self.allowed_users)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_bot_token() {
        let creds = DiscordCredentials {
            bot_token: "MTA-super-secret-token.xyz".to_string(),
            allowed_users: HashSet::new(),
        };
        let rendered = format!("{creds:?}");
        assert!(
            !rendered.contains("super-secret"),
            "token leaked: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }
}
