mod client;
mod commands;
mod history;

use chaz_core::commands::{
    self as shared_commands, Command, CommandContext, CommandOutcome, Parsed,
};
use chaz_core::config::{Config, MatrixLoginSpec};
use chaz_core::gateway::{ApprovalDecision, ApprovalExchange, Gateway};
use chaz_core::security::SecretStore;
use chaz_core::server::Server;
use chaz_core::session::Session;

use matrix_sdk::ruma::OwnedEventId;
use matrix_sdk::ruma::events::reaction::OriginalSyncReactionEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
};
use matrix_sdk::{Room, RoomState};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tracing::{error, info};

use client::{Login, MatrixClient, is_allowed};
use commands::{get_backend, rate_limit};
use history::read_room_history;

type PendingApprovals = Arc<Mutex<HashMap<OwnedEventId, oneshot::Sender<ApprovalDecision>>>>;

struct RoomApprovalRequest {
    room_id: String,
    exchange: ApprovalExchange,
}

fn make_room_approval_tx(
    room_id: String,
    relay_tx: mpsc::Sender<RoomApprovalRequest>,
) -> mpsc::Sender<ApprovalExchange> {
    let (tx, mut rx) = mpsc::channel::<ApprovalExchange>(8);
    tokio::spawn(async move {
        while let Some(exchange) = rx.recv().await {
            let _ = relay_tx
                .send(RoomApprovalRequest {
                    room_id: room_id.clone(),
                    exchange,
                })
                .await;
        }
    });
    tx
}

pub struct MatrixGateway {
    /// The Matrix login this gateway signs in as — credentials, per-login
    /// overrides, and the agent that owns it. One gateway runs per login;
    /// the spawn loop in `main` builds one of these per resolved login.
    login: MatrixLoginSpec,
    /// Resolved on-disk state directory for this login's matrix client
    /// (sync token, session). For explicit `logins:` entries this is
    /// `{base}/matrix/{login_id}` so logins never collide on disk; for the
    /// legacy synthesized login it is the historical location (verbatim
    /// `config.state_dir`, or the per-name default when unset) so existing
    /// installs keep their session.
    state_dir: Option<String>,
    /// Broader bot configuration (backends, agents, limits) shared across
    /// all logins. The matrix *identity* no longer comes from here — it
    /// comes from `login` — but everything else still does.
    config: Config,
    secrets: SecretStore,
    /// Stable id of the login this gateway runs (`login.login_id`).
    /// Stamped into every inbound entry's `TransportRef::login_id` and used
    /// as the `login_id` dimension of every channel binding. Since a login
    /// belongs to one agent, it doubles as that agent's transport identity.
    login_id: String,
    /// The agent that owns this login. The gateway attaches it to each of
    /// this login's rooms as the session host (`ensure_session_host`), so
    /// resolution routes to it by default. An explicit per-room re-host
    /// (`/agent host`) is preserved. The gateway itself does no per-message
    /// agent resolution.
    owning_agent: String,
    /// Cooperative shutdown signal. When the parent (typically `main` after
    /// the TUI exits) calls `notify_waiters`, the sync loop returns `Ok(())`
    /// instead of looping on the client sync.
    shutdown: Arc<Notify>,
}

impl MatrixGateway {
    pub fn new(
        login: MatrixLoginSpec,
        state_dir: Option<String>,
        config: Config,
        secrets: SecretStore,
        shutdown: Arc<Notify>,
    ) -> anyhow::Result<Self> {
        if login.homeserver_url.is_empty() {
            anyhow::bail!("homeserver_url is required for Matrix gateway");
        }
        if login.username.is_empty() {
            anyhow::bail!("username is required for Matrix gateway");
        }
        let login_id = login.login_id.clone();
        let owning_agent = login.owning_agent.clone();
        Ok(Self {
            login,
            state_dir,
            config,
            secrets,
            login_id,
            owning_agent,
            shutdown,
        })
    }
}

