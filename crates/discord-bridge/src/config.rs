//! The Discord bridge's own config file + idempotent credential seeding.
//!
//! Distinct from chaz's runtime config: the bridge owns its eidetica key, its
//! state dir, and the bot token chaz deliberately no longer holds.
//! [`DiscordBridgeConfig`] is that file's shape. The bot token is given as a
//! `${ENV}` reference (or literal, or left to the `DISCORD_TOKEN` env var) and
//! resolved via
//! [`SecretStore::resolve_env`](chaz_core::security::SecretStore::resolve_env)
//! at seed time, so the file never carries a plaintext secret.
//!
//! [`DiscordBridgeConfig::seed_into`] resolves every login and writes its
//! [`DiscordCredentials`] into the bridge settings DB; it's idempotent, so the
//! bridge can run it on every boot. Standing up the bridge's eidetica `User`,
//! bootstrapping access, and registering the public `LoginRef` pointer are the
//! binary's job (chaz-core `bridge_identity`); this module stops at "the
//! encrypted creds are in the bridge DB".

use crate::credentials::DiscordCredentials;
use chaz_core::bridge_db::BridgeDb;
use chaz_core::security::SecretStore;
use serde::Deserialize;
use std::collections::HashSet;

/// The Discord bridge's own config file.
/// `Debug` is hand-written to redact `unlock_password` (a literal value is a
/// valid config, so it can be a plaintext secret).
#[derive(Clone, Deserialize)]
pub struct DiscordBridgeConfig {
    /// State directory for the bridge's eidetica DB + key material. When unset
    /// the binary falls back to a platform default.
    #[serde(default)]
    pub state_dir: Option<String>,

    /// Label for this bridge's settings DB — the `bridge:<label>` name passed
    /// to [`create_bridge_db`](chaz_core::bridge_db::create_bridge_db).
    #[serde(default = "default_label")]
    pub label: String,

    /// Password that unlocks the bridge settings DB's encrypted credentials
    /// store. A `${ENV}` reference is resolved at seed time.
    pub unlock_password: String,

    /// The per-agent logins this bridge manages.
    #[serde(default)]
    pub logins: Vec<DiscordLoginConfig>,
}

impl std::fmt::Debug for DiscordBridgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordBridgeConfig")
            .field("state_dir", &self.state_dir)
            .field("label", &self.label)
            .field("unlock_password", &"<redacted>")
            .field("logins", &self.logins)
            .finish()
    }
}

/// One login a Discord bridge manages, tying a bot token to the agent that
/// owns it.
///
/// `Debug` is hand-written to redact `bot_token`.
#[derive(Clone, Deserialize)]
pub struct DiscordLoginConfig {
    /// Display name of the agent this login belongs to. Its AgentDb is where
    /// the public `LoginRef` pointer gets registered.
    pub agent: String,

    /// Access ticket for this agent's DB — produced by the chaz daemon's
    /// `/agent share`. The bridge bootstraps Write access through it and
    /// registers the login pointer on the agent DB it points at.
    pub ticket: String,

    /// Routing id stamped into every inbound entry's `TransportRef::login_id`
    /// and matched when delivering replies. Defaults to `"discord"`.
    #[serde(default = "default_login_id")]
    pub login_id: String,

    /// Bot token. A `${ENV}` reference (or literal); when unset, falls back to
    /// the `DISCORD_TOKEN` env var so the secret can stay out of the file.
    #[serde(default)]
    pub bot_token: Option<String>,

    /// Optional allow-list of Discord user ids permitted to talk to the bot.
    /// Empty means "allow everyone".
    #[serde(default)]
    pub allowed_users: HashSet<u64>,
}

impl std::fmt::Debug for DiscordLoginConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordLoginConfig")
            .field("agent", &self.agent)
            .field("ticket", &self.ticket)
            .field("login_id", &self.login_id)
            .field("bot_token", &self.bot_token.as_ref().map(|_| "<redacted>"))
            .field("allowed_users", &self.allowed_users)
            .finish()
    }
}

fn default_label() -> String {
    "discord".to_string()
}

fn default_login_id() -> String {
    "discord".to_string()
}

