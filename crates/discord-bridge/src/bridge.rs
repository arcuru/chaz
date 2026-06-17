//! The Discord bridge: a bidirectional translator between one Discord
//! channel and its eidetica session DB, written entirely against the
//! `chaz_core` bridge SDK.
//!
//! Inbound (`EventHandler::message`): gate the message, resolve the
//! channel→session binding through the transport-agnostic registry, ensure
//! the owning agent hosts it, register it with the server (so the runtime
//! answers), and stamp an inbound entry via
//! [`chaz_core::bridge::inbound_user_entry`].
//!
//! Outbound: [`chaz_core::bridge::attach_reconciler`] installs an `on_write`
//! callback that converges the channel to DB state; the only Discord-specific
//! part is the `send` closure (`ChannelId::say`). The reconcile rule, the
//! delivered-set, and the `[guest]` prefixing all live in the lib — the same
//! code the Matrix bridge runs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use chaz_core::backends::BackendManager;
use chaz_core::bridge::{
    ApprovalDecision, ApprovalExchange, Bridge, attach_reconciler, inbound_user_entry,
};
use chaz_core::commands::{self, CommandContext, CommandOutcome, Parsed};
use chaz_core::config::Config;
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

use crate::credentials::DiscordCredentials;

/// A tool-approval prompt routed to a specific channel. The runtime hands the
/// bridge an [`ApprovalExchange`]; the relay tags it with the channel to post
/// in.
struct ApprovalRequest {
    channel_id: ChannelId,
    exchange: ApprovalExchange,
}

/// Approval prompt message id → the channel waiting on a decision. A reaction
/// on that message resolves it.
type PendingApprovals = Arc<Mutex<HashMap<MessageId, oneshot::Sender<ApprovalDecision>>>>;

/// A standalone Discord bridge bound to one login/agent. Credentials are
/// resolved (out of the bridge's own `BridgeDb`) before this is built; the
/// `login_id` / `owning_agent` pointer fields come from the bridge config.
pub struct DiscordBridge {
    pub login_id: String,
    pub owning_agent: String,
    pub creds: DiscordCredentials,
    pub config: Config,
    pub secrets: SecretStore,
}

impl DiscordBridge {
    pub fn new(
        login_id: String,
        owning_agent: String,
        creds: DiscordCredentials,
        config: Config,
        secrets: SecretStore,
    ) -> Self {
        Self {
            login_id,
            owning_agent,
            creds,
            config,
            secrets,
        }
    }
}