/// Install the reconciling response callback for a Matrix room.
///
/// Thin transport adapter over [`chaz_core::gateway::attach_reconciler`]: the
/// reconcile rule (delta scan, delivered-set, `[guest]` prefixing) lives in
/// the lib; this only supplies the Matrix `send` closure — render markdown and
/// hand it to `room.send`. Proving the lib API is sufficient for Matrix is
/// what keeps it honest as the surface a `chaz-discord` binary links against.
async fn attach_response_callback(
    session_db: &eidetica::Database,
    room: Room,
    agents: Arc<chaz_core::agent::AgentRegistry>,
    owning_agent: String,
) -> anyhow::Result<()> {
    chaz_core::gateway::attach_reconciler(session_db, agents, owning_agent, move |body| {
        let room = room.clone();
        async move {
            info!("→ Matrix({}): {}", room.room_id(), body.replace('\n', " "));
            let content = RoomMessageEventContent::text_markdown(&body);
            room.send(content).await?;
            Ok(())
        }
    })
    .await
}

/// Ensure this login's owning agent hosts the given session — a login
/// belongs to one agent, so that agent owns and hosts its rooms. The
/// gateway does no per-message agent resolution; it just attaches the owner
/// once at room setup and lets the runtime resolve from there. No-op when
/// the name doesn't resolve to a hosted agent (e.g. a legacy default that
/// isn't configured) — resolution then falls back to the global default.
async fn ensure_owning_agent_hosts(server: &Server, session_db_id: &str, owning_agent: &str) {
    let Some(entry) = server.agent_index().find_by_name(owning_agent) else {
        return;
    };
    if let Err(e) = server
        .registry()
        .ensure_session_host(session_db_id, &entry)
        .await
    {
        error!(
            owning_agent,
            session_db_id, "Failed to set owning agent as session host: {e}"
        );
    }
}

/// Dispatch a shared command in the context of a Matrix room.
async fn dispatch_in_room(
    cmd: Command,
    room: Room,
    server: Arc<Server>,
    config: Arc<Config>,
    secrets: SecretStore,
    login_id: &str,
    owning_agent: &str,
) -> anyhow::Result<()> {
    let room_id = room.room_id().to_string();

    let (_conv_id, session_db) = server
        .registry()
        .get_or_create_channel_session("matrix", login_id, &room_id)
        .await?;
    let session_db_id = session_db.root_id().to_string();
    // A login belongs to one agent: ensure it hosts this room's session, so
    // resolution picks it without the gateway choosing per message.
    ensure_owning_agent_hosts(&server, &session_db_id, owning_agent).await;
    let backend = get_backend(&room, &config, &secrets, server.registry(), login_id).await;
    let meta = chaz_core::session::read_meta_from_db(&session_db).await;
    let agent = server
        .registry()
        .resolve_agent(&session_db_id, None, server.agent_index())
        .await;
    let ctx = CommandContext {
        server: &server,
        secrets: &secrets,
        backend: &backend,
        session_db_id: &session_db_id,
        session_db: &session_db,
        current_agent: &agent.name,
        session_name: meta.name.as_deref(),
    };

    let outcome = shared_commands::dispatch(cmd, &ctx).await;
    render_outcome_to_room(&room, outcome).await;
    Ok(())
}

async fn render_outcome_to_room(room: &Room, outcome: CommandOutcome) {
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
            "!chaz To bind this room to a different session, use `!chaz attach <session>`."
                .to_string()
        }
        CommandOutcome::Quit => return,
    };

    if let Err(e) = room.send(RoomMessageEventContent::notice_plain(text)).await {
        tracing::error!("Failed to send command response: {e}");
    }
}

/// Help text for `!chaz help` — the shared `/`-vocabulary plus the Matrix-local
/// verbs, listed under the `!chaz ` prefix this transport uses.
fn help_text() -> String {
    [
        "**chaz commands** (prefix with `!chaz `):",
        "",
        "`sessions` · `info` · `print` · `compact` · `name [<alias>]` — session ops",
        "`agents` · `agent <add|remove|host|new|delete|share|import|set|reload|invite|rehost> …` — living agents",
        "`model [<id>|<agent> <id>]` · `role [<name> [prompt]]` · `backend <name> <url> <key>` · `backends` — LLM config",
        "`share` · `unshare` · `sync <ticket>` · `sharing <status|requests|approve|reject>` — sharing",
        "`extensions <list|add|remove|settings|set> …` — per-session extensions",
        "`channels` — rooms bound to this session",
        "",
        "Matrix-local: `attach <session>` · `detach` · `clear` · `approve` · `deny` · `send <msg>` · `rename` · `party`",
        "",
        "In a DM or when @mentioned, just talk — no prefix needed.",
    ]
    .join("\n")
}