impl DiscordLoginConfig {
    /// Map this entry to its `(login_id, credentials)`, resolving the bot token
    /// from a `${ENV}` reference / literal / the `DISCORD_TOKEN` env var.
    pub fn to_credentials(&self) -> anyhow::Result<(String, DiscordCredentials)> {
        let bot_token = match &self.bot_token {
            Some(raw) if !raw.is_empty() => {
                SecretStore::resolve_env(raw).map_err(|e| anyhow::anyhow!(e))?
            }
            _ => std::env::var("DISCORD_TOKEN").map_err(|_| {
                anyhow::anyhow!(
                    "no Discord bot token for login {}: set `bot_token` (literal or \
                     ${{ENV}}) or the DISCORD_TOKEN environment variable",
                    self.login_id
                )
            })?,
        };
        Ok((
            self.login_id.clone(),
            DiscordCredentials {
                bot_token,
                allowed_users: self.allowed_users.clone(),
            },
        ))
    }
}

impl DiscordBridgeConfig {
    /// Resolve the settings-DB unlock password (expanding a `${ENV}` ref).
    pub fn resolve_unlock_password(&self) -> anyhow::Result<String> {
        SecretStore::resolve_env(&self.unlock_password).map_err(|e| anyhow::anyhow!(e))
    }

    /// Seed every configured login's credentials into `bridge_db`, encrypting
    /// them under the resolved unlock password. Idempotent — safe to run on
    /// every boot.
    pub async fn seed_into(&self, bridge_db: &BridgeDb) -> anyhow::Result<()> {
        let unlock = self.resolve_unlock_password()?;
        for entry in &self.logins {
            let (login_id, creds) = entry.to_credentials()?;
            bridge_db
                .seed_credentials(&login_id, &creds, &unlock)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_with(unlock_var: &str, token_var: &str) -> String {
        format!(
            r#"
state_dir: /var/lib/chaz-discord
label: discord
unlock_password: ${{{unlock_var}}}
logins:
  - agent: chaz
    ticket: "eidetica:?db=sha256:agentdbid&pr=iroh:peeraddr"
    login_id: discord
    bot_token: ${{{token_var}}}
    allowed_users: [123, 456]
"#
        )
    }

    #[test]
    fn parses_full_config() {
        let cfg: DiscordBridgeConfig =
            serde_yaml::from_str(&sample_with("UNLOCK", "TOKEN")).unwrap();
        assert_eq!(cfg.state_dir.as_deref(), Some("/var/lib/chaz-discord"));
        assert_eq!(cfg.label, "discord");
        assert_eq!(cfg.logins.len(), 1);
        let entry = &cfg.logins[0];
        assert_eq!(entry.agent, "chaz");
        assert_eq!(
            entry.ticket,
            "eidetica:?db=sha256:agentdbid&pr=iroh:peeraddr"
        );
        assert_eq!(entry.login_id, "discord");
        assert_eq!(entry.allowed_users.len(), 2);
    }

    #[test]
    fn label_and_login_id_default_when_omitted() {
        let cfg: DiscordBridgeConfig =
            serde_yaml::from_str("unlock_password: literal-pw\n").unwrap();
        assert_eq!(cfg.label, "discord");
        assert!(cfg.logins.is_empty());
    }

    #[test]
    fn to_credentials_resolves_env_token() {
        // SAFETY: single-threaded per #[test]; var name scoped to this test.
        unsafe { std::env::set_var("CHAZ_DISCORD_TOKEN_RESOLVE", "bot-token-xyz") };
        let cfg: DiscordBridgeConfig = serde_yaml::from_str(&sample_with(
            "CHAZ_DISCORD_UNLOCK_RESOLVE",
            "CHAZ_DISCORD_TOKEN_RESOLVE",
        ))
        .unwrap();
        let (login_id, creds) = cfg.logins[0].to_credentials().unwrap();
        unsafe { std::env::remove_var("CHAZ_DISCORD_TOKEN_RESOLVE") };

        assert_eq!(login_id, "discord");
        assert_eq!(creds.bot_token, "bot-token-xyz");
        assert!(creds.allowed_users.contains(&123));
    }
}
