//! The Discord gateway: a bidirectional translator between one Discord
//! channel and its eidetica session DB, written entirely against the
//! `chaz_core` gateway SDK.
//!
//! Inbound (`EventHandler::message`): gate the message, resolve the
//! channel→session binding through the transport-agnostic registry, ensure
//! the owning agent hosts it, register it with the server (so the runtime
//! answers), and stamp an inbound entry via
//! [`chaz_core::gateway::inbound_user_entry`].
//!
//! Outbound: [`chaz_core::gateway::attach_reconciler`] installs an `on_write`
//! callback that converges the channel to DB state; the only Discord-specific
//! part is the `send` closure (`ChannelId::say`). The reconcile rule, the
//! delivered-set, and the `[guest]` prefixing all live in the lib — the same
//! code the Matrix gateway runs.

use std::collections::HashSet;
use std::sync::Arc;

use chaz_core::backends::BackendManager;
use chaz_core::config::Config;
use chaz_core::gateway::{Gateway, attach_reconciler, inbound_user_entry};
use chaz_core::security::SecretStore;
use chaz_core::server::Server;
use chaz_core::session::Session;
use chaz_core::types::ConversationId;

use serenity::async_trait;
use serenity::http::Http;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::model::id::ChannelId;
use serenity::prelude::*;

use tokio::sync::Mutex;
use tracing::{error, info};

use crate::config::DiscordConfig;

/// A standalone Discord gateway bound to one login/agent.
pub struct DiscordGateway {
    pub config: Config,
    pub discord: DiscordConfig,
    pub secrets: SecretStore,
}

impl DiscordGateway {
    pub fn new(config: Config, discord: DiscordConfig, secrets: SecretStore) -> Self {
        Self {
            config,
            discord,
            secrets,
        }
    }
}

impl Gateway for DiscordGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let token = self.discord.resolve_token()?;

        // MESSAGE_CONTENT is privileged — it must also be toggled on in the
        // Discord developer portal for the bot, or message bodies arrive empty.
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let handler = Handler {
            server,
            login_id: self.discord.login_id.clone(),
            owning_agent: self.discord.owning_agent.clone(),
            secrets: self.secrets.clone(),
            config: self.config.clone(),
            allowed_users: self.discord.allowed_users.clone(),
            attached: Arc::new(Mutex::new(HashSet::new())),
        };

        let mut client = Client::builder(&token, intents)
            .event_handler(handler)
            .await?;

        info!(
            login_id = %self.discord.login_id,
            agent = %self.discord.owning_agent,
            "Starting Discord gateway"
        );
        client.start().await?;
        Ok(())
    }
}

/// All shared state the serenity event loop needs. serenity gives each handler
/// method an owned `Context` (with `ctx.http` for sends) but no per-callback
/// capture, so everything lives here behind `Arc`/`Clone`.
struct Handler {
    server: Arc<Server>,
    login_id: String,
    owning_agent: String,
    secrets: SecretStore,
    config: Config,
    allowed_users: HashSet<u64>,
    /// Session db ids that already have a reconciler installed — guards the
    /// `on_write` callback against double-install on later messages.
    attached: Arc<Mutex<HashSet<String>>>,
}

impl Handler {
    /// Inbound path, fallible so the trait method can log uniformly. Returns
    /// `Ok(())` for ignored messages (bot/self/not-allowed).
    async fn handle_message(&self, ctx: &Context, msg: &Message) -> anyhow::Result<()> {
        // Never react to bots (covers our own messages and other integrations).
        if msg.author.bot {
            return Ok(());
        }
        // Allow-list, when configured.
        if !self.allowed_users.is_empty() && !self.allowed_users.contains(&msg.author.id.get()) {
            return Ok(());
        }

        let channel = msg.channel_id.get().to_string();

        // Resolve (or create) the session bound to this channel. The registry
        // is already transport-agnostic — "discord" sits beside "matrix".
        let (_conv_id, session_db) = self
            .server
            .registry()
            .get_or_create_channel_session("discord", &self.login_id, &channel)
            .await?;
        let session_db_id = session_db.root_id().to_string();

        // A login belongs to one agent: ensure it hosts this channel's session
        // (idempotent — only writes on first contact).
        self.ensure_owning_agent_hosts(&session_db_id).await;
        let agent_override = chaz_core::session::read_meta_from_db(&session_db)
            .await
            .agent_name;

        // Per-session backend (config backends; per-session meta overrides are
        // a later phase). Registering makes the server watch the session and
        // run the ReAct loop on inbound writes.
        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());
        self.server
            .register_session(&session_db, backend, agent_override, None)
            .await?;