/// Approve or deny the oldest pending tool-approval request in `room`.
async fn resolve_pending_approval(pending: &PendingApprovals, room: &Room, approve: bool) {
    let mut p = pending.lock().await;
    let Some(event_id) = p.keys().next().cloned() else {
        let _ = room
            .send(RoomMessageEventContent::notice_plain(
                "No pending approval requests",
            ))
            .await;
        return;
    };
    if let Some(tx) = p.remove(&event_id) {
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
        let _ = room
            .send(RoomMessageEventContent::notice_plain(label))
            .await;
    }
}

/// `!chaz attach <session>` — bind this room to a specific session and install
/// the response callback so future writes reach the room. Gateway-local because
/// it touches the live `attached_sessions` set and matrix `Room`.
#[allow(clippy::too_many_arguments)]
async fn handle_attach(
    arg: &str,
    room: Room,
    server: Arc<Server>,
    config: Arc<Config>,
    secrets: SecretStore,
    login_id: &str,
    owning_agent: &str,
    attached_sessions: Arc<Mutex<HashSet<String>>>,
) {
    let arg = arg.trim();
    if arg.is_empty() {
        let _ = room
            .send(RoomMessageEventContent::notice_plain(
                "Usage: !chaz attach <session-name-or-id>",
            ))
            .await;
        return;
    }
    let room_id = room.room_id().to_string();
    let (_cv, target_db) = match server.registry().resolve_session(arg).await {
        Ok(r) => r,
        Err(e) => {
            let _ = room
                .send(RoomMessageEventContent::notice_plain(format!(
                    "!chaz Error: unknown session '{arg}': {e}"
                )))
                .await;
            return;
        }
    };
    let target_sid = target_db.root_id().to_string();
    if let Err(e) = server
        .registry()
        .attach_channel("matrix", login_id, &room_id, &target_sid)
        .await
    {
        let _ = room
            .send(RoomMessageEventContent::notice_plain(format!(
                "!chaz Error: failed to attach: {e}"
            )))
            .await;
        return;
    }

    // Install the response callback on the newly-attached session so future
    // writes (including scheduler fires) reach this room.
    let backend = get_backend(&room, &config, &secrets, server.registry(), login_id).await;
    let agent_override = chaz_core::session::read_meta_from_db(&target_db)
        .await
        .agent_name;
    let _ = server
        .register_session(&target_db, backend, agent_override, None)
        .await;
    let mut attached = attached_sessions.lock().await;
    if attached.insert(target_sid.clone()) {
        drop(attached);
        if let Err(e) = attach_response_callback(
            &target_db,
            room.clone(),
            server.agents_arc(),
            owning_agent.to_string(),
        )
        .await
        {
            error!("Failed to attach response callback: {e}");
        }
    }

    let _ = room
        .send(RoomMessageEventContent::notice_plain(format!(
            "Attached this room to session {target_sid}."
        )))
        .await;
}

/// `!chaz detach` — unbind this room from its session.
async fn handle_detach(room: Room, server: Arc<Server>, login_id: &str) {
    let room_id = room.room_id().to_string();
    match server
        .registry()
        .detach_channel("matrix", login_id, &room_id)
        .await
    {
        Ok(()) => {
            let _ = room
                .send(RoomMessageEventContent::notice_plain(
                    "Room detached. Future messages will create a fresh session.",
                ))
                .await;
        }
        Err(e) => {
            let _ = room
                .send(RoomMessageEventContent::notice_plain(format!(
                    "!chaz Error: {e}"
                )))
                .await;
        }
    }
}

