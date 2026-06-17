mod client;
mod commands;
mod history;

use chaz_core::bridge::{ApprovalDecision, ApprovalExchange, Bridge};
use chaz_core::commands::{
    self as shared_commands, Command, CommandContext, CommandOutcome, Parsed,
};
use chaz_core::config::Config;
use chaz_core::hosted_index::DbEntry;
use chaz_core::security::SecretStore;
use chaz_core::server::Server;
use chaz_core::session::{Session, bind_transport, transport_bindings, unbind_transport};

use crate::credentials::MatrixCredentials;

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

pub struct MatrixBridge {
    /// The resolved Matrix credentials this bridge signs in as: homeserver,
    /// username, password, plus the per-login allow_list / room_size_limit
    /// filters. Read out of the bridge's own `BridgeDb` (opaque to chaz-core)
    /// before this bridge is built. One bridge runs per login.
    creds: MatrixCredentials,
    /// Resolved on-disk state directory for this login's matrix client
    /// (sync token, session). For explicit `logins:` entries this is
    /// `{base}/matrix/{login_id}` so logins never collide on disk; for the
    /// legacy synthesized login it is the historical location (verbatim
    /// `config.state_dir`, or the per-name default when unset) so existing
    /// installs keep their session.
    state_dir: Option<String>,
    /// Broader bot configuration (backends, limits) shared across all logins.
    /// The matrix *identity* comes from `creds`, not here.
    config: Config,
    secrets: SecretStore,
    /// Stable id of the login this bridge runs (`login.login_id`).
    /// Stamped into every inbound entry's `TransportRef::login_id`, used as the
    /// `login_id` dimension of every channel binding, and as this bridge's
    /// label in the agent's session registry (`exposed_on`). Since a login
    /// belongs to one agent, it doubles as that agent's transport identity.
    login_id: String,
    /// The agent that owns this login. The bridge attaches it to each of this
    /// login's sessions as the host (so the daemon's resolver routes to it),
    /// but never runs it — the daemon does. Resolved to a [`DbEntry`] against
    /// the (ticket-synced) agent index at message time.
    owning_agent: String,
    /// Cooperative shutdown signal. When the parent (typically `main` after
    /// the TUI exits) calls `notify_waiters`, the sync loop returns `Ok(())`
    /// instead of looping on the client sync.
    shutdown: Arc<Notify>,
}

impl MatrixBridge {
    pub fn new(
        creds: MatrixCredentials,
        login_id: String,
        owning_agent: String,
        state_dir: Option<String>,
        config: Config,
        secrets: SecretStore,
        shutdown: Arc<Notify>,
    ) -> anyhow::Result<Self> {
        if creds.homeserver_url.is_empty() {
            anyhow::bail!("homeserver_url is required for Matrix bridge");
        }
        if creds.username.is_empty() {
            anyhow::bail!("username is required for Matrix bridge");
        }
        Ok(Self {
            creds,
            state_dir,
            config,
            secrets,
            login_id,
            owning_agent,
            shutdown,
        })
    }
}

/// Resolve this login's owning agent to a [`DbEntry`] against the peer's agent
/// index. `None` when the agent isn't hosted/synced yet — callers skip the
/// session work rather than fork a session against a missing agent.
fn owning_agent_entry(server: &Server, owning_agent: &str) -> Option<DbEntry> {
    server.agent_index().find_by_name(owning_agent)
}

