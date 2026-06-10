//! Discord-specific config, layered over the shared chaz config file.
//!
//! The standalone binary loads the same YAML chaz uses (for `state_dir`,
//! `backends`, `agents`) as a [`chaz_core::config::Config`], then reads its
//! own `discord:` section out of the same file via [`DiscordRoot`]. Keeping
//! the Discord fields in a crate-local struct is the whole point of the
//! standalone model — chaz-core never has to learn about Discord.

use serde::Deserialize;
use std::collections::HashSet;

/// Wrapper to pull just the `discord:` block out of the shared config file.
/// Every other top-level chaz key is ignored here (the same bytes are parsed
/// separately as a full [`chaz_core::config::Config`]).
#[derive(Debug, Deserialize)]
pub struct DiscordRoot {
    pub discord: DiscordConfig,
}

/// The `discord:` section of the config file.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscordConfig {
    /// Bot token. When unset, falls back to the `DISCORD_TOKEN` env var so the
    /// secret can stay out of the config file.
    #[serde(default)]
    pub bot_token: Option<String>,

    /// Routing id stamped into every inbound entry's `TransportRef::login_id`
    /// and matched by the publisher when delivering replies. One login per
    /// binding; defaults to `"discord"`.
    #[serde(default = "default_login_id")]
    pub login_id: String,

    /// The agent that owns this login. Its writes go out plain; other agents
    /// writing into the session are shown with an `[AgentName]` prefix.
    pub owning_agent: String,

    /// Optional allow-list of Discord user ids permitted to talk to the bot.
    /// Empty means "allow everyone" (bots and the bot's own messages are
    /// always ignored regardless).
    #[serde(default)]
    pub allowed_users: HashSet<u64>,
}

fn default_login_id() -> String {
    "discord".to_string()
}

impl DiscordConfig {
    /// Resolve the bot token from config or the `DISCORD_TOKEN` env var.
    pub fn resolve_token(&self) -> anyhow::Result<String> {
        if let Some(t) = &self.bot_token
            && !t.is_empty()
        {
            return Ok(t.clone());
        }
        std::env::var("DISCORD_TOKEN").map_err(|_| {
            anyhow::anyhow!(
                "no Discord bot token: set `discord.bot_token` in the config or the \
                 DISCORD_TOKEN environment variable"
            )
        })
    }
}