        // Install the reconciler once per session: outbound delivery is the
        // lib's job; we only supply the Discord send closure.
        self.attach_once(
            ctx.http.clone(),
            msg.channel_id,
            &session_db,
            &session_db_id,
        )
        .await?;

        // Stamp transport provenance so an agent's reply routes back here.
        let display = msg.author.global_name.clone();
        let mut session = Session::new(ConversationId(session_db_id), session_db).await;
        session
            .add_entry(inbound_user_entry(
                "discord",
                &self.login_id,
                &channel,
                &msg.author.name,
                display,
                &msg.content,
                Some(msg.id.get().to_string()),
            ))
            .await;
        Ok(())
    }

    /// Install the outbound reconciler for a channel exactly once, guarded by
    /// the `attached` set. The reconcile rule lives in `chaz_core`; the only
    /// Discord-specific part is the `ChannelId::say` send closure.
    async fn attach_once(
        &self,
        http: Arc<Http>,
        channel_id: ChannelId,
        session_db: &eidetica::Database,
        session_db_id: &str,
    ) -> anyhow::Result<()> {
        let mut attached = self.attached.lock().await;
        if !attached.insert(session_db_id.to_string()) {
            return Ok(());
        }
        drop(attached);
        attach_reconciler(
            session_db,
            self.server.agents_arc(),
            self.owning_agent.clone(),
            move |body| {
                let http = http.clone();
                async move {
                    info!("→ Discord({channel_id}): {}", body.replace('\n', " "));
                    channel_id.say(&http, body).await?;
                    Ok(())
                }
            },
        )
        .await?;
        info!(session_db_id, "Discord reconciler installed");
        Ok(())
    }

    /// On connect, reattach every Discord channel already bound to this login
    /// so scheduled-session output is delivered with no inbound trigger.
    /// Mirrors the Matrix gateway's startup reattach.
    async fn reattach_existing_channels(&self, http: Arc<Http>) {
        let channels = match self.server.registry().list_channels().await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to list channels at startup: {e}");
                return;
            }
        };
        for (transport, chan_login, channel, session_db_id) in channels {
            if transport != "discord" || chan_login != self.login_id {
                continue;
            }
            if let Err(e) = self
                .reattach_channel(http.clone(), &channel, &session_db_id)
                .await
            {
                error!(channel = %channel, "Failed to reattach Discord channel: {e}");
            }
        }
    }

    /// Host + register + reconcile an already-bound channel (no inbound entry).
    async fn reattach_channel(
        &self,
        http: Arc<Http>,
        channel: &str,
        session_db_id: &str,
    ) -> anyhow::Result<()> {
        let (_conv_id, session_db) = self.server.registry().open_session(session_db_id).await?;
        self.ensure_owning_agent_hosts(session_db_id).await;
        let agent_override = chaz_core::session::read_meta_from_db(&session_db)
            .await
            .agent_name;
        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());
        self.server
            .register_session(&session_db, backend, agent_override, None)
            .await?;
        let channel_id = ChannelId::new(channel.parse::<u64>()?);
        self.attach_once(http, channel_id, &session_db, session_db_id)
            .await
    }

    /// Ensure this login's owning agent hosts the given session — mirrors the
    /// Matrix gateway. No-op when the name doesn't resolve to a hosted agent.
    async fn ensure_owning_agent_hosts(&self, session_db_id: &str) {
        let Some(entry) = self.server.agent_index().find_by_name(&self.owning_agent) else {
            return;
        };
        if let Err(e) = self
            .server
            .registry()
            .ensure_session_host(session_db_id, &entry)
            .await
        {
            error!(
                owning_agent = %self.owning_agent,
                session_db_id, "Failed to set owning agent as session host: {e}"
            );
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if let Err(e) = self.handle_message(&ctx, &msg).await {
            error!(channel = %msg.channel_id, "Discord inbound error: {e}");
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(bot = %ready.user.name, "Discord gateway connected");
        // Reattach already-bound channels so scheduled output flows without an
        // inbound message first.
        self.reattach_existing_channels(ctx.http.clone()).await;
    }
}
