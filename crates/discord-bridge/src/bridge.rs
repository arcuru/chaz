//! The Discord bridge: a bidirectional translator between one Discord
//! channel and its eidetica session DB, written entirely against the
//! `chaz_core` bridge SDK.
//!
//! Inbound (`EventHandler::message`): gate the message, resolve (or create) the
//! channel→session binding through the registry — which binds the channel into
//! the session DB, attaches the owning agent as host, and exposes the session
//! on this bridge — then stamp an inbound entry via
//! [`chaz_core::bridge::inbound_user_entry`]. The bridge never runs the agent;
//! the daemon, watching the exposed session, does.
//!
//! Outbound: [`chaz_core::bridge::attach_reconciler`] installs an `on_write`
//! callback that converges the channel to DB state; the only Discord-specific
//! part is the `send` closure (`ChannelId::say`). The reconcile rule, the
//! delivered-set, and the `[guest]` prefixing all live in the lib — the same
//! code the Matrix bridge runs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chaz_core::backends::BackendManager;
use chaz_core::bridge::{ApprovalDecision, Bridge, attach_reconciler, inbound_user_entry};
use chaz_core::commands::{self, CommandContext, CommandOutcome, Parsed};
use chaz_core::config::Config;
use chaz_core::hosted_index::DbEntry;
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

use tokio::sync::Mutex;
use tracing::{error, info};