/// Install the reconciling response callback for a Matrix room.
///
/// Thin transport adapter over [`chaz_core::bridge::attach_reconciler`]: the
/// reconcile rule (delta scan, delivered-set, `[guest]` prefixing) lives in
/// the lib; this only supplies the Matrix `send` closure — render markdown and
/// hand it to `room.send`. This is the bridge's whole outbound path: the
/// daemon writes the agent's reply into the (synced) session DB, the callback
/// fires here, and the reply lands in the room.
async fn attach_response_callback(
    session_db: &eidetica::Database,
    room: Room,
    agents: Arc<chaz_core::agent::AgentRegistry>,
    owning_agent: String,
) -> anyhow::Result<()> {
    chaz_core::bridge::attach_reconciler(session_db, agents, owning_agent, move |body| {
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

    let Some(agent_entry) = owning_agent_entry(&server, owning_agent) else {
        anyhow::bail!("owning agent '{owning_agent}' is not hosted on this bridge yet");
    };
    let (_conv_id, session_db) = server
        .registry()
        .get_or_create_channel_session(&agent_entry, login_id, "matrix", login_id, &room_id)
        .await?;
    let session_db_id = session_db.root_id().to_string();
    let backend = get_backend(Some(&session_db), &config, &secrets).await;
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
        "Matrix-local: `attach <session>` · `detach` · `clear` · `send <msg>` · `rename` · `party`",
        "",
        "In a DM or when @mentioned, just talk — no prefix needed.",
    ]
    .join("\n")
}

/// `!chaz attach <session>` — bind this room to a specific session and install
/// the response callback so future writes reach the room. Bridge-local because
/// it touches the live `attached_sessions` set and matrix `Room`.
async fn handle_attach(
    arg: &str,
    room: Room,
    server: Arc<Server>,
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

    // Bind the room into the session DB, then expose the session on this bridge
    // and ensure the owning agent hosts it so the daemon picks it up.
    if let Err(e) = bind_transport(&target_db, "matrix", login_id, &room_id).await {
        let _ = room
            .send(RoomMessageEventContent::notice_plain(format!(
                "!chaz Error: failed to bind room: {e}"
            )))
            .await;
        return;
    }
    if let Some(agent_entry) = owning_agent_entry(&server, owning_agent) {
        let _ = server
            .registry()
            .ensure_session_host(&target_sid, &agent_entry)
            .await;
        if let Ok(Some(agent_db)) = server
            .registry()
            .open_agent_db(&agent_entry.db_id, None)
            .await
        {
            let _ = agent_db.expose_session_on(&target_sid, login_id).await;
        }
    }

    // Install the response callback on the newly-attached session so future
    // writes (the daemon's replies, scheduler fires) reach this room.
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
async fn handle_detach(room: Room, server: Arc<Server>, login_id: &str, owning_agent: &str) {
    let room_id = room.room_id().to_string();
    let Some(agent_entry) = owning_agent_entry(&server, owning_agent) else {
        let _ = room
            .send(RoomMessageEventContent::notice_plain(
                "!chaz Error: owning agent not available",
            ))
            .await;
        return;
    };
    match server
        .registry()
        .find_channel_session(&agent_entry, login_id, "matrix", login_id, &room_id)
        .await
    {
        Ok(Some((_cv, db))) => {
            let sid = db.root_id().to_string();
            let _ = unbind_transport(&db, "matrix", login_id, &room_id).await;
            if let Ok(Some(agent_db)) = server
                .registry()
                .open_agent_db(&agent_entry.db_id, None)
                .await
            {
                let _ = agent_db.unexpose_session_from(&sid, login_id).await;
            }
            let _ = room
                .send(RoomMessageEventContent::notice_plain(
                    "Room detached. Future messages will create a fresh session.",
                ))
                .await;
        }
        Ok(None) => {
            let _ = room
                .send(RoomMessageEventContent::notice_plain(
                    "This room isn't bound to a session.",
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

impl Bridge for MatrixBridge {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let creds = self.creds;
        let login_id = self.login_id.clone();
        let owning_agent = self.owning_agent.clone();
        let state_dir = self.state_dir;
        let secrets = self.secrets;
        let config = Arc::new(self.config);
        let shutdown = self.shutdown;

        let allow_list = creds
            .allow_list
            .clone()
            .or_else(|| config.allow_list.clone());
        let room_size_limit = creds.room_size_limit.or(config.room_size_limit);

        // --- Connect: login/restore, auto-join, prime the sync token ---
        let mut mc = MatrixClient::login(
            &Login {
                homeserver_url: creds.homeserver_url.clone(),
                username: creds.username.clone(),
                password: creds.password.clone(),
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

        // Track which session DBs have the Matrix response callback installed.
        // Keyed by session_db_id because a single session may be attached to
        // multiple rooms (fan-out delivery).
        let attached_sessions: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        // --- Startup: install response callbacks for every room this login
        //     already has a session bound to. This is what makes the daemon's
        //     replies (including scheduler fires) deliver when no user has
        //     recently spoken in the room. We discover them by walking the
        //     owning agent's session registry — the sessions exposed on this
        //     bridge — and reading each session's transport bindings. ---
        {
            let server = server.clone();
            let client = mc.client().clone();
            let attached_sessions = attached_sessions.clone();
            let login_id = login_id.clone();
            let owning_agent = owning_agent.clone();
            tokio::spawn(async move {
                let Some(agent_entry) = owning_agent_entry(&server, &owning_agent) else {
                    info!("Owning agent not hosted yet; skipping startup channel attach");
                    return;
                };
                let agent_db = match server
                    .registry()
                    .open_agent_db(&agent_entry.db_id, None)
                    .await
                {
                    Ok(Some(db)) => db,
                    _ => {
                        info!("Owning agent DB not openable yet; skipping startup attach");
                        return;
                    }
                };
                let refs = agent_db.list_session_refs().await.unwrap_or_default();
                for r in refs {
                    if !r.exposed_on.iter().any(|b| b == &login_id) {
                        continue;
                    }
                    let Ok((_c, db)) = server.registry().open_session(&r.session_db_id).await
                    else {
                        continue;
                    };
                    let bindings = transport_bindings(&db).await.unwrap_or_default();
                    for (transport, chan_login, room_id) in bindings {
                        if transport != "matrix" || chan_login != login_id {
                            continue;
                        }
                        attach_existing_channel(
                            &server,
                            &client,
                            &attached_sessions,
                            &owning_agent,
                            &room_id,
                            &r.session_db_id,
                        )
                        .await;
                    }
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
        // bot is addressed (DM or @mention). The bridge never runs an agent;
        // writing the inbound entry is enough — the daemon, watching the
        // exposed session, runs the agent and writes the reply back.
        {
            let server = server.clone();
            let config = config.clone();
            let secrets = secrets.clone();
            let login_id = login_id.clone();
            let owning_agent = owning_agent.clone();
            let allow_list = allow_list.clone();
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
                            // shared parser. These either need bridge state
                            // (response-callback install) or are Matrix-only.
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
                                "send" => {
                                    let _ = commands::send(
                                        event.sender.clone(),
                                        raw.to_string(),
                                        room.clone(),
                                        &config,
                                        &message_counts,
                                        &secrets,
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
                                        &login_id,
                                        &owning_agent,
                                        attached_sessions.clone(),
                                    )
                                    .await;
                                    return;
                                }
                                "detach" => {
                                    handle_detach(
                                        room.clone(),
                                        server.clone(),
                                        &login_id,
                                        &owning_agent,
                                    )
                                    .await;
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

                        // Resolve (or create) the session bound to this room.
                        // On create this binds the room into the session DB,
                        // attaches the owning agent as host (delegating session
                        // auth to the agent DB), and exposes the session on this
                        // bridge — so the daemon discovers and runs it.
                        let Some(agent_entry) = owning_agent_entry(&server, &owning_agent) else {
                            error!(
                                "Owning agent '{owning_agent}' not hosted; dropping message in {room_id}"
                            );
                            return;
                        };
                        let (_conv_id, session_db) = match server
                            .registry()
                            .get_or_create_channel_session(
                                &agent_entry,
                                &login_id,
                                "matrix",
                                &login_id,
                                &room_id,
                            )
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                error!("Failed to get session for {room_id}: {e}");
                                return;
                            }
                        };
                        let session_db_id = session_db.root_id().to_string();

                        // Install response callback if we haven't already, so
                        // the daemon's reply reaches this room.
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

                        // Write the user entry to the session DB. This is the
                        // bridge's whole inbound job — the daemon, watching the
                        // exposed session, runs the agent and writes the reply,
                        // which the response callback above delivers back here.
                        let mut session = Session::new(
                            chaz_core::types::ConversationId(session_db_id),
                            session_db,
                        )
                        .await;
                        // Stamp transport provenance so an agent's reply can
                        // be routed back to this room on this login.
                        session
                            .add_entry(chaz_core::bridge::inbound_user_entry(
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
        // shutdown signal so the parent can drain background bridges
        // without surfacing a spurious error.
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!("Matrix bridge received shutdown signal");
                    return Ok(());
                }
                res = mc.run_sync_loop() => match res {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        error!("Matrix sync error (retrying in 5s): {e}");
                        tokio::select! {
                            _ = shutdown.notified() => {
                                info!("Matrix bridge received shutdown signal during backoff");
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

/// Install the response-delivery callback for a persisted channel at startup.
/// Skips rooms the bot isn't joined to, or sessions that fail to open. The
/// bridge does not register the session for processing — the daemon does that
/// when it sees the session exposed in the agent registry — so this only wires
/// up outbound delivery.
async fn attach_existing_channel(
    server: &Arc<Server>,
    client: &matrix_sdk::Client,
    attached_sessions: &Arc<Mutex<HashSet<String>>>,
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
            room_id, "Matrix channel response callback installed at startup"
        );
    }
}

// ---------------------------------------------------------------------------
// Parked approval scaffolding — reused in Stage 4 (TODO-7755e097).
//
// The dumb bridge no longer runs the agent, so a tool-approval request no
// longer arrives over an in-process mpsc from a co-located runtime — the trigger
// (`register_session`'s approval channel) is gone. The render-the-prompt /
// capture-the-reaction half below is transport code that comes back near
// verbatim once approvals are proxied over the session DB (the daemon writes a
// request entry → the bridge renders it + captures the reaction → the bridge
// writes a decision entry → the daemon resolves the `ApprovalExchange`). Kept
// compiling behind `allow(dead_code)` rather than resurrected from git history;
// Stage 4 rewires it to the session-DB watcher.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
type PendingApprovals = Arc<Mutex<HashMap<OwnedEventId, oneshot::Sender<ApprovalDecision>>>>;

#[allow(dead_code)]
struct RoomApprovalRequest {
    room_id: String,
    exchange: ApprovalExchange,
}

/// Per-room forwarder: the runtime sends an [`ApprovalExchange`] here; this tags
/// it with the room and relays it to the bridge's approval task.
#[allow(dead_code)]
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

/// Background task: post each approval request to its room with seeded
/// ✅/❌/⏭ reactions and remember the message so a reaction resolves it.
/// Posting failure denies (safe default).
#[allow(dead_code)]
async fn run_approval_relay(
    mut rx: mpsc::Receiver<RoomApprovalRequest>,
    client: matrix_sdk::Client,
    pending: PendingApprovals,
) {
    while let Some(req) = rx.recv().await {
        let Ok(room_id_parsed) = matrix_sdk::ruma::RoomId::parse(&req.room_id) else {
            continue;
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
                pending
                    .lock()
                    .await
                    .insert(result.response.event_id, req.exchange.decision_tx);
            }
            Err(e) => {
                tracing::error!("Failed to send approval request: {e}");
                let _ = req.exchange.decision_tx.send(ApprovalDecision::Deny);
            }
        }
    }
}

/// Resolve an approval decision arriving as an emoji reaction on a prompt.
#[allow(dead_code)]
async fn handle_approval_reaction(
    event: OriginalSyncReactionEvent,
    room: Room,
    pending: PendingApprovals,
) {
    let relates_to = &event.content.relates_to;
    let decision = match relates_to.key.as_str() {
        "✅" => Some(ApprovalDecision::Approve),
        "❌" => Some(ApprovalDecision::Deny),
        "⏭" | "⏭️" => Some(ApprovalDecision::ApproveAll),
        _ => None,
    };
    if let Some(decision) = decision
        && let Some(tx) = pending.lock().await.remove(&relates_to.event_id)
    {
        info!(
            "Approval decision via reaction in {}: {:?}",
            room.room_id(),
            decision
        );
        let _ = tx.send(decision);
    }
}

/// Approve or deny the oldest pending tool-approval request in `room` — the
/// `!chaz approve` / `!chaz deny` text path.
#[allow(dead_code)]
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