impl Gateway for MatrixGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let login = self.login;
        let login_id = self.login_id.clone();
        let owning_agent = self.owning_agent.clone();
        let state_dir = self.state_dir;
        let secrets = self.secrets;
        let config = Arc::new(self.config);
        let shutdown = self.shutdown;

        let allow_list = login
            .allow_list
            .clone()
            .or_else(|| config.allow_list.clone());
        let room_size_limit = login.room_size_limit.or(config.room_size_limit);

        // --- Connect: login/restore, auto-join, prime the sync token ---
        let mut mc = MatrixClient::login(
            &Login {
                homeserver_url: login.homeserver_url.clone(),
                username: login.username.clone(),
                password: login.password.clone(),
            },
            state_dir.as_deref(),
            "chaz",
        )
        .await?;

        mc.install_autojoin(allow_list.clone(), room_size_limit);

        // Initial sync primes the token *before* the message handlers are
        // installed, so room history is not replayed through them on startup.
        mc.initial_sync().await;

        info!("The client is ready! Listening to new messages…");

        // === Approval infrastructure ===
        let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (approval_relay_tx, mut approval_relay_rx) = mpsc::channel::<RoomApprovalRequest>(64);

        {
            let pending = pending_approvals.clone();
            let client = mc.client().clone();
            tokio::spawn(async move {
                while let Some(req) = approval_relay_rx.recv().await {
                    let room_id_parsed = match matrix_sdk::ruma::RoomId::parse(&req.room_id) {
                        Ok(id) => id,
                        Err(_) => continue,
                    };
                    let Some(room) = client.get_room(&room_id_parsed) else {
                        continue;
                    };

                    let info = &req.exchange.info;
                    let notice = format!(
                        "🔒 **Tool approval required**\n\n\
                         **Tool:** `{}`\n\
                         **Risk:** {:?}\n\
                         **Args:** `{}`\n\n\
                         React: ✅ approve · ❌ deny · ⏭ approve all\n\
                         Or reply: `!chaz approve` / `!chaz deny`",
                        info.name, info.risk_level, info.arguments_display
                    );
                    let content = RoomMessageEventContent::text_markdown(notice);
                    match room.send(content).await {
                        Ok(result) => {
                            let mut p = pending.lock().await;
                            p.insert(result.response.event_id, req.exchange.decision_tx);
                        }
                        Err(e) => {
                            tracing::error!("Failed to send approval request: {e}");
                            let _ = req.exchange.decision_tx.send(ApprovalDecision::Deny);
                        }
                    }
                }
            });
        }

        // Approval decisions via emoji reaction.
        {
            let pending = pending_approvals.clone();
            mc.client().add_event_handler(
                move |event: OriginalSyncReactionEvent, room: matrix_sdk::Room| {
                    let pending = pending.clone();
                    async move {
                        let relates_to = &event.content.relates_to;
                        let decision = match relates_to.key.as_str() {
                            "✅" => Some(ApprovalDecision::Approve),
                            "❌" => Some(ApprovalDecision::Deny),
                            "⏭" | "⏭️" => Some(ApprovalDecision::ApproveAll),
                            _ => None,
                        };
                        if let Some(decision) = decision {
                            let event_id = &relates_to.event_id;
                            let mut p = pending.lock().await;
                            if let Some(tx) = p.remove(event_id) {
                                info!(
                                    "Approval decision via reaction in {}: {:?}",
                                    room.room_id(),
                                    decision
                                );
                                let _ = tx.send(decision);
                            }
                        }
                    }
                },
            );
        }

        // Track which session DBs have the Matrix response callback installed.
        // Keyed by session_db_id because a single session may be attached to
        // multiple rooms (fan-out delivery).
        let attached_sessions: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        // --- Startup: attach response callbacks + server processing to every
        //     existing Matrix channel for which the bot is joined to the room.
        //     This is what makes scheduled-session responses actually deliver
        //     when no user has recently spoken in the room. ---
        {
            let server = server.clone();
            let client = mc.client().clone();
            let attached_sessions = attached_sessions.clone();
            let config = config.clone();
            let secrets = secrets.clone();
            let login_id = login_id.clone();
            let owning_agent = owning_agent.clone();
            tokio::spawn(async move {
                // Fold any legacy Matrix-only bindings into external_channels
                // under this login before we read them back.
                match server
                    .registry()
                    .migrate_legacy_matrix_channels(&login_id)
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => info!("Migrated {n} legacy matrix channel(s) to external_channels"),
                    Err(e) => error!("Legacy matrix channel migration failed: {e}"),
                }
                match server.registry().list_channels().await {
                    Ok(channels) => {
                        for (transport, chan_login, room_id, session_db_id) in channels {
                            if transport != "matrix" || chan_login != login_id {
                                continue;
                            }
                            attach_existing_channel(
                                &server,
                                &client,
                                &attached_sessions,
                                &config,
                                &secrets,
                                &login_id,
                                &owning_agent,
                                &room_id,
                                &session_db_id,
                            )
                            .await;
                        }
                    }
                    Err(e) => error!("Failed to list channels at startup: {e}"),
                }
            });
        }

        // === Unified message handler ===
        //
        // One handler replaces headjack's ~20 per-command registrations plus the
        // free-text handler. A `!chaz ` prefix is the command channel: Matrix-
        // local verbs are checked first, then the line is normalized to the
        // shared `/`-grammar and routed through `chaz_core::commands::parse`.
        // Everything else is a plain message — written to the session when the
        // bot is addressed (DM or @mention).
        {
            let server = server.clone();
            let config = config.clone();
            let secrets = secrets.clone();
            let login_id = login_id.clone();
            let owning_agent = owning_agent.clone();
            let allow_list = allow_list.clone();
            let approval_relay_tx = approval_relay_tx.clone();
            let pending_approvals = pending_approvals.clone();
            let attached_sessions = attached_sessions.clone();
            let message_counts: Arc<Mutex<HashMap<String, u64>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let backfilled_rooms: Arc<Mutex<HashSet<String>>> =
                Arc::new(Mutex::new(HashSet::new()));
            let seen_events: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

            mc.client().add_event_handler(
                move |event: OriginalSyncRoomMessageEvent, room: Room| {
                    let server = server.clone();
                    let config = config.clone();
                    let secrets = secrets.clone();
                    let login_id = login_id.clone();
                    let owning_agent = owning_agent.clone();
                    let allow_list = allow_list.clone();
                    let approval_relay_tx = approval_relay_tx.clone();
                    let pending_approvals = pending_approvals.clone();
                    let attached_sessions = attached_sessions.clone();
                    let message_counts = message_counts.clone();
                    let backfilled_rooms = backfilled_rooms.clone();
                    let seen_events = seen_events.clone();
                    async move {
                        if room.state() != RoomState::Joined {
                            return;
                        }
                        let MessageType::Text(text_content) = &event.content.msgtype else {
                            return;
                        };
                        // Dedupe: the sync loop can redeliver on reconnect.
                        {
                            let mut seen = seen_events.lock().await;
                            if !seen.insert(event.event_id.to_string()) {
                                return;
                            }
                        }
                        let Some(bot_uid) = room.client().user_id().map(|u| u.to_string()) else {
                            return;
                        };
                        if !is_allowed(allow_list.as_deref(), event.sender.as_str(), &bot_uid) {
                            return;
                        }

                        let raw = text_content.body.trim_start();

                        // Command channel: `!chaz` optionally followed by args.
                        // A word boundary is required so `!chazfoo` is treated
                        // as a plain message, not the command `foo`.
                        let command_inner = raw.strip_prefix("!chaz").and_then(|r| {
                            (r.is_empty() || r.starts_with(char::is_whitespace))
                                .then(|| r.trim().to_string())
                        });

                        if let Some(inner) = command_inner {
                            let verb = inner.split_whitespace().next().unwrap_or("");
                            // View-local Matrix verbs, checked before the
                            // shared parser. These either need gateway state
                            // (approvals, response-callback install) or are
                            // Matrix-only.
                            match verb {
                                "" | "help" => {
                                    let _ = room
                                        .send(RoomMessageEventContent::text_markdown(help_text()))
                                        .await;
                                    return;
                                }
                                "party" => {
                                    let _ = room
                                        .send(RoomMessageEventContent::notice_plain(
                                            ".🎉🎊🥳 let's PARTY!! 🥳🎊🎉",
                                        ))
                                        .await;
                                    return;
                                }
                                "clear" => {
                                    let _ = room
                                        .send(RoomMessageEventContent::notice_plain(
                                            "!chaz clear: All messages before this will be ignored",
                                        ))
                                        .await;
                                    return;
                                }
                                "approve" => {
                                    resolve_pending_approval(&pending_approvals, &room, true).await;
                                    return;
                                }
                                "deny" => {
                                    resolve_pending_approval(&pending_approvals, &room, false)
                                        .await;
                                    return;
                                }
                                "send" => {
                                    let _ = commands::send(
                                        event.sender.clone(),
                                        raw.to_string(),
                                        room.clone(),
                                        &config,
                                        &message_counts,
                                        &secrets,
                                        server.registry(),
                                        &login_id,
                                    )
                                    .await;
                                    return;
                                }
                                "rename" => {
                                    let _ = commands::rename(
                                        event.sender.clone(),
                                        raw.to_string(),
                                        room.clone(),
                                        &config,
                                        &message_counts,
                                        &secrets,
                                        server.registry(),
                                        &login_id,
                                    )
                                    .await;
                                    return;
                                }
                                "attach" => {
                                    let arg = inner.strip_prefix("attach").unwrap_or("").trim();
                                    handle_attach(
                                        arg,
                                        room.clone(),
                                        server.clone(),
                                        config.clone(),
                                        secrets.clone(),
                                        &login_id,
                                        &owning_agent,
                                        attached_sessions.clone(),
                                    )
                                    .await;
                                    return;
                                }
                                "detach" => {
                                    handle_detach(room.clone(), server.clone(), &login_id).await;
                                    return;
                                }
                                _ => {}
                            }

                            // Shared vocabulary: `!chaz <rest>` → `/<rest>`.
                            // `list` is a Matrix-only alias for `backends`
                            // (the shared grammar has no `/list`).
                            let slash = if verb == "list" {
                                "/backends".to_string()
                            } else {
                                format!("/{inner}")
                            };
                            match shared_commands::parse(&slash) {
                                Parsed::Command(cmd) => {
                                    if let Err(e) = dispatch_in_room(
                                        cmd,
                                        room.clone(),
                                        server.clone(),
                                        config.clone(),
                                        secrets.clone(),
                                        &login_id,
                                        &owning_agent,
                                    )
                                    .await
                                    {
                                        error!("Command dispatch failed: {e}");
                                    }
                                }
                                Parsed::Usage(msg) => {
                                    let _ =
                                        room.send(RoomMessageEventContent::notice_plain(msg)).await;
                                }
                                // Unreachable: the input always has a leading `/`.
                                Parsed::NotCommand => {}
                            }
                            return;
                        }

                        // Plain message: only engage when addressed.
                        let is_direct = room.is_direct().await.unwrap_or(false)
                            || room.joined_members_count() < 3;
                        let mentions_bot = event
                            .content
                            .mentions
                            .as_ref()
                            .map(|mentions| {
                                mentions
                                    .user_ids
                                    .iter()
                                    .any(|mention| mention == room.client().user_id().unwrap())
                            })
                            .unwrap_or(false);
                        if !(is_direct || mentions_bot) {
                            return;
                        }

                        if rate_limit(&room, &event.sender, &config, &message_counts).await {
                            return;
                        }

                        let room_id = room.room_id().to_string();
                        let backend =
                            get_backend(&room, &config, &secrets, server.registry(), &login_id)
                                .await;

                        let (_conv_id, session_db) = match server
                            .registry()
                            .get_or_create_channel_session("matrix", &login_id, &room_id)
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                error!("Failed to get session for {room_id}: {e}");
                                return;
                            }
                        };
                        let session_db_id = session_db.root_id().to_string();

                        // A login belongs to one agent: ensure it hosts this
                        // room's session (idempotent — only writes on first
                        // contact). Resolution then picks the owner via the host
                        // slot; an explicit per-room re-host still wins.
                        ensure_owning_agent_hosts(&server, &session_db_id, &owning_agent).await;
                        let agent_override = chaz_core::session::read_meta_from_db(&session_db)
                            .await
                            .agent_name;

                        let approval_tx =
                            make_room_approval_tx(room_id.clone(), approval_relay_tx.clone());

                        if let Err(e) = server
                            .register_session(
                                &session_db,
                                backend,
                                agent_override,
                                Some(approval_tx),
                            )
                            .await
                        {
                            error!("Failed to register session: {e}");
                            return;
                        }

                        // Install response callback if we haven't already.
                        {
                            let mut attached = attached_sessions.lock().await;
                            if attached.insert(session_db_id.clone()) {
                                drop(attached);
                                if let Err(e) = attach_response_callback(
                                    &session_db,
                                    room.clone(),
                                    server.agents_arc(),
                                    owning_agent.clone(),
                                )
                                .await
                                {
                                    error!("Failed to register response callback: {e}");
                                } else {
                                    info!(
                                        session_db_id = %session_db_id,
                                        room_id = %room_id,
                                        "Matrix response callback installed"
                                    );
                                }
                            }
                        }

                        // Backfill room history on first message per room.
                        {
                            let mut backfilled = backfilled_rooms.lock().await;
                            if backfilled.insert(room_id.clone()) {
                                info!("Backfilling history for room {room_id}");
                                let history = read_room_history(&room).await;
                                let mut session = Session::new(
                                    chaz_core::types::ConversationId(session_db_id.clone()),
                                    session_db.clone(),
                                )
                                .await;
                                session.backfill(history).await;
                            }
                        }

                        // Write user entry to session DB — triggers server →
                        // agent → response.
                        let mut session = Session::new(
                            chaz_core::types::ConversationId(session_db_id),
                            session_db,
                        )
                        .await;
                        // Stamp transport provenance so an agent's reply can
                        // be routed back to this room on this login.
                        session
                            .add_entry(chaz_core::gateway::inbound_user_entry(
                                "matrix",
                                &login_id,
                                room.room_id().as_ref(),
                                event.sender.as_ref(),
                                None,
                                raw,
                                Some(event.event_id.to_string()),
                            ))
                            .await;
                    }
                },
            );
        }

        // Retry loop for transient sync errors. Returns Ok(()) on a clean
        // shutdown signal so the parent can drain background gateways
        // without surfacing a spurious error.
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!("Matrix gateway received shutdown signal");
                    return Ok(());
                }
                res = mc.run_sync_loop() => match res {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        error!("Matrix sync error (retrying in 5s): {e}");
                        tokio::select! {
                            _ = shutdown.notified() => {
                                info!("Matrix gateway received shutdown signal during backoff");
                                return Ok(());
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        }
                    }
                }
            }
        }
    }
}