use crate::credentials::DiscordCredentials;

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
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILD_MESSAGE_REACTIONS
            | GatewayIntents::DIRECT_MESSAGE_REACTIONS;

        let handler = Handler {
            server,
            login_id: self.login_id.clone(),
            owning_agent: self.owning_agent.clone(),
            secrets: self.secrets.clone(),
            config: self.config.clone(),
            allowed_users: self.creds.allowed_users.clone(),
            attached: Arc::new(Mutex::new(HashSet::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            bot_id: Arc::new(Mutex::new(None)),
        };

        let mut client = Client::builder(&token, intents)
            .event_handler(handler)
            .await?;

        info!(
            login_id = %self.login_id,
            agent = %self.owning_agent,
            "Starting Discord bridge"
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
    /// Posted tool-approval prompts awaiting a reaction/command, keyed by the
    /// prompt message id.
    pending: PendingApprovals,
    /// This bot's user id, learned in `ready` — used to ignore its own seed
    /// reactions on approval prompts.
    bot_id: Arc<Mutex<Option<UserId>>>,
}

impl Handler {
    /// Resolve this login's owning agent to a [`DbEntry`]. `None` when the
    /// agent isn't hosted/synced yet.
    fn owning_agent_entry(&self) -> Option<DbEntry> {
        self.server.agent_index().find_by_name(&self.owning_agent)
    }

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

        // Resolve (or create) the session bound to this channel. On create this
        // binds the channel into the session DB, attaches the owning agent as
        // host (delegating session auth to the agent DB), and exposes the
        // session on this bridge — so the daemon discovers and runs it.
        let Some(agent_entry) = self.owning_agent_entry() else {
            error!(
                owning_agent = %self.owning_agent,
                "Owning agent not hosted; dropping Discord message"
            );
            return Ok(());
        };
        let (_conv_id, session_db) = self
            .server
            .registry()
            .get_or_create_channel_session(
                &agent_entry,
                &self.login_id,
                "discord",
                &self.login_id,
                &channel,
            )
            .await?;
        let session_db_id = session_db.root_id().to_string();

        // Install the reconciler once per session: outbound delivery is the
        // lib's job; we only supply the Discord send closure.
        self.attach_once(
            ctx.http.clone(),
            msg.channel_id,
            &session_db,
            &session_db_id,
        )
        .await?;

        // Stamp transport provenance so an agent's reply routes back here. This
        // is the bridge's whole inbound job — the daemon runs the agent.
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
        // Same session DB, two watchers: deliver agent replies, and surface
        // tool-approval prompts the daemon proxied here.
        attach_approval_watcher(session_db, http.clone(), channel_id, self.pending.clone()).await?;
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
    /// so the daemon's scheduled output is delivered with no inbound trigger.
    /// Discovered by walking the owning agent's session registry — the sessions
    /// exposed on this bridge — and reading each session's bindings. Mirrors
    /// the Matrix bridge's startup reattach.
    async fn reattach_existing_channels(&self, http: Arc<Http>) {
        let Some(agent_entry) = self.owning_agent_entry() else {
            info!("Owning agent not hosted yet; skipping startup reattach");
            return;
        };
        let agent_db = match self
            .server
            .registry()
            .open_agent_db(&agent_entry.db_id, None)
            .await
        {
            Ok(Some(db)) => db,
            _ => {
                info!("Owning agent DB not openable yet; skipping startup reattach");
                return;
            }
        };
        let refs = agent_db.list_session_refs().await.unwrap_or_default();
        for r in refs {
            if !r.exposed_on.iter().any(|b| b == &self.login_id) {
                continue;
            }
            let Ok((_c, db)) = self.server.registry().open_session(&r.session_db_id).await else {
                continue;
            };
            let bindings = chaz_core::session::transport_bindings(&db)
                .await
                .unwrap_or_default();
            for (transport, chan_login, channel) in bindings {
                if transport != "discord" || chan_login != self.login_id {
                    continue;
                }
                if let Err(e) = self
                    .reattach_channel(http.clone(), &channel, &r.session_db_id)
                    .await
                {
                    error!(channel = %channel, "Failed to reattach Discord channel: {e}");
                }
            }
        }
    }

    /// Install the response-delivery reconciler for an already-bound channel
    /// (no inbound entry, no agent run — the daemon runs the agent).
    async fn reattach_channel(
        &self,
        http: Arc<Http>,
        channel: &str,
        session_db_id: &str,
    ) -> anyhow::Result<()> {
        let (_conv_id, session_db) = self.server.registry().open_session(session_db_id).await?;
        let channel_id = ChannelId::new(channel.parse::<u64>()?);
        self.attach_once(http, channel_id, &session_db, session_db_id)
            .await
    }

    /// Handle a `!chaz <inner>` command. The view-local `help` verb is checked
    /// first; everything else normalizes to `/<inner>` and runs through the
    /// shared parser/dispatch — the same grammar Matrix and the TUI use.
    async fn handle_command(
        &self,
        ctx: &Context,
        msg: &Message,
        inner: &str,
    ) -> anyhow::Result<()> {
        let verb = inner.split_whitespace().next().unwrap_or("");
        if verb.is_empty() || verb == "help" {
            msg.channel_id.say(&ctx.http, help_text()).await?;
            return Ok(());
        }
        if verb == "approve" || verb == "deny" {
            resolve_pending_approval(
                &ctx.http,
                msg.channel_id,
                &self.pending,
                &msg.author.id.to_string(),
                verb == "approve",
            )
            .await;
            return Ok(());
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
        let Some(agent_entry) = self.owning_agent_entry() else {
            return Ok(CommandOutcome::Error(format!(
                "owning agent '{}' is not hosted yet",
                self.owning_agent
            )));
        };
        let (_conv_id, session_db) = self
            .server
            .registry()
            .get_or_create_channel_session(
                &agent_entry,
                &self.login_id,
                "discord",
                &self.login_id,
                &channel,
            )
            .await?;
        let session_db_id = session_db.root_id().to_string();
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
        "In a channel the bot watches, just talk — no prefix needed.",
    ]
    .join("\n")
}

// ---------------------------------------------------------------------------
// Tool-approval relay over the session DB.
//
// The dumb bridge runs no agent, so an approval request arrives as an
// `ApprovalRequest` entry the daemon's proxy writes into the (synced) session
// DB. The watcher posts each new request to its channel with seeded ✅/❌/⏭️
// reactions; a human's reaction (or `!chaz approve`/`deny`) writes an
// `ApprovalDecision` entry back, which syncs to the daemon and unblocks the
// runtime. The bridge holds no oneshot — the session DB is the whole channel.
// ---------------------------------------------------------------------------

/// What a posted approval prompt resolves: the session DB to write the decision
/// into, the request it answers, and the channel (so a `!chaz approve` command
/// resolves the right channel's oldest pending prompt).
struct PendingApproval {
    session_db: eidetica::Database,
    request_id: String,
    channel_id: ChannelId,
}
type PendingApprovals = Arc<Mutex<HashMap<MessageId, PendingApproval>>>;

/// Render the approval prompt a human reacts to.
fn approval_body(req: &chaz_core::bridge::ApprovalRequestPayload) -> String {
    format!(
        "🔒 **Tool approval required**\n\n\
         **Tool:** `{}`\n\
         **Risk:** {}\n\
         **Args:** `{}`\n\n\
         React: ✅ approve · ❌ deny · ⏭️ approve all\n\
         Or reply: `!chaz approve` / `!chaz deny`",
        req.tool_name, req.risk_level, req.arguments_display
    )
}

/// Write an `ApprovalDecision` entry resolving `request_id` into the session DB.
/// The decision syncs to the daemon, whose proxy matches it back and unblocks
/// the runtime's `request_approval`.
async fn write_approval_decision(
    session_db: &eidetica::Database,
    approver: &str,
    request_id: &str,
    decision: ApprovalDecision,
) {
    let sid = session_db.root_id().to_string();
    let mut session = Session::new(ConversationId(sid), session_db.clone()).await;
    session
        .add_entry(chaz_core::bridge::approval_decision_entry(
            approver, request_id, decision,
        ))
        .await;
}

/// Install a per-session watcher that posts each new `ApprovalRequest` entry to
/// `channel_id` (seeding the ✅/❌/⏭️ reactions) and records it in `pending` so
/// a reaction/command can resolve it.
///
/// Renders only requests with no decision yet and not already posted this
/// process (`seen`); a failed post is left unseen so the next session write
/// retries it. Never commits to the session tree (the decision is written later
/// from the reaction handler), so it is safe inside `on_write`.
async fn attach_approval_watcher(
    session_db: &eidetica::Database,
    http: Arc<Http>,
    channel_id: ChannelId,
    pending: PendingApprovals,
) -> anyhow::Result<()> {
    let sid = session_db.root_id().to_string();
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let db = session_db.clone();
    session_db
        .on_write(move |_event, _db| {
            let http = http.clone();
            let pending = pending.clone();
            let seen = seen.clone();
            let sid = sid.clone();
            let db = db.clone();
            Box::pin(async move {
                let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
                let mut seen = seen.lock().await;
                let requests =
                    chaz_core::bridge::unrendered_approval_requests(session.entries(), &seen);
                for req in requests {
                    seen.insert(req.request_id.clone());
                    match channel_id.say(&http, approval_body(&req)).await {
                        Ok(posted) => {
                            for e in ["✅", "❌", "⏭️"] {
                                let _ = posted
                                    .react(&http, ReactionType::Unicode(e.to_string()))
                                    .await;
                            }
                            pending.lock().await.insert(
                                posted.id,
                                PendingApproval {
                                    session_db: db.clone(),
                                    request_id: req.request_id,
                                    channel_id,
                                },
                            );
                        }
                        Err(e) => {
                            error!("Failed to post approval prompt: {e}");
                            seen.remove(&req.request_id); // retry on next write
                        }
                    }
                }
                Ok(())
            })
        })
        .await?
        .detach();
    Ok(())
}

/// Approve or deny the oldest pending tool-approval request for a channel — the
/// `!chaz approve` / `!chaz deny` text path.
async fn resolve_pending_approval(
    http: &Http,
    channel_id: ChannelId,
    pending: &PendingApprovals,
    approver: &str,
    approve: bool,
) {
    let resolved = {
        let mut p = pending.lock().await;
        let message_id = p
            .iter()
            .find(|(_, pa)| pa.channel_id == channel_id)
            .map(|(id, _)| *id);
        message_id.and_then(|id| p.remove(&id))
    };
    let Some(req) = resolved else {
        let _ = channel_id.say(http, "No pending approval requests").await;
        return;
    };
    let decision = if approve {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Deny
    };
    write_approval_decision(&req.session_db, approver, &req.request_id, decision).await;
    let label = if approve {
        "✅ Approved"
    } else {
        "❌ Denied"
    };
    let _ = channel_id.say(http, label).await;
}

/// Resolve an approval decision arriving as an emoji reaction on a prompt,
/// ignoring the bot's own seed reactions.
async fn handle_approval_reaction(
    add: Reaction,
    pending: &PendingApprovals,
    bot_id: Option<UserId>,
    allowed_users: &HashSet<u64>,
) {
    if add.user_id == bot_id {
        return;
    }
    // Same gate as the message path: when an allow-list is configured, only
    // those users may resolve an approval. Without this, any channel member
    // could ✅ a privileged tool call.
    let reactor = add.user_id.map(|u| u.get());
    if !allowed_users.is_empty() && !reactor.is_some_and(|u| allowed_users.contains(&u)) {
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
    let Some(req) = pending.lock().await.remove(&add.message_id) else {
        return;
    };
    let approver = add
        .user_id
        .map(|u| u.to_string())
        .unwrap_or_else(|| "discord-user".to_string());
    info!(channel = %add.channel_id, request_id = %req.request_id, ?decision, "Approval decision via reaction");
    write_approval_decision(&req.session_db, &approver, &req.request_id, decision).await;
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
        // Remember our own id so we ignore the seed reactions we post on
        // approval prompts.
        *self.bot_id.lock().await = Some(ready.user.id);
        // Reattach already-bound channels so the daemon's scheduled output
        // flows without an inbound message first.
        self.reattach_existing_channels(ctx.http.clone()).await;
    }

    /// ✅/❌/⏭️ on a posted approval prompt is the primary approve path. Look the
    /// reacted-to message up in `pending` and write the decision back into its
    /// session DB.
    async fn reaction_add(&self, _ctx: Context, add: Reaction) {
        let bot_id = *self.bot_id.lock().await;
        handle_approval_reaction(add, &self.pending, bot_id, &self.allowed_users).await;
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
