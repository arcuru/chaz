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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use chaz_core::backends::BackendManager;
use chaz_core::commands::{self, CommandContext, CommandOutcome, Parsed};
use chaz_core::config::Config;
use chaz_core::gateway::{
    ApprovalDecision, ApprovalExchange, Gateway, attach_reconciler, inbound_user_entry,
};
use chaz_core::security::SecretStore;
use chaz_core::server::Server;
use chaz_core::session::Session;
use chaz_core::types::ConversationId;

use serenity::async_trait;
use serenity::http::Http;
use serenity::model::channel::{Message, Reaction, ReactionType};
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId, UserId};
use serenity::prelude::*;

use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{error, info};

use crate::config::DiscordConfig;

/// A tool-approval prompt routed to a specific channel. The runtime hands the
/// gateway an [`ApprovalExchange`]; the relay tags it with the channel to post
/// in.
struct ApprovalRequest {
    channel_id: ChannelId,
    exchange: ApprovalExchange,
}

/// Approval prompt message id → the channel waiting on a decision. A reaction
/// on that message resolves it.
type PendingApprovals = Arc<Mutex<HashMap<MessageId, oneshot::Sender<ApprovalDecision>>>>;

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
        // Reaction intents drive the approve/deny-via-emoji flow.
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILD_MESSAGE_REACTIONS
            | GatewayIntents::DIRECT_MESSAGE_REACTIONS;

        let (approval_relay_tx, approval_relay_rx) = mpsc::channel::<ApprovalRequest>(64);
        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

        let handler = Handler {
            server,
            login_id: self.discord.login_id.clone(),
            owning_agent: self.discord.owning_agent.clone(),
            secrets: self.secrets.clone(),
            config: self.config.clone(),
            allowed_users: self.discord.allowed_users.clone(),
            attached: Arc::new(Mutex::new(HashSet::new())),
            approval_relay_tx,
            pending_approvals: pending_approvals.clone(),
            bot_id: Arc::new(OnceLock::new()),
        };

        let mut client = Client::builder(&token, intents)
            .event_handler(handler)
            .await?;

        // The Arc<Http> send handle, captured before the event loop owns the
        // client — serenity's analog of the cloned matrix-sdk client. The
        // approval relay posts prompts and seeds reactions through it.
        let http = client.http.clone();
        tokio::spawn(approval_relay(approval_relay_rx, http, pending_approvals));

        info!(
            login_id = %self.discord.login_id,
            agent = %self.discord.owning_agent,
            "Starting Discord gateway"
        );
        client.start().await?;
        Ok(())
    }
}