/// Install server processing + response-delivery for a persisted channel at
/// startup. Skips rooms the bot isn't joined to, or sessions that fail to open.
///
/// Without an active user in the room, we pass no approval channel — scheduled
/// Directives fire autonomously. When the user next speaks, the message handler
/// re-registers the session with an approval channel bound to that message.
#[allow(clippy::too_many_arguments)]
async fn attach_existing_channel(
    server: &Arc<Server>,
    client: &matrix_sdk::Client,
    attached_sessions: &Arc<Mutex<HashSet<String>>>,
    config: &Arc<Config>,
    secrets: &SecretStore,
    login_id: &str,
    owning_agent: &str,
    room_id: &str,
    session_db_id: &str,
) {
    let Ok(room_id_parsed) = matrix_sdk::ruma::RoomId::parse(room_id) else {
        return;
    };
    let Some(room) = client.get_room(&room_id_parsed) else {
        tracing::debug!(room_id, "Not joined to room; skipping channel attach");
        return;
    };

    let Ok((_conv_id, session_db)) = server.registry().open_session(session_db_id).await else {
        tracing::warn!(session_db_id, "Stale matrix channel — session not openable");
        return;
    };

    ensure_owning_agent_hosts(server, session_db_id, owning_agent).await;
    let agent_override = chaz_core::session::read_meta_from_db(&session_db)
        .await
        .agent_name;
    let backend = get_backend(&room, config, secrets, server.registry(), login_id).await;
    if let Err(e) = server
        .register_session(&session_db, backend, agent_override, None)
        .await
    {
        error!(session_db_id, "Failed to register session at startup: {e}");
        return;
    }

    {
        let mut attached = attached_sessions.lock().await;
        if !attached.insert(session_db_id.to_string()) {
            return;
        }
    }

    if let Err(e) = attach_response_callback(
        &session_db,
        room,
        server.agents_arc(),
        owning_agent.to_string(),
    )
    .await
    {
        error!("Failed to attach response callback at startup: {e}");
    } else {
        info!(
            session_db_id,
            room_id, "Matrix channel attached at startup (server + response callbacks installed)"
        );
    }
}