impl Bridge for DiscordBridge {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let token = self.creds.bot_token.clone();

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
            login_id: self.login_id.clone(),
            owning_agent: self.owning_agent.clone(),
            secrets: self.secrets.clone(),
            config: self.config.clone(),
            allowed_users: self.creds.allowed_users.clone(),
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
            login_id = %self.login_id,
            agent = %self.owning_agent,
            "Starting Discord bridge"
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
                    // Discord rejects any message over DISCORD_MESSAGE_LIMIT
                    // characters, so a long agent reply goes out as several
                    // messages split on natural boundaries. A body that already
                    // fits produces a single chunk (and an empty body, none).
                    for piece in chunk_message(&body, DISCORD_MESSAGE_LIMIT) {
                        channel_id.say(&http, piece).await?;
                    }
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
    /// Mirrors the Matrix bridge's startup reattach.
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
    /// Matrix bridge. No-op when the name doesn't resolve to a hosted agent.
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
    /// relays it to the bridge's approval task. Mirrors the Matrix bridge's
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

/// Discord rejects messages longer than 2000 characters (counted as Unicode
/// scalar values, not bytes). Outbound bodies are split to fit this ceiling.
const DISCORD_MESSAGE_LIMIT: usize = 2000;

/// Split `body` into pieces that each fit within `limit` characters, for
/// delivery as one Discord message apiece.
///
/// Splitting prefers the least disruptive boundary: first newlines, then
/// spaces within an over-long line, and only as a last resort a hard cut
/// through a single oversized token (a URL, a base64 blob). Lengths are
/// measured in `char`s — Discord counts Unicode scalar values — and cuts never
/// fall inside a multi-byte char. A body that already fits yields one chunk; an
/// empty body yields none (nothing to send).
///
/// Pure, so it is unit-testable without a live transport.
fn chunk_message(body: &str, limit: usize) -> Vec<String> {
    pack_parts(body.split('\n'), "\n", limit, |line, lim| {
        pack_parts(line.split(' '), " ", lim, hard_split)
    })
}

/// Greedily pack `parts` (rejoined with `sep`) into chunks of at most `limit`
/// chars. A single part that exceeds `limit` on its own is broken up by
/// `break_part`, whose last piece is left open so following parts can still
/// pack onto it — this avoids stranding a long token's tail on its own line.
fn pack_parts<'a, I, B>(parts: I, sep: &str, limit: usize, break_part: B) -> Vec<String>
where
    I: Iterator<Item = &'a str>,
    B: Fn(&str, usize) -> Vec<String>,
{
    let sep_len = sep.chars().count();
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;

    for part in parts {
        let plen = part.chars().count();
        let need = if cur.is_empty() {
            plen
        } else {
            cur_len + sep_len + plen
        };
        if need <= limit {
            if !cur.is_empty() {
                cur.push_str(sep);
                cur_len += sep_len;
            }
            cur.push_str(part);
            cur_len += plen;
            continue;
        }

        // The part won't pack onto the current chunk: flush it first.
        if !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if plen <= limit {
            // Fits on its own — start a fresh chunk with it.
            cur.push_str(part);
            cur_len = plen;
        } else {
            // Too long even alone: break it, emitting all but the last piece
            // and leaving that last piece open for the next part to join.
            let mut pieces = break_part(part, limit);
            if let Some(last) = pieces.pop() {
                chunks.extend(pieces);
                cur_len = last.chars().count();
                cur = last;
            }
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Last-resort split of a single token that exceeds `limit`: cut it into
/// consecutive `limit`-char pieces on char boundaries. Joining the result back
/// reproduces the input exactly (no separator is inserted or dropped).
fn hard_split(token: &str, limit: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    let mut n = 0usize;
    for ch in token.chars() {
        if n == limit {
            pieces.push(std::mem::take(&mut cur));
            n = 0;
        }
        cur.push(ch);
        n += 1;
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if let Err(e) = self.handle_message(&ctx, &msg).await {
            error!(channel = %msg.channel_id, "Discord inbound error: {e}");
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(bot = %ready.user.name, "Discord bridge connected");
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

#[cfg(test)]
mod tests {
    use super::{chunk_message, hard_split};

    /// Every chunk must respect the char ceiling — the whole point of the split.
    fn assert_within_limit(chunks: &[String], limit: usize) {
        for c in chunks {
            assert!(
                c.chars().count() <= limit,
                "chunk exceeds limit {limit}: {} chars",
                c.chars().count()
            );
        }
    }

    #[test]
    fn short_body_is_one_chunk_unchanged() {
        assert_eq!(chunk_message("hello world", 2000), vec!["hello world"]);
    }

    #[test]
    fn body_at_exactly_the_limit_is_not_split() {
        let body = "x".repeat(2000);
        let chunks = chunk_message(&body, 2000);
        assert_eq!(chunks, vec![body]);
    }

    #[test]
    fn empty_body_sends_nothing() {
        assert!(chunk_message("", 2000).is_empty());
    }

    #[test]
    fn splits_on_newline_boundaries() {
        // Three 40-char lines, limit 100: two lines pack, the third spills over.
        let line = "a".repeat(40);
        let body = format!("{line}\n{line}\n{line}");
        let chunks = chunk_message(&body, 100);
        assert_within_limit(&chunks, 100);
        // First chunk packs two lines joined by the newline; never a torn line.
        assert_eq!(chunks, vec![format!("{line}\n{line}"), line]);
    }

    #[test]
    fn splits_long_line_on_spaces_without_breaking_words() {
        // One line of 10 six-char words ("word00".."word09") = 69 chars.
        let words: Vec<String> = (0..10).map(|i| format!("word{i:02}")).collect();
        let body = words.join(" ");
        let chunks = chunk_message(&body, 20);
        assert_within_limit(&chunks, 20);
        // No word is ever split: every whitespace-delimited token in every chunk
        // is one of the originals.
        for chunk in &chunks {
            for tok in chunk.split(' ') {
                assert!(words.contains(&tok.to_string()), "torn word: {tok:?}");
            }
        }
        // And nothing is lost: flatten the chunks back to the original words.
        let recovered: Vec<String> = chunks
            .iter()
            .flat_map(|c| c.split(' ').map(str::to_string))
            .collect();
        assert_eq!(recovered, words);
    }

    #[test]
    fn hard_splits_a_single_oversized_token() {
        // A 4500-char URL-like blob with no spaces: only a hard cut fits it.
        let blob = "h".repeat(4500);
        let chunks = chunk_message(&blob, 2000);
        assert_within_limit(&chunks, 2000);
        assert_eq!(chunks.len(), 3); // 2000 + 2000 + 500
        assert_eq!(chunks.concat(), blob); // no character lost or duplicated
    }

    #[test]
    fn oversized_token_tail_packs_with_following_words() {
        // A 25-char token then a short word, limit 10: the token hard-splits to
        // [10, 10, 5] and the trailing "hi" packs onto the open 5-char tail
        // rather than stranding on its own message.
        let body = format!("{} hi", "z".repeat(25));
        let chunks = chunk_message(&body, 10);
        assert_within_limit(&chunks, 10);
        assert_eq!(chunks, vec!["zzzzzzzzzz", "zzzzzzzzzz", "zzzzz hi"]);
    }

    #[test]
    fn cuts_never_fall_inside_a_multibyte_char() {
        // Each char is multi-byte; a byte-indexed split would panic or corrupt.
        let body = "áéíóú".repeat(3); // 15 chars, 30 bytes
        let chunks = chunk_message(&body, 4);
        assert_within_limit(&chunks, 4);
        assert_eq!(chunks.concat(), body); // round-trips intact
        // Sanity: a 4-byte emoji is one char, so it packs four-per-chunk.
        let crabs = "🦀".repeat(9);
        let chunks = chunk_message(&crabs, 4);
        assert_within_limit(&chunks, 4);
        assert_eq!(chunks.concat(), crabs);
    }

    #[test]
    fn hard_split_is_exact_and_lossless() {
        assert_eq!(hard_split("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(hard_split("ab", 3), vec!["ab"]);
        assert!(hard_split("", 3).is_empty());
    }

    #[test]
    fn mixed_body_every_chunk_within_limit() {
        // Newlines, normal words, a giant token, and blank lines together.
        let body = format!(
            "intro line here\n\n{}\nshort tail\n{}",
            "tok ".repeat(200).trim(),
            "Q".repeat(3000)
        );
        let chunks = chunk_message(&body, 2000);
        assert_within_limit(&chunks, 2000);
        assert!(!chunks.is_empty());
    }
}