/// Background task: turn each [`ApprovalRequest`] into a Discord prompt with
/// seeded ✅/❌/⏭️ reactions, and remember the message so a reaction resolves
/// the decision. Posting failure denies (safe default).
async fn approval_relay(
    mut rx: mpsc::Receiver<ApprovalRequest>,
    http: Arc<Http>,
    pending: PendingApprovals,
) {
    while let Some(req) = rx.recv().await {
        let info = &req.exchange.info;
        let body = format!(
            "🔒 **Tool approval required**\n\n\
             **Tool:** `{}`\n\
             **Risk:** {:?}\n\
             **Args:** `{}`\n\n\
             React: ✅ approve · ❌ deny · ⏭️ approve all\n\
             Or reply: `!chaz approve` / `!chaz deny`",
            info.name, info.risk_level, info.arguments_display
        );
        match req.channel_id.say(&http, body).await {
            Ok(msg) => {
                for e in ["✅", "❌", "⏭️"] {
                    let _ = msg.react(&http, ReactionType::Unicode(e.to_string())).await;
                }
                pending
                    .lock()
                    .await
                    .insert(msg.id, req.exchange.decision_tx);
            }
            Err(e) => {
                error!("Failed to post approval prompt: {e}");
                let _ = req.exchange.decision_tx.send(ApprovalDecision::Deny);
            }
        }
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
    /// Sends approval prompts to the relay task, tagged with the channel.
    approval_relay_tx: mpsc::Sender<ApprovalRequest>,
    /// Approval prompt message → decision channel; resolved by `reaction_add`.
    pending_approvals: PendingApprovals,
    /// The bot's own user id (set on `ready`), so we ignore the reactions we
    /// seed on approval prompts.
    bot_id: Arc<OnceLock<UserId>>,
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

        // Command channel: `!chaz` followed by a word boundary. A bare `!chazfoo`
        // is a plain message, not a command.
        if let Some(inner) = msg
            .content
            .trim_start()
            .strip_prefix("!chaz")
            .and_then(|r| {
                (r.is_empty() || r.starts_with(char::is_whitespace)).then(|| r.trim().to_string())
            })
        {
            return self.handle_command(ctx, msg, &inner).await;
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
        let approval_tx = self.make_channel_approval_tx(msg.channel_id);
        self.server
            .register_session(&session_db, backend, agent_override, Some(approval_tx))
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
        let channel_id = ChannelId::new(channel.parse::<u64>()?);
        let approval_tx = self.make_channel_approval_tx(channel_id);
        self.server
            .register_session(&session_db, backend, agent_override, Some(approval_tx))
            .await?;
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

    /// Build a per-channel approval sender: the runtime sends an
    /// [`ApprovalExchange`] here; a forwarder tags it with `channel_id` and
    /// relays it to the gateway's approval task. Mirrors the Matrix gateway's
    /// per-room approval tx.
    fn make_channel_approval_tx(&self, channel_id: ChannelId) -> mpsc::Sender<ApprovalExchange> {
        let (tx, mut rx) = mpsc::channel::<ApprovalExchange>(8);
        let relay = self.approval_relay_tx.clone();
        tokio::spawn(async move {
            while let Some(exchange) = rx.recv().await {
                if relay
                    .send(ApprovalRequest {
                        channel_id,
                        exchange,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        tx
    }

    /// Handle a `!chaz <inner>` command. View-local verbs (help, approve, deny)
    /// are checked first; everything else normalizes to `/<inner>` and runs
    /// through the shared parser/dispatch — the same grammar Matrix and the TUI
    /// use.
    async fn handle_command(
        &self,
        ctx: &Context,
        msg: &Message,
        inner: &str,
    ) -> anyhow::Result<()> {
        let verb = inner.split_whitespace().next().unwrap_or("");
        match verb {
            "" | "help" => {
                msg.channel_id.say(&ctx.http, help_text()).await?;
                return Ok(());
            }
            "approve" => {
                self.resolve_pending_approval(ctx, msg.channel_id, true)
                    .await;
                return Ok(());
            }
            "deny" => {
                self.resolve_pending_approval(ctx, msg.channel_id, false)
                    .await;
                return Ok(());
            }
            _ => {}
        }

        // Shared vocabulary: `!chaz <rest>` → `/<rest>`.
        let slash = format!("/{inner}");
        let outcome = match commands::parse(&slash) {
            Parsed::Command(cmd) => self.dispatch_shared(cmd, msg.channel_id).await?,
            Parsed::Usage(usage) => CommandOutcome::Text(usage),
            // Unreachable: `slash` always has a leading `/`.
            Parsed::NotCommand => return Ok(()),
        };
        self.render_outcome(ctx, msg.channel_id, outcome).await;
        Ok(())
    }

    /// Run a parsed shared command against this channel's session.
    async fn dispatch_shared(
        &self,
        cmd: commands::Command,
        channel_id: ChannelId,
    ) -> anyhow::Result<CommandOutcome> {
        let channel = channel_id.get().to_string();
        let (_conv_id, session_db) = self
            .server
            .registry()
            .get_or_create_channel_session("discord", &self.login_id, &channel)
            .await?;
        let session_db_id = session_db.root_id().to_string();
        self.ensure_owning_agent_hosts(&session_db_id).await;
        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());
        let meta = chaz_core::session::read_meta_from_db(&session_db).await;
        let agent = self
            .server
            .registry()
            .resolve_agent(&session_db_id, None, self.server.agent_index())
            .await;
        let ctx = CommandContext {
            server: &self.server,
            secrets: &self.secrets,
            backend: &backend,
            session_db_id: &session_db_id,
            session_db: &session_db,
            current_agent: &agent.name,
            session_name: meta.name.as_deref(),
        };
        Ok(commands::dispatch(cmd, &ctx).await)
    }

    /// Render a [`CommandOutcome`] to the channel.
    async fn render_outcome(&self, ctx: &Context, channel_id: ChannelId, outcome: CommandOutcome) {
        let text = match outcome {
            CommandOutcome::Text(t) => t,
            CommandOutcome::Error(e) => format!("!chaz Error: {e}"),
            CommandOutcome::SessionsList(list) => {
                if list.is_empty() {
                    "No sessions found.".to_string()
                } else {
                    let mut s = String::from("Sessions:");
                    for info in &list {
                        let agent = info.agent_name.as_deref().unwrap_or("default");
                        let name = info
                            .name
                            .as_deref()
                            .map(|n| format!(" \"{n}\""))
                            .unwrap_or_default();
                        s.push_str(&format!(
                            "\n  {}{} ({}, {} entries)",
                            info.session_db_id, name, agent, info.entry_count
                        ));
                        if let Some(preview) = &info.last_message {
                            s.push_str(&format!("\n    {preview}"));
                        }
                    }
                    s
                }
            }
            CommandOutcome::SessionSwitched(_) => {
                "!chaz Channel↔session binding is fixed per channel on Discord.".to_string()
            }
            CommandOutcome::Quit => return,
        };
        if let Err(e) = msg_say(ctx, channel_id, text).await {
            error!("Failed to send command response: {e}");
        }
    }

    /// Approve or deny the oldest pending tool-approval request for `channel`.
    async fn resolve_pending_approval(&self, ctx: &Context, channel_id: ChannelId, approve: bool) {
        let mut p = self.pending_approvals.lock().await;
        let Some(message_id) = p.keys().next().copied() else {
            let _ = msg_say(ctx, channel_id, "No pending approval requests".to_string()).await;
            return;
        };
        if let Some(tx) = p.remove(&message_id) {
            let decision = if approve {
                ApprovalDecision::Approve
            } else {
                ApprovalDecision::Deny
            };
            let _ = tx.send(decision);
            let label = if approve {
                "✅ Approved"
            } else {
                "❌ Denied"
            };
            let _ = msg_say(ctx, channel_id, label.to_string()).await;
        }
    }
}

/// Small helper so the various send sites read uniformly.
async fn msg_say(ctx: &Context, channel_id: ChannelId, text: String) -> serenity::Result<()> {
    channel_id.say(&ctx.http, text).await.map(|_| ())
}

/// Help text for `!chaz help` — the shared `/`-vocabulary plus the Discord-local
/// verbs, listed under the `!chaz ` prefix this transport uses.
fn help_text() -> String {
    [
        "**chaz commands** (prefix with `!chaz `):",
        "",
        "`sessions` · `info` · `print` · `compact` · `name [<alias>]` — session ops",
        "`agents` · `agent <add|remove|host|new|delete|share|import|set|reload|invite|rehost> …` — living agents",
        "`model [<id>|<agent> <id>]` · `role [<name> [prompt]]` · `backend <name> <url> <key>` · `backends` — LLM config",
        "`extensions <list|add|remove|settings|set> …` — per-session extensions",
        "`channels` — channels bound to this session",
        "",
        "Discord-local: `approve` · `deny` (or react ✅/❌/⏭️ on the prompt)",
        "",
        "In a channel the bot watches, just talk — no prefix needed.",
    ]
    .join("\n")
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
        // Remember our own id so we ignore the reactions we seed on prompts.
        let _ = self.bot_id.set(ready.user.id);
        // Reattach already-bound channels so scheduled output flows without an
        // inbound message first.
        self.reattach_existing_channels(ctx.http.clone()).await;
    }

    async fn reaction_add(&self, _ctx: Context, add: Reaction) {
        // Ignore the bot's own seed reactions.
        if add.user_id == self.bot_id.get().copied() {
            return;
        }
        let ReactionType::Unicode(emoji) = &add.emoji else {
            return;
        };
        let decision = match emoji.as_str() {
            "✅" => ApprovalDecision::Approve,
            "❌" => ApprovalDecision::Deny,
            "⏭" | "⏭️" => ApprovalDecision::ApproveAll,
            _ => return,
        };
        let mut p = self.pending_approvals.lock().await;
        if let Some(tx) = p.remove(&add.message_id) {
            info!(channel = %add.channel_id, ?decision, "Approval decision via reaction");
            let _ = tx.send(decision);
        }
    }
}
