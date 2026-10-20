pub(crate) mod client;
mod commands;
mod history;
mod threads;

use chaz_core::bridge::delivery::{DeliveryBinding, DeliveryReconciler, DeliveryStore};
use chaz_core::bridge::outbound::{
    chunk_message_bytes, is_transient_outbound_status, retry_outbound_chunk,
};
use chaz_core::bridge::{ApprovalDecision, Bridge};
use chaz_core::bridge_db::BridgeDb;
use chaz_core::commands::{
    self as shared_commands, Command, CommandContext, CommandOutcome, Parsed,
};
use chaz_core::config::Config;
use chaz_core::hosted_index::DbEntry;
use chaz_core::security::SecretStore;
use chaz_core::server::Server;
use chaz_core::session::{
    MatrixParticipation, Session, ambient_participation_receipt, read_meta_from_db,
    transport_bindings, unbind_transport, update_meta_on_db,
};

use crate::credentials::MatrixCredentials;

use matrix_sdk::ruma::api::client::state::get_state_events::v3::Request as RoomStateRequest;
use matrix_sdk::ruma::api::client::typing::create_typing_event::v3::{
    Request as TypingRequest, Typing, TypingInfo,
};
use matrix_sdk::ruma::events::reaction::OriginalSyncReactionEvent;
use matrix_sdk::ruma::events::room::encrypted::{
    EncryptedEventScheme, OriginalSyncRoomEncryptedEvent,
};
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::member::OriginalSyncRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
};
use matrix_sdk::ruma::{OwnedEventId, OwnedTransactionId};
use matrix_sdk::{Room, RoomState};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use client::is_allowed;
pub(crate) use client::{Login, MatrixClient};
use commands::{get_backend, rate_limit};
use history::read_room_history;
use threads::{
    RoomMode, RoomTarget, continued_thread_root, needs_managed_lookup, opened_thread_root,
    parse_thread_channel, read_room_mode, thread_channel, write_room_mode,
};

pub struct MatrixBridge {
    /// The resolved Matrix credentials this bridge signs in as: homeserver,
    /// username, password, plus the per-login allow_list / room_size_limit
    /// filters. Read out of the bridge's own `BridgeDb` (opaque to chaz-core)
    /// before this bridge is built. One bridge runs per login.
    creds: MatrixCredentials,
    /// Resolved on-disk state directory for this login's matrix client
    /// (sync token, session): the per-login directory under
    /// `{base}/matrix/` that [`crate::login_state`] resolved for this login,
    /// so logins never collide on disk.
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
    /// This bridge's own settings DB. Durable operator state that is neither a
    /// credential nor part of any session's LLM context lives here — currently
    /// the per-`(login, room)` [`RoomMode`].
    bridge_db: BridgeDb,
    /// Cooperative shutdown signal. When the parent (typically `main` after
    /// the TUI exits) calls `notify_waiters`, the sync loop returns `Ok(())`
    /// instead of looping on the client sync.
    shutdown: Arc<Notify>,
}

impl MatrixBridge {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        creds: MatrixCredentials,
        login_id: String,
        owning_agent: String,
        state_dir: Option<String>,
        config: Config,
        secrets: SecretStore,
        bridge_db: BridgeDb,
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
            bridge_db,
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

/// Whether `root` names a Matrix thread this bridge already manages a session
/// for — i.e. one that has a durable `(login, room, root event)` binding.
///
/// The durable binding, not in-process bookkeeping, is the source of truth: a
/// thread opened before a restart is still managed after it, and a thread in a
/// room whose mode has since been set back to `normal` stays reachable instead
/// of being stranded.
async fn is_managed_thread(
    server: &Arc<Server>,
    owning_agent: &str,
    login_id: &str,
    room: &Room,
    root: &matrix_sdk::ruma::EventId,
) -> anyhow::Result<bool> {
    let agent_entry = owning_agent_entry(server, owning_agent)
        .ok_or_else(|| anyhow::anyhow!("owning agent is not hosted yet"))?;
    let channel = thread_channel(room.room_id().as_str(), root);
    Ok(server
        .registry()
        .find_channel_session(&agent_entry, login_id, "matrix", login_id, &channel)
        .await?
        .is_some())
}

struct BackfilledRoom {
    join_id: String,
    prejoin_ids: HashSet<String>,
}

/// Bootstrap a joined room before any live event is written. The per-room lock
/// serializes join handling with concurrent inbound sync events. Retry after a
/// failed fetch; never mark a partial import complete.
async fn backfill_joined_room(
    room: &Room,
    server: &Server,
    bridge_db: &BridgeDb,
    login_id: &str,
    owning_agent: &str,
    backfilled: &Mutex<HashMap<String, BackfilledRoom>>,
) -> anyhow::Result<()> {
    if room.state() != RoomState::Joined {
        return Ok(());
    }
    let room_id = room.room_id().to_string();
    let mut done = backfilled.lock().await;
    let agent = owning_agent_entry(server, owning_agent)
        .ok_or_else(|| anyhow::anyhow!("owning agent {owning_agent} not hosted"))?;
    let client = room.client();
    let user_id = client
        .user_id()
        .ok_or_else(|| anyhow::anyhow!("not logged in"))?;
    // The SDK's local state cache can lack our own member event after a
    // restored login. Read current state from the homeserver instead.
    let state = client
        .send(RoomStateRequest::new(room.room_id().to_owned()))
        .await?;
    let join_id = state
        .room_state
        .iter()
        .find(|raw| {
            raw.get_field::<String>("state_key")
                .ok()
                .flatten()
                .as_deref()
                == Some(user_id.as_str())
                && raw.get_field::<String>("type").ok().flatten().as_deref()
                    == Some("m.room.member")
        })
        .ok_or_else(|| anyhow::anyhow!("own membership state missing"))?;
    anyhow::ensure!(
        join_id
            .get_field::<serde_json::Value>("content")?
            .and_then(|content| content.get("membership")?.as_str().map(str::to_owned))
            .as_deref()
            == Some("join"),
        "own membership state is not a joined event"
    );
    let join_id = join_id
        .get_field::<String>("event_id")?
        .ok_or_else(|| anyhow::anyhow!("join event has no ID"))?;
    if done
        .get(&room_id)
        .is_some_and(|value| value.join_id == join_id)
    {
        return Ok(());
    }
    // A `threaded-sessions` room deliberately has no room-wide session: each
    // thread's session starts from its own root. Keep the join cutoff (delayed
    // pre-join events must not open threads or run commands), but do not
    // create or import into a room-wide session the mode detached.
    if read_room_mode(bridge_db, login_id, &room_id).await? == RoomMode::ThreadedSessions {
        let (_history, prejoin_ids) =
            read_room_history(room, &join_id, None, user_id.as_str(), login_id).await?;
        done.insert(
            room_id,
            BackfilledRoom {
                join_id,
                prejoin_ids,
            },
        );
        return Ok(());
    }
    // The session is published with no newly imported entries. A transaction
    // below makes the history, consumed baseline and join checkpoint visible
    // together, even if this is a rejoin after a previous run.
    let (conv, db) = server
        .registry()
        .get_or_create_conversational_channel_session(
            &agent, login_id, "matrix", login_id, &room_id,
        )
        .await?;
    let mut session = Session::new(conv, db).await;
    let previous = session.last_backfilled_join(&room_id).await?;
    // Reconstruct the cutoff even after restart: the checkpoint alone cannot
    // distinguish a delayed pre-join command or a message before !chaz clear.
    let already_imported = previous.as_deref() == Some(join_id.as_str());
    let (history, prejoin_ids) = read_room_history(
        room,
        &join_id,
        if already_imported {
            None
        } else {
            previous.as_deref()
        },
        user_id.as_str(),
        login_id,
    )
    .await?;
    if !already_imported {
        session.backfill(history, (&room_id, &join_id)).await?;
    }
    done.insert(
        room_id,
        BackfilledRoom {
            join_id,
            prejoin_ids,
        },
    );
    Ok(())
}

/// Channels this login has outbound delivery installed for, keyed by
/// `(channel, session_db_id)` — the binding delivery progress is persisted
/// under. The channel is the bare room ID for a room-wide session, or the
/// composite room-plus-thread-root key for a thread session.
type AttachedChannels = Arc<Mutex<HashMap<(String, String), Arc<DeliveryReconciler>>>>;

/// Install durable outbound delivery and the approval watcher for a Matrix
/// delivery target.
///
/// Thin transport adapter over [`chaz_core::bridge::delivery`]: the
/// reconcile rule, persisted progress, retry schedule, and `[guest]`
/// prefixing live in the lib; this only supplies the Matrix `send` closure —
/// render markdown and hand it to the target under the chunk's stable
/// transaction ID, which lets the homeserver collapse a chunk repeated after
/// a lost acknowledgement. This is the bridge's whole outbound path: the
/// executor writes the agent's reply into the session DB, the callback fires
/// here, and the reply lands on the target — the room's main timeline for a
/// room-wide session, or inside its own thread for a thread session.
async fn attach_response_callback(
    session_db: &eidetica::Database,
    target: RoomTarget,
    server: &Server,
    login_id: &str,
    owning_agent: String,
    pending: PendingApprovals,
) -> anyhow::Result<Arc<DeliveryReconciler>> {
    // Matrix typing notices are room-scoped, so a thread's turn shows in the
    // room it belongs to.
    attach_typing_watcher(session_db, target.room().clone()).await?;
    // Same session DB, two watchers: deliver agent replies, and surface tool
    // approval prompts the daemon proxied here.
    attach_approval_watcher(session_db, target.clone(), login_id.to_string(), pending).await?;
    let binding = DeliveryBinding::new(
        "matrix",
        login_id,
        target.channel(),
        session_db.root_id().to_string(),
    );
    DeliveryReconciler::attach(
        session_db.clone(),
        DeliveryStore::new(server.registry().chaz_peer().clone()),
        binding,
        server.agents_arc(),
        owning_agent,
        |body| chunk_message_bytes(body, MATRIX_MESSAGE_LIMIT),
        move |chunk| {
            let target = target.clone();
            Box::pin(async move {
                info!(
                    room_id = %target.room().room_id(),
                    thread_root = ?target.thread_root().map(|id| id.to_string()),
                    body_bytes = chunk.body.len(),
                    "Delivering Matrix response"
                );
                let content = RoomMessageEventContent::text_markdown(&chunk.body);
                // Stable across in-call retries, durable retries, and
                // restarts, so the homeserver deduplicates every repeat of
                // this chunk it still remembers.
                let txn_id = OwnedTransactionId::from(chunk.idempotency_key);
                retry_outbound_chunk(
                    || {
                        let target = target.clone();
                        let content = content.clone();
                        let txn_id = txn_id.clone();
                        async move {
                            target
                                .send_with_transaction_id(content, &txn_id)
                                .await
                                .map(|_| ())
                        }
                    },
                    is_transient_matrix_send_error,
                )
                .await?;
                Ok(())
            })
        },
    )
    .await
}

// Give the server ample time beyond the renewal interval; the SDK typing_notice
// helper uses a four-second timeout and suppresses sends within three seconds.
const TYPING_LEASE: std::time::Duration = std::time::Duration::from_secs(10);
const TYPING_RENEWAL: std::time::Duration = std::time::Duration::from_secs(3);

fn turn_typing(active: bool) -> Typing {
    if active {
        Typing::Yes(TypingInfo::new(TYPING_LEASE))
    } else {
        Typing::No
    }
}

async fn send_turn_typing(room: &Room, active: bool) -> matrix_sdk::Result<()> {
    room.client()
        .send(TypingRequest::new(
            room.own_user_id().to_owned(),
            room.room_id().to_owned(),
            turn_typing(active),
        ))
        .await
        .map(|_| ())
        .map_err(Into::into)
}

/// Observe the executor's per-turn claim through session sync. Timer reads
/// both renew Matrix's short typing timeout and expire orphaned starts if a
/// remote executor dies without a final write.
async fn update_typing<E, F, Fut>(typing: &mut bool, active: bool, send: F) -> Result<(), E>
where
    F: FnOnce(bool) -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    // True is renewed on every timer pass; false is sent once on release.
    if active || *typing {
        send(active).await?;
        *typing = active;
    }
    Ok(())
}

async fn attach_typing_watcher(session_db: &eidetica::Database, room: Room) -> anyhow::Result<()> {
    let db = session_db.clone();
    let changed = Arc::new(Notify::new());
    let wake = changed.clone();
    session_db
        .on_write(move |_, _| {
            let wake = wake.clone();
            Box::pin(async move {
                wake.notify_one();
                Ok(())
            })
        })
        .await?
        .detach();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TYPING_RENEWAL);
        let mut typing = false;
        loop {
            tokio::select! {
                _ = tick.tick() => {},
                _ = changed.notified() => {},
            }
            if room.state() != RoomState::Joined {
                break;
            }
            let active = match Session::active_turn_attempts(&db).await {
                Ok(attempts) => !attempts.is_empty(),
                Err(error) => {
                    error!(%error, "Could not read Matrix turn activity");
                    continue;
                }
            };
            if let Err(error) =
                update_typing(&mut typing, active, |state| send_turn_typing(&room, state)).await
            {
                error!(%error, "Could not update Matrix typing notice");
            }
        }
        if typing {
            let _ = send_turn_typing(&room, false).await;
        }
    });
    Ok(())
}

fn is_transient_matrix_send_error(error: &matrix_sdk::Error) -> bool {
    match error {
        matrix_sdk::Error::Http(http) => match http.as_ref() {
            matrix_sdk::HttpError::Reqwest(error) => error.is_connect() || error.is_timeout(),
            matrix_sdk::HttpError::Api(_) => http
                .as_client_api_error()
                .is_some_and(|error| is_transient_outbound_status(error.status_code.as_u16())),
            _ => false,
        },
        _ => false,
    }
}

/// Matrix homeservers limit event content, so outbound reply bodies must be
/// delivered as separate events. Markdown carries both plain and formatted
/// bodies; this byte ceiling leaves room for both fields and the event envelope.
const MATRIX_MESSAGE_LIMIT: usize = 8_000;

/// Dispatch a shared command against the session that owns a Matrix target —
/// the room-wide session on the main timeline, or a thread's own session when
/// the command was sent inside a managed thread. Output goes back to the same
/// target, so a thread-local `!chaz info` answers in its thread.
async fn dispatch_in_target(
    cmd: Command,
    target: RoomTarget,
    server: Arc<Server>,
    config: Arc<Config>,
    secrets: SecretStore,
    login_id: &str,
    owning_agent: &str,
) -> anyhow::Result<()> {
    let channel = target.channel();

    let Some(agent_entry) = owning_agent_entry(&server, owning_agent) else {
        anyhow::bail!("owning agent '{owning_agent}' is not hosted on this bridge yet");
    };
    let (_conv_id, session_db) = server
        .registry()
        .get_or_create_conversational_channel_session(
            &agent_entry,
            login_id,
            "matrix",
            login_id,
            &channel,
        )
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
    render_outcome_to_target(&target, outcome).await;
    Ok(())
}

async fn render_outcome_to_target(target: &RoomTarget, outcome: CommandOutcome) {
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
                    s.push_str(&format!("\n  {}{} ({})", info.session_db_id, name, agent));
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

    if let Err(e) = target
        .send(RoomMessageEventContent::notice_plain(text))
        .await
    {
        tracing::error!("Failed to send command response: {e}");
    }
}

/// Post a plain notice to a target, discarding the event ID. The bridge's
/// one-line answers (usage, refusals, mode reports) all go out this way.
async fn notice(target: &RoomTarget, text: impl Into<String>) {
    if let Err(e) = target
        .send(RoomMessageEventContent::notice_plain(text.into()))
        .await
    {
        tracing::error!("Failed to send notice: {e}");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoomKind {
    Direct,
    Group,
    Unknown,
}

fn classify_room(is_direct: Option<bool>, members: Result<usize, ()>) -> RoomKind {
    match (is_direct, members) {
        (Some(true), _) | (Some(false), Ok(0..=2)) => RoomKind::Direct,
        (Some(false), Ok(_)) => RoomKind::Group,
        _ => RoomKind::Unknown,
    }
}

async fn room_kind(room: &Room) -> RoomKind {
    // The sync summary count can be absent even in a populated room. The SDK
    // fetches members only while its local member list is unsynced.
    let direct = room.is_direct().await.ok();
    if direct != Some(false) {
        return classify_room(direct, Err(()));
    }
    let members = room
        .members(matrix_sdk::RoomMemberships::JOIN)
        .await
        .map(|members| members.len())
        .map_err(|_| ());
    classify_room(direct, members)
}

fn participation_sender_allowed(allow_list: Option<&str>, sender: &str, bot_uid: &str) -> bool {
    allow_list.is_some_and(|list| !list.trim().is_empty())
        && is_allowed(allow_list, sender, bot_uid)
}

/// Participation is bridge-local; enabling requires the executor's synced
/// generation receipt before the room is told it is active.
async fn handle_participation(
    args: &str,
    room: &Room,
    server: &Server,
    login_id: &str,
    owning_agent: &str,
    allow_list: Option<&str>,
    sender: &str,
) -> anyhow::Result<String> {
    if !participation_sender_allowed(
        allow_list,
        sender,
        room.client().user_id().map(|id| id.as_str()).unwrap_or(""),
    ) {
        return Ok("!chaz Error: participation requires an explicit allowlisted sender".into());
    }
    if room_kind(room).await != RoomKind::Group {
        return Ok("!chaz Error: participation is only available in group rooms".into());
    }
    let Some(agent) = owning_agent_entry(server, owning_agent) else {
        anyhow::bail!("owning agent not hosted");
    };
    let room_id = room.room_id().to_string();
    let db = match args {
        "on" => Some(
            server
                .registry()
                .get_or_create_conversational_channel_session(
                    &agent, login_id, "matrix", login_id, &room_id,
                )
                .await?
                .1,
        ),
        "off" | "status" => server
            .registry()
            .find_channel_session(&agent, login_id, "matrix", login_id, &room_id)
            .await?
            .map(|(_, db)| db),
        _ => return Ok("Usage: !chaz participation <on|off|status>".into()),
    };
    let Some(db) = db else {
        return Ok("Matrix participation: off".into());
    };
    let current = read_meta_from_db(&db).await.ambient_participation;
    if args == "status" {
        return Ok(if current
            .as_ref()
            .is_some_and(|p| p.matches(login_id, &room_id))
            && ambient_participation_receipt(&db).await?.as_deref()
                == current.as_ref().map(|p| p.generation.as_str())
        {
            "Matrix participation: on"
        } else {
            "Matrix participation: off"
        }
        .into());
    }
    if args == "off" {
        // Another room attached to this session must not disable its policy.
        if current
            .as_ref()
            .is_some_and(|p| p.matches(login_id, &room_id))
        {
            update_meta_on_db(&db, |meta| meta.ambient_participation = None).await?;
        }
        return Ok("Matrix participation: off".into());
    }
    if current
        .as_ref()
        .is_some_and(|p| !p.matches(login_id, &room_id))
    {
        return Ok("!chaz Error: another room is already participating on this session".into());
    }
    let policy = MatrixParticipation::new(login_id, &room_id);
    let generation = policy.generation.clone();
    update_meta_on_db(&db, |meta| meta.ambient_participation = Some(policy)).await?;
    for _ in 0..150 {
        if ambient_participation_receipt(&db).await?.as_deref() == Some(&generation)
            && read_meta_from_db(&db)
                .await
                .ambient_participation
                .as_ref()
                .is_some_and(|p| p.matches(login_id, &room_id) && p.generation == generation)
        {
            return Ok("Matrix participation: on".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // Never claim activation without executor observation. Do not clear a
    // newer setting installed while this request was waiting.
    if read_meta_from_db(&db)
        .await
        .ambient_participation
        .as_ref()
        .is_some_and(|p| p.generation == generation)
    {
        update_meta_on_db(&db, |meta| meta.ambient_participation = None).await?;
    }
    Ok("!chaz Error: executor did not confirm participation; mode is off".into())
}

/// Help text for `!chaz help` — the shared `/`-vocabulary plus the Matrix-local
/// verbs, listed under the `!chaz ` prefix this transport uses.
fn help_text() -> String {
    [
        "**chaz commands** (prefix with `!chaz `):",
        "",
        "`sessions` · `info` · `print` · `compact` · `name [<alias>]` — session ops",
        "`context` — context-window usage, shared with the agent's next turn",
        "`groups` — named agent rosters a new session can start with",
        "`agents` · `agent <add|remove|host|new|delete|share|import|set|reload|invite|rehost> …` — living agents",
        "`model [<id>|<agent> <id>]` · `role [<name> [prompt]]` · `backend <name> <url> <key>` · `backends` — LLM config",
        "`share` · `unshare` · `sync <ticket>` · `sharing <status|requests|approve|reject>` — sharing",
        "`extensions <list|add|remove|settings|set> …` — per-session extensions",
        "`channels` — rooms bound to this session",
        "",
        "Matrix-local: `attach <session>` · `detach` · `participation <on|off|status>` · `clear` · `send <msg>` · `rename` · `party`",
        "`room mode [normal|threaded-sessions]` — how this room routes messages",
        "",
        "In a DM or when @mentioned, just talk — no prefix needed.",
    ]
    .join("\n")
}

/// `!chaz room mode` / `!chaz room mode <mode>` — report or set how this room
/// routes inbound messages, durably for this login.
///
/// Enabling `threaded-sessions` detaches an existing room-wide binding so new
/// top-level traffic opens threads instead of feeding the old session; the old
/// session itself is kept and stays reachable through chaz's session UI.
/// Selecting `normal` again stops *new* threads without stranding existing
/// ones, and does not re-attach the detached session.
async fn handle_room_mode(
    arg: &str,
    target: &RoomTarget,
    server: &Arc<Server>,
    bridge_db: &BridgeDb,
    login_id: &str,
    owning_agent: &str,
    pending: &PendingApprovals,
) {
    let room_id = target.room().room_id().to_string();

    // A mode command belongs to the room, not to one of its threads: a thread
    // has no mode of its own, and answering in-thread would hide the change
    // from the room it applies to.
    if target.thread_root().is_some() {
        notice(
            target,
            "`room mode` applies to the whole room — send it on the main timeline, \
             not in a thread.",
        )
        .await;
        return;
    }

    let arg = arg.trim();
    if arg.is_empty() {
        let mode = match read_room_mode(bridge_db, login_id, &room_id).await {
            Ok(mode) => mode,
            Err(e) => {
                notice(target, format!("!chaz Error: {e}")).await;
                return;
            }
        };
        notice(
            target,
            format!(
                "This room's mode is `{}`. Change it with \
                 `!chaz room mode <normal|threaded-sessions>`.",
                mode.as_str()
            ),
        )
        .await;
        return;
    }

    let Some(mode) = RoomMode::parse(arg) else {
        let hint = if arg == "threaded-replies" {
            " `threaded-replies` is not supported in this build."
        } else {
            ""
        };
        notice(
            target,
            format!("Unknown room mode `{arg}`. Valid modes: `normal`, `threaded-sessions`.{hint}"),
        )
        .await;
        return;
    };

    // Detach before committing the mode: a failure is visible and retryable,
    // never reported as a successful enable with the old binding still active.
    let detached = if mode == RoomMode::ThreadedSessions {
        match detach_room_binding(server, login_id, owning_agent, &room_id).await {
            Ok(sid) => {
                pending
                    .slots
                    .lock()
                    .await
                    .retain(|_, req| req.scope != room_id);
                sid
            }
            Err(e) => {
                notice(
                    target,
                    format!("!chaz Error: could not detach room session: {e}"),
                )
                .await;
                return;
            }
        }
    } else {
        None
    };
    if let Err(e) = write_room_mode(bridge_db, login_id, &room_id, mode).await {
        notice(
            target,
            format!("!chaz Error: could not store room mode: {e}"),
        )
        .await;
        return;
    }
    info!(room_id, login_id, mode = mode.as_str(), "Room mode set");
    let mut text = format!("Room mode set to `{}`.", mode.as_str());
    if let Some(sid) = detached {
        text.push_str(&format!(
            " Detached the room-wide session {sid}; it is kept in the session UI."
        ));
    }
    text.push_str(match mode {
        RoomMode::ThreadedSessions => {
            " Every new top-level message now starts its own session, answered in its own \
             thread."
        }
        RoomMode::Normal => {
            " New top-level messages use the room-wide session again. Existing threads keep \
             working; a previously detached session is not re-attached."
        }
    });
    notice(target, text).await;
}

/// Unbind this room's main-timeline session from the room, without deleting the
/// session. Returns the session id that was detached, or `None` when the room
/// had no room-wide binding.
async fn detach_room_binding(
    server: &Arc<Server>,
    login_id: &str,
    owning_agent: &str,
    room_id: &str,
) -> anyhow::Result<Option<String>> {
    let Some(agent_entry) = owning_agent_entry(server, owning_agent) else {
        anyhow::bail!("owning agent '{owning_agent}' is not hosted on this bridge yet");
    };
    let Some((_cv, db)) = server
        .registry()
        .find_channel_session(&agent_entry, login_id, "matrix", login_id, room_id)
        .await?
    else {
        return Ok(None);
    };
    let sid = db.root_id().to_string();
    unbind_transport(&db, "matrix", login_id, room_id).await?;
    if let Some(agent_db) = server
        .registry()
        .open_agent_db(&agent_entry.db_id, None)
        .await?
    {
        agent_db.unexpose_session_from(&sid, login_id).await?;
    }
    info!(
        room_id,
        session_db_id = %sid,
        "Detached room-wide session on switch to threaded-sessions"
    );
    Ok(Some(sid))
}

/// `!chaz attach <session>` — bind this room to a specific session and install
/// the response callback so future writes reach the room. Bridge-local because
/// it touches the live attached-channel map and matrix `Room`.
#[allow(clippy::too_many_arguments)]
async fn handle_attach(
    arg: &str,
    target: &RoomTarget,
    server: Arc<Server>,
    login_id: &str,
    owning_agent: &str,
    attached_channels: AttachedChannels,
    pending: PendingApprovals,
) {
    let arg = arg.trim();
    if arg.is_empty() {
        notice(target, "Usage: !chaz attach <session-name-or-id>").await;
        return;
    }
    let room = target.room().clone();
    let room_id = room.room_id().to_string();
    let (_cv, target_db) = match server.registry().resolve_session(arg).await {
        Ok(r) => r,
        Err(e) => {
            notice(target, format!("!chaz Error: unknown session '{arg}': {e}")).await;
            return;
        }
    };
    let target_sid = target_db.root_id().to_string();

    // Bind the room into the session DB, then expose the session on this bridge
    // and ensure the owning agent hosts it so the daemon picks it up.
    if let Err(e) =
        chaz_core::session::bind_conversational_transport(&target_db, "matrix", login_id, &room_id)
            .await
    {
        notice(target, format!("!chaz Error: failed to bind room: {e}")).await;
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

    // Install outbound delivery on the newly-attached session so future
    // writes (the daemon's replies, scheduler fires) reach this room.
    if let Err(e) = attach_once(
        &attached_channels,
        &target_db,
        RoomTarget::timeline(room),
        &server,
        login_id,
        owning_agent,
        pending.clone(),
    )
    .await
    {
        error!("Failed to attach response callback: {e}");
    }

    notice(
        target,
        format!("Attached this room to session {target_sid}."),
    )
    .await;
}

/// `!chaz detach` — unbind this room from its session.
async fn handle_detach(
    target: &RoomTarget,
    server: Arc<Server>,
    login_id: &str,
    owning_agent: &str,
) {
    let room_id = target.room().room_id().to_string();
    let Some(agent_entry) = owning_agent_entry(&server, owning_agent) else {
        notice(target, "!chaz Error: owning agent not available").await;
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
            notice(
                target,
                "Room detached. Future messages will create a fresh session.",
            )
            .await;
        }
        Ok(None) => notice(target, "This room isn't bound to a session.").await,
        Err(e) => notice(target, format!("!chaz Error: {e}")).await,
    }
}

/// Why a room-wide verb (`attach`, `detach`, `participation`) is refused where
/// thread-scoped behavior would be needed, or `None` when the verb may proceed.
///
/// These verbs change or read a *room-wide* binding. In a managed thread that
/// binding is not the session the sender is looking at, and at the top level of
/// a `threaded-sessions` room it is a binding the mode deliberately detached.
/// Either way, doing the room-wide thing silently is the wrong answer;
/// thread-scoped rebinding is a separate design.
fn rebinding_refusal(target: &RoomTarget, mode: RoomMode) -> Option<&'static str> {
    if target.thread_root().is_some() {
        return Some(
            "`attach`/`detach`/`participation` act on this room's main-timeline session, \
             not a thread's session, so they are refused inside a thread. Thread-scoped \
             rebinding isn't designed yet; use `!chaz info` here to see which session this \
             thread is.",
        );
    }
    if mode == RoomMode::ThreadedSessions {
        return Some(
            "This room runs in `threaded-sessions` mode, where each thread has its own \
             session and the room-wide binding is deliberately detached, so \
             `attach`/`detach`/`participation` are refused here. Switch the room back with \
             `!chaz room mode normal` first if you want a room-wide session.",
        );
    }
    None
}

impl Bridge for MatrixBridge {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let creds = self.creds;
        let login_id = self.login_id.clone();
        let owning_agent = self.owning_agent.clone();
        let state_dir = self.state_dir;
        let secrets = self.secrets;
        let config = Arc::new(self.config);
        let bridge_db = self.bridge_db;
        let shutdown = self.shutdown;

        let allow_list = creds
            .allow_list
            .clone()
            .or_else(|| config.allow_list.clone());
        let room_size_limit = creds.room_size_limit.or(config.room_size_limit);

        // --- Connect: login/restore, auto-join, prime the sync token ---
        let login = Login {
            homeserver_url: creds.homeserver_url.clone(),
            username: creds.username.clone(),
            password: creds.password.clone(),
        };
        let mut mc =
            MatrixClient::login(&login, state_dir.as_deref(), "chaz", Some(&login_id)).await?;
        mc.ensure_cross_signed(&login).await?;

        mc.install_autojoin(allow_list.clone(), room_size_limit);

        // Initial sync primes the token *before* the message handlers are
        // installed, so room history is not replayed through them on startup.
        mc.initial_sync().await;

        let backfilled_rooms: Arc<Mutex<HashMap<String, BackfilledRoom>>> =
            Arc::new(Mutex::new(HashMap::new()));
        for room in mc.client().joined_rooms() {
            if let Err(e) = backfill_joined_room(
                &room,
                &server,
                &bridge_db,
                &login_id,
                &owning_agent,
                &backfilled_rooms,
            )
            .await
            {
                error!(room_id = %room.room_id(), "Join history backfill failed: {e}");
            }
        }

        // Subsequent joins (including invite auto-joins) bootstrap at join,
        // not when the room first addresses the agent.
        {
            let server = server.clone();
            let login_id = login_id.clone();
            let owning_agent = owning_agent.clone();
            let backfilled_rooms = backfilled_rooms.clone();
            let bridge_db = bridge_db.clone();
            mc.client()
                .add_event_handler(move |event: OriginalSyncRoomMemberEvent, room: Room| {
                    let server = server.clone();
                    let login_id = login_id.clone();
                    let owning_agent = owning_agent.clone();
                    let backfilled_rooms = backfilled_rooms.clone();
                    let bridge_db = bridge_db.clone();
                    async move {
                        if room.client().user_id() != Some(event.state_key.as_ref())
                            || event.content.membership != MembershipState::Join
                        {
                            return;
                        }
                        if let Err(e) = backfill_joined_room(
                            &room,
                            &server,
                            &bridge_db,
                            &login_id,
                            &owning_agent,
                            &backfilled_rooms,
                        )
                        .await
                        {
                            error!(room_id = %room.room_id(), "Join history backfill failed: {e}");
                        }
                    }
                });
        }

        // The SDK decrypts before dispatch, so a message the bridge can read
        // reaches the handler below as an ordinary `m.room.message`. What
        // arrives here is an event it could not decrypt: no room key yet, a
        // withheld key, or a sender device that is not cross-signed. Name it,
        // so an encrypted room that gets no reply is diagnosable from the log.
        {
            let allow_list = allow_list.clone();
            mc.client().add_event_handler(
                move |event: OriginalSyncRoomEncryptedEvent, room: Room| {
                    let allow_list = allow_list.clone();
                    async move {
                        let Some(bot_uid) = room.client().user_id().map(|u| u.to_string()) else {
                            return;
                        };
                        let session_id = match &event.content.scheme {
                            EncryptedEventScheme::MegolmV1AesSha2(c) => c.session_id.as_str(),
                            _ => "",
                        };
                        if is_allowed(allow_list.as_deref(), event.sender.as_str(), &bot_uid) {
                            warn!(
                                room_id = %room.room_id(),
                                sender = %event.sender,
                                event_id = %event.event_id,
                                session_id,
                                "Could not decrypt a message; it is ignored. The sender's \
                                 device must be cross-signed and must share its room key \
                                 with this device"
                            );
                        } else {
                            debug!(
                                room_id = %room.room_id(),
                                sender = %event.sender,
                                event_id = %event.event_id,
                                "Could not decrypt a message from a sender outside the allow list"
                            );
                        }
                    }
                },
            );
        }

        info!("The client is ready! Listening to new messages…");

        // Rooms with outbound delivery installed, and the reconciler that
        // owns each one's persisted progress.
        let attached_channels: AttachedChannels = Arc::new(Mutex::new(HashMap::new()));

        // Posted tool-approval prompts awaiting a reaction/command, keyed by the
        // prompt's event id. Shared across the startup attach, the message
        // handler, and the reaction handler.
        let pending_approvals: PendingApprovals = Arc::new(ApprovalRelay {
            // No timeout here: the daemon owns the clock and stamps each
            // request with its own deadline.
            slots: Mutex::new(HashMap::new()),
        });

        // --- Startup: install response callbacks for every room this login
        //     already has a session bound to. This is what makes the daemon's
        //     replies (including scheduler fires) deliver when no user has
        //     recently spoken in the room. We discover them by walking the
        //     owning agent's session registry — the sessions exposed on this
        //     bridge — and reading each session's transport bindings. ---
        {
            let server = server.clone();
            let client = mc.client().clone();
            let attached_channels = attached_channels.clone();
            let login_id = login_id.clone();
            let owning_agent = owning_agent.clone();
            let pending_approvals = pending_approvals.clone();
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
                    for (transport, chan_login, channel) in bindings {
                        if transport != "matrix" || chan_login != login_id {
                            continue;
                        }
                        attach_existing_channel(
                            &server,
                            &client,
                            &attached_channels,
                            &login_id,
                            &owning_agent,
                            &channel,
                            &r.session_db_id,
                            pending_approvals.clone(),
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
            let attached_channels = attached_channels.clone();
            let message_counts: Arc<Mutex<HashMap<String, u64>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let seen_events: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
            // Per-process pilot ceiling; persist a room budget if this
            // mode becomes a long-lived feature across bridge restarts.
            let participation_counts: Arc<Mutex<HashMap<String, u64>>> =
                Arc::new(Mutex::new(HashMap::new()));
            // Serialize setting commands so an expired enable cannot clear a
            // newer enable (or undo an off) while waiting for its receipt.
            let participation_command_lock = Arc::new(Mutex::new(()));
            let pending_approvals = pending_approvals.clone();
            let bridge_db = bridge_db.clone();
            // Serialize mode switches and inbound writes per room.
            let room_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>> = Arc::default();

            mc.client().add_event_handler(
                move |event: OriginalSyncRoomMessageEvent, room: Room| {
                    let server = server.clone();
                    let config = config.clone();
                    let secrets = secrets.clone();
                    let login_id = login_id.clone();
                    let owning_agent = owning_agent.clone();
                    let allow_list = allow_list.clone();
                    let attached_channels = attached_channels.clone();
                    let message_counts = message_counts.clone();
                    let backfilled_rooms = backfilled_rooms.clone();
                    let seen_events = seen_events.clone();
                    let participation_counts = participation_counts.clone();
                    let participation_command_lock = participation_command_lock.clone();
                    let pending_approvals = pending_approvals.clone();
                    let bridge_db = bridge_db.clone();
                    let room_locks = room_locks.clone();
                    // Own task, awaited by the returned future: the SDK polls
                    // handlers inline in its sync-loop future, and in debug
                    // builds that nesting left a mutating command's signed
                    // commit too little worker stack. Awaiting keeps the SDK's
                    // sequential delivery unchanged.
                    let handled = tokio::spawn(async move {
                        if room.state() != RoomState::Joined {
                            return;
                        }
                        let MessageType::Text(text_content) = &event.content.msgtype else {
                            return;
                        };
                        let Some(bot_uid) = room.client().user_id().map(|u| u.to_string()) else {
                            return;
                        };
                        if !is_allowed(allow_list.as_deref(), event.sender.as_str(), &bot_uid) {
                            return;
                        }

                        // Apply the join cutoff before commands or address checks:
                        // delayed history must not execute an old command either.
                        let room_id = room.room_id().to_string();
                        if let Err(e) = backfill_joined_room(
                            &room, &server, &bridge_db, &login_id, &owning_agent, &backfilled_rooms,
                        ).await {
                            error!(room_id, "Join history backfill failed: {e}");
                            return;
                        }
                        if backfilled_rooms.lock().await.get(&room_id).is_some_and(
                            |value| value.prejoin_ids.contains(event.event_id.as_str()),
                        ) {
                            return;
                        }
                        let raw = text_content.body.trim_start();

                        // --- Thread routing, decided before anything else ---
                        //
                        // Two independent facts: the room's durable mode, and
                        // whether this event arrived inside a Matrix thread the
                        // bridge already manages a session for. A reply in a
                        // managed thread continues that thread's session even
                        // after the room's mode changed, which is what keeps a
                        // mode switch from stranding live conversations.
                        //
                        // Serialize mode switches and inbound writes for this
                        // room, and dedupe sync redelivery under that lock.
                        // Dedupe after successful bootstrap, so a failed fetch
                        // does not consume the event on a later sync retry.
                        let lock = room_locks
                            .lock()
                            .await
                            .entry(room_id.clone())
                            .or_default()
                            .clone();
                        let _guard = lock.lock().await;
                        if seen_events.lock().await.contains(event.event_id.as_str()) {
                            return;
                        }
                        let mode = match read_room_mode(&bridge_db, &login_id, &room_id).await {
                            Ok(mode) => mode,
                            Err(e) => {
                                notice(
                                    &RoomTarget::timeline(room.clone()),
                                    format!("!chaz Error: room mode unreadable: {e}"),
                                )
                                .await;
                                return;
                            }
                        };
                        let inbound_thread_root = match &event.content.relates_to {
                            Some(Relation::Thread(thread)) => Some(thread.event_id.clone()),
                            _ => None,
                        };
                        let thread_is_managed =
                            if needs_managed_lookup(mode, inbound_thread_root.as_deref()) {
                                let root = inbound_thread_root
                                    .as_ref()
                                    .expect("needs_managed_lookup implies a thread root");
                                match is_managed_thread(
                                    &server,
                                    &owning_agent,
                                    &login_id,
                                    &room,
                                    root,
                                )
                                .await
                                {
                                    Ok(managed) => managed,
                                    Err(e) => {
                                        notice(
                                            &RoomTarget::thread(room.clone(), root.clone()),
                                            format!(
                                                "!chaz Error: could not resolve this thread: {e}"
                                            ),
                                        )
                                        .await;
                                        return;
                                    }
                                }
                            } else {
                                false
                            };
                        // The thread this event continues, if any. A top-level
                        // event continues nothing, so both a command and the
                        // chat path start from the main timeline here; only the
                        // chat path below may *open* a thread.
                        let continued_root = continued_thread_root(
                            mode,
                            inbound_thread_root.as_deref(),
                            thread_is_managed,
                        );
                        let target = match &continued_root {
                            Some(root) => RoomTarget::thread(room.clone(), root.clone()),
                            // An unmanaged thread reply (including any thread in
                            // an ordinary room) keeps the pre-existing
                            // main-timeline behavior.
                            None => RoomTarget::timeline(room.clone()),
                        };

                        // Command channel: `!chaz` optionally followed by args.
                        // A word boundary is required so `!chazfoo` is treated
                        // as a plain message, not the command `foo`.
                        let command_inner = raw.strip_prefix("!chaz").and_then(|r| {
                            (r.is_empty() || r.starts_with(char::is_whitespace))
                                .then(|| r.trim().to_string())
                        });

                        if let Some(inner) = command_inner {
                            seen_events.lock().await.insert(event.event_id.to_string());
                            let verb = inner.split_whitespace().next().unwrap_or("");
                            // View-local Matrix verbs, checked before the
                            // shared parser. These either need bridge state
                            // (response-callback install) or are Matrix-only.
                            match verb {
                                "" | "help" => {
                                    let _ = target
                                        .send(RoomMessageEventContent::text_markdown(help_text()))
                                        .await;
                                    return;
                                }
                                "party" => {
                                    notice(&target, ".🎉🎊🥳 let's PARTY!! 🥳🎊🎉").await;
                                    return;
                                }
                                "clear" => {
                                    notice(
                                        &target,
                                        "!chaz clear: All messages before this will be ignored",
                                    )
                                    .await;
                                    return;
                                }
                                "room" => {
                                    // `room mode [<mode>]` is the only
                                    // sub-verb. A word boundary is required so
                                    // `room modex` is a usage error rather than
                                    // an unknown mode named `x`.
                                    let rest = inner.strip_prefix("room").unwrap_or("").trim();
                                    let arg = rest.strip_prefix("mode").filter(|tail| {
                                        tail.is_empty() || tail.starts_with(char::is_whitespace)
                                    });
                                    match arg.map(str::trim) {
                                        Some(arg) => {
                                            handle_room_mode(
                                                arg,
                                                &target,
                                                &server,
                                                &bridge_db,
                                                &login_id,
                                                &owning_agent,
                                                &pending_approvals,
                                            )
                                            .await
                                        }
                                        None => {
                                            notice(
                                                &target,
                                                "Usage: !chaz room mode [normal|threaded-sessions]",
                                            )
                                            .await
                                        }
                                    }
                                    return;
                                }
                                "send" | "rename" if target.thread_root().is_some() || mode == RoomMode::ThreadedSessions => {
                                    notice(&target, "`send` and `rename` use whole-room history, so they are unavailable in thread contexts. Use a plain message or `!chaz name` inside a thread instead.").await;
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
                                "approve" | "deny" => {
                                    resolve_pending_approval(
                                        &pending_approvals,
                                        &target,
                                        event.sender.as_str(),
                                        verb == "approve",
                                    )
                                    .await;
                                    return;
                                }
                                "participation" | "attach" | "detach" if rebinding_refusal(&target, mode).is_some() => {
                                    if let Some(refusal) = rebinding_refusal(&target, mode) {
                                        notice(&target, refusal).await;
                                    }
                                    return;
                                }
                                "participation" => {
                                    let args = inner.strip_prefix("participation").unwrap_or("").trim();
                                    let _guard = participation_command_lock.lock().await;
                                    let result = handle_participation(
                                        args, &room, &server, &login_id, &owning_agent,
                                        allow_list.as_deref(), event.sender.as_str(),
                                    ).await.unwrap_or_else(|e| format!("!chaz Error: {e}"));
                                    notice(&target, result).await;
                                    return;
                                }
                                "attach" => {
                                    let arg = inner.strip_prefix("attach").unwrap_or("").trim();
                                    handle_attach(
                                        arg,
                                        &target,
                                        server.clone(),
                                        &login_id,
                                        &owning_agent,
                                        attached_channels.clone(),
                                        pending_approvals.clone(),
                                    )
                                    .await;
                                    return;
                                }
                                "detach" => {
                                    handle_detach(
                                        &target,
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
                                    // A session-scoped command needs a session.
                                    // On the top level of a `threaded-sessions`
                                    // room there deliberately isn't one, and
                                    // creating one here would silently
                                    // resurrect the room-wide binding the mode
                                    // detached.
                                    if target.thread_root().is_none()
                                        && mode == RoomMode::ThreadedSessions
                                    {
                                        notice(
                                            &target,
                                            "This room runs in `threaded-sessions` mode, so \
                                             there is no room-wide session for a session \
                                             command to act on. Send it inside a thread to act \
                                             on that thread's session, or use `!chaz help` / \
                                             `!chaz room mode` here.",
                                        )
                                        .await;
                                        return;
                                    }
                                    if let Err(e) = dispatch_in_target(
                                        cmd,
                                        target.clone(),
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
                                Parsed::Usage(msg) => notice(&target, msg).await,
                                // Unreachable: the input always has a leading `/`.
                                Parsed::NotCommand => {}
                            }
                            return;
                        }

                        // Which session this message belongs to, and where its
                        // reply goes: the thread it continues, else the thread a
                        // top-level message in a `threaded-sessions` room opens,
                        // else the room-wide session.
                        let thread_root = continued_root.or_else(|| {
                            opened_thread_root(
                                mode,
                                inbound_thread_root.as_deref(),
                                &event.event_id,
                            )
                        });
                        let target = match &thread_root {
                            Some(root) => RoomTarget::thread(room.clone(), root.clone()),
                            None => target,
                        };
                        let channel = target.channel();

                        // Plain messages are observed; only addressed or
                        // explicitly opted-in messages start a model turn.
                        let kind = room_kind(&room).await;
                        let is_direct = kind == RoomKind::Direct;
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
                        // Ambient participation is a room-wide session policy;
                        // a thread session is addressed by definition instead.
                        let participating = if kind == RoomKind::Group && thread_root.is_none() {
                            if let Some(agent) = owning_agent_entry(&server, &owning_agent) {
                                match server.registry().find_channel_session(
                                    &agent, &login_id, "matrix", &login_id, &room_id,
                                ).await {
                                    Ok(Some((_, db))) => {
                                        let policy = read_meta_from_db(&db).await.ambient_participation;
                                        if let Some(policy) = policy {
                                            policy.matches(&login_id, &room_id)
                                                && ambient_participation_receipt(&db).await.ok().flatten()
                                                    .as_deref() == Some(policy.generation.as_str())
                                        } else { false }
                                    }
                                    _ => false,
                                }
                            } else { false }
                        } else { false };
                        // A `threaded-sessions` room is dedicated to the bot, so
                        // a top-level message there is addressed to it by
                        // definition — and a reply inside a thread the bridge
                        // routes continues that conversation whether or not it
                        // re-mentions the bot.
                        let mut wake = is_direct || mentions_bot || participating || thread_root.is_some();
                        if participating && !mentions_bot && !is_direct {
                            let mut counts = participation_counts.lock().await;
                            let count = counts.entry(room_id.clone()).or_default();
                            if *count >= 20 {
                                tracing::warn!(room_id, "Ambient turn budget exhausted; recording without wake");
                                wake = false;
                            } else {
                                *count += 1;
                            }
                        }
                        if wake && rate_limit(&room, &event.sender, &config, &message_counts).await {
                            wake = false;
                        }

                        // Resolve an already attached session for observations.
                        // Do not create a session from unrelated room chatter.
                        // On create this binds the channel into the session DB,
                        // attaches the owning agent as host (delegating session
                        // auth to the agent DB), and exposes the session on this
                        // bridge — so the daemon discovers and runs it.
                        //
                        // Creation is idempotent per channel: chaz-core holds a
                        // per-channel lock across the lookup and the create, so
                        // duplicate sync delivery or two near-simultaneous
                        // replies to a brand-new root produce one session.
                        let Some(agent_entry) = owning_agent_entry(&server, &owning_agent) else {
                            error!(owning_agent, room_id, "Agent not hosted; dropping message");
                            return;
                        };
                        let resolved = if wake {
                            server.registry().get_or_create_conversational_channel_session(
                                &agent_entry, &login_id, "matrix", &login_id, &channel,
                            ).await.map(Some)
                        } else {
                            server.registry().find_channel_session(
                                &agent_entry, &login_id, "matrix", &login_id, &channel,
                            ).await
                        };
                        let (_conv_id, session_db) = match resolved {
                            Ok(Some(r)) => r,
                            Ok(None) => {
                                tracing::debug!(room_id, "Unbound Matrix room observation ignored");
                                return;
                            }
                            Err(e) => {
                                error!("Failed to get session for {channel}: {e}");
                                if thread_root.is_some() {
                                    notice(
                                        &target,
                                        "!chaz Error: could not open this session; retry your message.",
                                    )
                                    .await;
                                }
                                return;
                            }
                        };
                        let session_db_id = session_db.root_id().to_string();

                        // Install outbound delivery if we haven't already, so
                        // the daemon's reply reaches this room — or this thread.
                        if let Err(e) = attach_once(
                            &attached_channels,
                            &session_db,
                            target.clone(),
                            &server,
                            &login_id,
                            &owning_agent,
                            pending_approvals.clone(),
                        )
                        .await
                        {
                            error!("Failed to register response callback: {e}");
                            if thread_root.is_some() {
                                notice(
                                    &target,
                                    "!chaz Error: could not attach session delivery; \
                                     retry your message.",
                                )
                                .await;
                                return;
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
                        // A sync after rejoining or restarting can deliver
                        // events again. A recorded event ID is never another
                        // inbound request; this durable check also keeps a
                        // thread root or reply to one turn across restarts.
                        if session.entries().iter().any(|entry| {
                            entry.routing.as_ref()
                                .and_then(|routing| routing.source.as_ref())
                                .and_then(|source| source.message_id.as_deref())
                                == Some(event.event_id.as_str())
                        }) {
                            seen_events.lock().await.insert(event.event_id.to_string());
                            return;
                        }
                        // Stamp transport provenance so an agent's reply can
                        // be routed back to this channel on this login.
                        let mut entry = chaz_core::bridge::inbound_user_entry(
                            "matrix", &login_id, &channel,
                            event.sender.as_ref(), None, raw,
                            Some(event.event_id.to_string()),
                        );
                        if participating && wake {
                            entry.routing.as_mut().unwrap().ambient_candidate = true;
                        }
                        if !wake {
                            tracing::debug!(room_id, "Recorded unaddressed Matrix observation");
                        }
                        match session.add_bridge_input(entry, wake).await {
                            Ok(_) => {
                                seen_events.lock().await.insert(event.event_id.to_string());
                            }
                            Err(e) => {
                                error!("Failed to record inbound message: {e}");
                                if thread_root.is_some() {
                                    notice(
                                        &target,
                                        "!chaz Error: could not record this message; retry it.",
                                    )
                                    .await;
                                }
                            }
                        }
                    });
                    async move {
                        if let Err(e) = handled.await {
                            error!("Matrix message handler task failed: {e}");
                        }
                    }
                },
            );
        }

        // === Reaction handler ===
        //
        // ✅/❌/⏭ on a posted approval prompt is the primary approve path. The
        // handler looks the reacted-to event up in `pending_approvals` and
        // writes the decision back into that prompt's session DB.
        {
            let pending_approvals = pending_approvals.clone();
            let allow_list = allow_list.clone();
            mc.client()
                .add_event_handler(move |event: OriginalSyncReactionEvent, room: Room| {
                    let pending_approvals = pending_approvals.clone();
                    let allow_list = allow_list.clone();
                    async move {
                        // Same gate as the message path: only an allow-listed
                        // sender (never the bot itself) can resolve an approval.
                        // Without this, any room member could ✅ a privileged
                        // tool call.
                        let Some(bot_uid) = room.client().user_id().map(|u| u.to_string()) else {
                            return;
                        };
                        if !is_allowed(allow_list.as_deref(), event.sender.as_str(), &bot_uid) {
                            return;
                        }
                        handle_approval_reaction(event, pending_approvals, &room).await;
                    }
                });
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

/// Install outbound delivery for `(channel, session)` exactly once per process.
/// A second call for the same binding is a no-op; the persisted progress is
/// what makes a repeat install after a restart resume rather than replay.
#[allow(clippy::too_many_arguments)]
async fn attach_once(
    attached: &AttachedChannels,
    session_db: &eidetica::Database,
    target: RoomTarget,
    server: &Arc<Server>,
    login_id: &str,
    owning_agent: &str,
    pending: PendingApprovals,
) -> anyhow::Result<()> {
    let key = (target.channel(), session_db.root_id().to_string());
    let mut attached = attached.lock().await;
    if attached.contains_key(&key) {
        return Ok(());
    }
    let reconciler = attach_response_callback(
        session_db,
        target,
        server,
        login_id,
        owning_agent.to_string(),
        pending,
    )
    .await?;
    info!(
        session_db_id = %key.1,
        channel = %key.0,
        "Matrix outbound delivery installed"
    );
    attached.insert(key, reconciler);
    Ok(())
}

/// Install outbound delivery for a persisted channel at startup. Skips rooms
/// the bot isn't joined to, or sessions that fail to open. The bridge does not
/// register the session for processing — the executor does that when it sees
/// the session exposed in the agent registry — so this only wires up outbound
/// delivery, and the first pass delivers whatever was committed while this
/// bridge was down.
///
/// `channel` is whatever was bound: a bare room ID for a room-wide session, or
/// a composite room-plus-thread-root key. Decoding it here is what makes a
/// thread's delayed replies still land in their thread after a restart, instead
/// of leaking onto the main timeline.
#[allow(clippy::too_many_arguments)]
async fn attach_existing_channel(
    server: &Arc<Server>,
    client: &matrix_sdk::Client,
    attached: &AttachedChannels,
    login_id: &str,
    owning_agent: &str,
    channel: &str,
    session_db_id: &str,
    pending: PendingApprovals,
) {
    let (room_id, thread_root) = match parse_thread_channel(channel) {
        Some((room_id, root)) => (room_id, Some(root)),
        None => (channel.to_string(), None),
    };
    let Ok(room_id_parsed) = matrix_sdk::ruma::RoomId::parse(&room_id) else {
        return;
    };
    let Some(room) = client.get_room(&room_id_parsed) else {
        tracing::debug!(room_id, "Not joined to room; skipping channel attach");
        return;
    };
    let target = match thread_root {
        Some(root) => RoomTarget::thread(room, root),
        None => RoomTarget::timeline(room),
    };

    let Ok((_conv_id, session_db)) = server.registry().open_session(session_db_id).await else {
        tracing::warn!(session_db_id, "Stale matrix channel — session not openable");
        return;
    };

    if let Err(e) = attach_once(
        attached,
        &session_db,
        target,
        server,
        login_id,
        owning_agent,
        pending,
    )
    .await
    {
        error!("Failed to attach response callback at startup: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tool-approval relay over the session DB.
//
// The dumb bridge runs no agent, so an approval request arrives not over an
// in-process channel but as an `ApprovalRequest` entry the daemon's proxy
// writes into the (synced) session DB. The watcher renders each new request to
// its room; a human's reaction (or `!chaz approve`/`deny`) writes an
// `ApprovalDecision` entry back, which syncs to the daemon and unblocks the
// runtime. The bridge holds no oneshot — the session DB is the whole channel.
// ---------------------------------------------------------------------------

/// What a posted approval prompt resolves: the session DB to write the decision
/// into, the request it answers, and the conversation it was posted in (so a
/// `!chaz approve` text command resolves that conversation's oldest prompt).
struct PendingApproval {
    session_db: eidetica::Database,
    request_id: String,
    /// The conversation the prompt was posted into — a room's main timeline or
    /// one of its threads, spelled as that target's chaz-core channel. An
    /// untargeted `!chaz approve` only ever answers a prompt from the same
    /// conversation it was sent in, so two threads with prompts open at once
    /// cannot answer each other's.
    scope: String,
    /// Post order, so an untargeted `!chaz approve` answers the prompt the
    /// conversation has had open longest rather than an arbitrary map entry.
    seq: u64,
}

/// The posted prompts awaiting an answer.
struct ApprovalRelay {
    slots: Mutex<HashMap<OwnedEventId, PendingApproval>>,
}
type PendingApprovals = Arc<ApprovalRelay>;

/// Stamps [`PendingApproval::seq`]. Process-wide: prompts from every room share
/// one order, and only their relative order matters.
static NEXT_PROMPT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Render the approval prompt a human reacts to. The window comes off the
/// request itself, so the room is told how long it has without this process
/// knowing anything about the daemon's configuration.
fn approval_notice(req: &chaz_core::bridge::ApprovalRequestPayload) -> String {
    format!(
        "🔒 **Tool approval required**\n\n\
         **Tool:** `{}`\n\
         **Risk:** {}\n\
         **Args:** `{}`\n\
         **Expires:** {}\n\n\
         React: ✅ approve · ❌ deny · ⏭ approve all\n\
         Or reply: `!chaz approve` / `!chaz deny`",
        req.tool_name,
        req.risk_level,
        req.arguments_display,
        chaz_core::bridge::render_approval_window(req.timeout_secs),
    )
}

/// Write an `ApprovalDecision` entry resolving `request_id` into the session DB.
/// The decision syncs to the daemon, whose proxy matches it back and unblocks
/// the runtime's `request_approval`.
///
/// Returns the decision already on record when there is one, in which case
/// nothing is written: the daemon has closed this request and the answer has
/// nothing left to resolve. Losing that race is harmless — `resolved_decisions`
/// resolves to the daemon's outcome whether or not a late answer got written.
async fn write_approval_decision(
    session_db: &eidetica::Database,
    approver: &str,
    request_id: &str,
    decision: ApprovalDecision,
) -> Option<ApprovalDecision> {
    let sid = session_db.root_id().to_string();
    let mut session = Session::new(chaz_core::types::ConversationId(sid), session_db.clone()).await;
    if let Some(already) = chaz_core::bridge::existing_decision(session.entries(), request_id) {
        info!(%request_id, ?already, "Request already resolved; not writing this answer");
        return Some(already);
    }
    if let Err(e) = session
        .add_entry(chaz_core::bridge::approval_decision_entry(
            approver, request_id, decision,
        ))
        .await
    {
        error!("Failed to write approval decision for {request_id}: {e}");
    }
    None
}

/// What the room is told when an answer arrives for a request that is already
/// closed. `None` when the answer simply lost a race with another human's,
/// which needs no explanation.
fn already_resolved_notice(already: &ApprovalDecision) -> Option<String> {
    matches!(already, ApprovalDecision::TimedOut)
        .then(|| "⌛ That approval had already expired — the tool was not run".to_string())
}

/// Drop the posted prompt for a request the daemon has given up on, and tell
/// the room which tool it was.
///
/// The daemon owns the clock and has already written the `TimedOut` decision;
/// this only renders it, the same way the watcher renders the request itself.
async fn announce_timeout(
    pending: &PendingApprovals,
    target: &RoomTarget,
    req: &chaz_core::bridge::ApprovalRequestPayload,
) {
    pending
        .slots
        .lock()
        .await
        .retain(|_, p| p.request_id != req.request_id);
    notice(
        target,
        format!(
            "⌛ Tool approval for `{}` expired with no answer — not approved",
            req.tool_name
        ),
    )
    .await;
}

/// Install a per-session watcher that posts each new `ApprovalRequest` entry to
/// `target`, records it in `pending` so a reaction/command can resolve it, and
/// posts a notice for each request the daemon times out.
///
/// Renders only requests with no decision yet and not already posted this
/// process (`seen`); on a failed post the request is left unseen so the next
/// session write retries it. One pass runs at attach time, so a prompt still
/// open across a bridge restart is posted again (under the same transaction ID)
/// and can be answered. Timeouts already in the session at attach time are
/// history and are not announced again. A detached target posts nothing and
/// drops its pending prompts.
async fn attach_approval_watcher(
    session_db: &eidetica::Database,
    target: RoomTarget,
    login_id: String,
    pending: PendingApprovals,
) -> anyhow::Result<()> {
    let sid = session_db.root_id().to_string();
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let announced: Arc<Mutex<HashSet<String>>> = {
        let session = Session::new(
            chaz_core::types::ConversationId(sid.clone()),
            session_db.clone(),
        )
        .await;
        let history = chaz_core::bridge::unannounced_timeouts(session.entries(), &HashSet::new());
        Arc::new(Mutex::new(
            history.into_iter().map(|req| req.request_id).collect(),
        ))
    };
    let db = session_db.clone();
    let reconcile = Arc::new(move || {
        let target = target.clone();
        let pending = pending.clone();
        let seen = seen.clone();
        let announced = announced.clone();
        let sid = sid.clone();
        let db = db.clone();
        let login_id = login_id.clone();
        async move {
            if !chaz_core::session::is_bound(&db, "matrix", &login_id, &target.channel()).await? {
                pending
                    .slots
                    .lock()
                    .await
                    .retain(|_, req| req.scope != target.channel());
                return Ok(());
            }
            let session =
                Session::new(chaz_core::types::ConversationId(sid.clone()), db.clone()).await;
            let mut seen = seen.lock().await;
            let requests =
                chaz_core::bridge::unrendered_approval_requests(session.entries(), &seen);
            for req in requests {
                seen.insert(req.request_id.clone());
                let txn_id = OwnedTransactionId::from(format!("approval-{}", req.request_id));
                match target
                    .send_with_transaction_id(
                        RoomMessageEventContent::text_markdown(approval_notice(&req)),
                        &txn_id,
                    )
                    .await
                {
                    Ok(event_id) => {
                        pending.slots.lock().await.insert(
                            event_id,
                            PendingApproval {
                                session_db: db.clone(),
                                request_id: req.request_id,
                                scope: target.channel(),
                                seq: NEXT_PROMPT_SEQ.fetch_add(1, Ordering::Relaxed),
                            },
                        );
                    }
                    Err(e) => {
                        error!("Failed to post approval request: {e}");
                        seen.remove(&req.request_id); // retry on next write
                    }
                }
            }

            let mut announced = announced.lock().await;
            for req in chaz_core::bridge::unannounced_timeouts(session.entries(), &announced) {
                announced.insert(req.request_id.clone());
                info!(
                    session = %sid, request_id = %req.request_id, tool = %req.tool_name,
                    "Daemon timed out an approval; telling the room"
                );
                announce_timeout(&pending, &target, &req).await;
            }
            Ok::<(), anyhow::Error>(())
        }
    });
    let on_write = reconcile.clone();
    session_db
        .on_write(move |event, _| {
            // Must not be gated on `Local`: the request this renders is
            // written by the daemon running the agent, which on a split
            // deployment is a different peer and arrives via sync. The same
            // goes for the timeout decision that closes it.
            tracing::trace!(source = ?event.source(), "Session write; rescanning approval requests");
            let reconcile = on_write.clone();
            // Detached: eidetica awaits write callbacks before a commit (or a
            // peer's sync request) completes, so a Matrix send here would
            // stall sync. `seen` serializes the reconciles.
            tokio::spawn(async move {
                if let Err(e) = reconcile().await {
                    error!("Approval prompt reconcile failed: {e}");
                }
            });
            Box::pin(async { Ok(()) })
        })
        .await?
        .detach();
    reconcile().await?;
    Ok(())
}

/// Resolve an approval decision arriving as an emoji reaction on a prompt.
async fn handle_approval_reaction(
    event: OriginalSyncReactionEvent,
    pending: PendingApprovals,
    room: &Room,
) {
    let relates_to = &event.content.relates_to;
    let decision = match relates_to.key.as_str() {
        "✅" => ApprovalDecision::Approve,
        "❌" => ApprovalDecision::Deny,
        "⏭" | "⏭️" => ApprovalDecision::ApproveAll,
        _ => return,
    };
    let Some(req) = pending.slots.lock().await.remove(&relates_to.event_id) else {
        return;
    };
    info!(
        scope = %req.scope, request_id = %req.request_id, ?decision,
        "Approval decision via reaction"
    );
    let already = write_approval_decision(
        &req.session_db,
        event.sender.as_str(),
        &req.request_id,
        decision,
    )
    .await;
    if let Some(text) = already.as_ref().and_then(already_resolved_notice) {
        // Answer where the prompt was posted, which for a thread's prompt is
        // that thread — the reaction itself carries no thread relation.
        notice(&scope_target(room, &req.scope), text).await;
    }
}

/// Rebuild the delivery target a pending prompt was posted to from its stored
/// scope. Falls back to the room's main timeline when the scope is not a thread
/// channel, which is exactly the room-level case.
fn scope_target(room: &Room, scope: &str) -> RoomTarget {
    match parse_thread_channel(scope) {
        Some((_room_id, root)) => RoomTarget::thread(room.clone(), root),
        None => RoomTarget::timeline(room.clone()),
    }
}

/// Approve or deny the oldest pending tool-approval request in this
/// conversation — the `!chaz approve` / `!chaz deny` text path.
///
/// Scoped to the target, not the room: sent inside a thread it answers that
/// thread's oldest prompt, and it will not reach across to a sibling thread's or
/// the main timeline's.
async fn resolve_pending_approval(
    pending: &PendingApprovals,
    target: &RoomTarget,
    approver: &str,
    approve: bool,
) {
    let scope = target.channel();
    let resolved = {
        let mut p = pending.slots.lock().await;
        let event_id = chaz_core::bridge::oldest_pending(
            p.iter()
                .map(|(id, pa)| (id.clone(), pa.scope.as_str(), pa.seq)),
            &scope.as_str(),
        );
        event_id.and_then(|id| p.remove(&id))
    };
    let Some(req) = resolved else {
        notice(target, "No pending approval requests").await;
        return;
    };
    let decision = if approve {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Deny
    };
    let already =
        write_approval_decision(&req.session_db, approver, &req.request_id, decision).await;
    let label = match already.as_ref() {
        Some(already) => already_resolved_notice(already)
            .unwrap_or_else(|| "That approval had already been answered".to_string()),
        None if approve => "✅ Approved".to_string(),
        None => "❌ Denied".to_string(),
    };
    notice(target, label).await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn command_dispatch_futures_stay_bounded() {
        fn future_size<F: std::future::Future>(
            _: impl FnOnce(
                super::Command,
                super::RoomTarget,
                std::sync::Arc<chaz_core::server::Server>,
                std::sync::Arc<chaz_core::config::Config>,
                chaz_core::security::SecretStore,
                &'static str,
                &'static str,
            ) -> F,
        ) -> usize {
            std::mem::size_of::<F>()
        }

        fn dispatch_size<F: std::future::Future>(
            _: impl FnOnce(super::Command, &'static chaz_core::commands::CommandContext<'static>) -> F,
        ) -> usize {
            std::mem::size_of::<F>()
        }
        // Measure both the actual room -> dispatch -> render composition and
        // its shared dispatch child, without constructing or polling either.
        // Rust 1.96 / x86_64: 27,112 and 6,304 bytes respectively. Removing
        // command arm boxing grows the child to 21,824 bytes, but leaves the
        // parent's layout unchanged (another await dominates its state size).
        // These ceilings leave ~21% / ~30% headroom, not a poll-stack safety
        // guarantee. Keep Box::pin and default-stack transport tests as well.
        let dispatch_bytes = dispatch_size(super::shared_commands::dispatch);
        eprintln!("Shared command dispatch future: {dispatch_bytes} bytes");
        assert!(
            dispatch_bytes <= 8 * 1024,
            "Shared command dispatch future grew to {dispatch_bytes} bytes (limit: 8192); \
             inspect command arm boxing before raising the measured ceiling"
        );

        let bytes = future_size(super::dispatch_in_target);
        eprintln!("Matrix command dispatch future: {bytes} bytes");
        assert!(
            bytes <= 32 * 1024,
            "Matrix command dispatch future grew to {bytes} bytes (limit: 32768); \
             inspect shared command arm boxing before raising the measured ceiling"
        );
    }

    #[test]
    fn group_eligibility_uses_joined_members_not_sync_summary() {
        let group = super::classify_room;
        use super::RoomKind;
        // /sync can omit summary even though /joined_members reports three.
        assert_eq!(group(Some(false), Ok(3)), RoomKind::Group);
        assert_eq!(group(Some(false), Ok(2)), RoomKind::Direct);
        assert_eq!(group(Some(false), Ok(1)), RoomKind::Direct);
        assert_eq!(group(Some(true), Ok(3)), RoomKind::Direct);
        assert_eq!(group(None, Ok(3)), RoomKind::Unknown);
        assert_eq!(group(Some(false), Err(())), RoomKind::Unknown);
    }

    #[test]
    fn participation_requires_explicit_matching_allowlist() {
        let allowed = super::participation_sender_allowed;
        assert!(!allowed(None, "@human:test", "@bot:test"));
        assert!(!allowed(Some(""), "@human:test", "@bot:test"));
        assert!(!allowed(Some("  "), "@human:test", "@bot:test"));
        assert!(!allowed(Some("@alice:test"), "@other:test", "@bot:test"));
        assert!(!allowed(Some("@bot:test"), "@bot:test", "@bot:test"));
        assert!(allowed(Some("@alice:test"), "@alice:test", "@bot:test"));
    }

    #[test]
    fn matrix_typing_wire_lease_exceeds_renewal() {
        assert!(super::TYPING_LEASE > super::TYPING_RENEWAL * 2);
        assert_eq!(
            serde_json::to_value(super::turn_typing(true)).unwrap(),
            serde_json::json!({"typing": true, "timeout": 10000})
        );
        assert_eq!(
            serde_json::to_value(super::turn_typing(false)).unwrap(),
            serde_json::json!({"typing": false})
        );
    }

    #[tokio::test]
    async fn matrix_typing_renews_and_cancels_when_claim_released() {
        use chaz_core::session::{Session, TurnRequestId};
        use chaz_core::types::ConversationId;
        use eidetica::backend::database::InMemory;
        use eidetica::{Instance, NewAccount};

        let (_instance, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::passwordless("typing-test"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let mut session = Session::new(ConversationId(db.root_id().to_string()), db).await;
        let mut sent = Vec::new();
        let mut typing = false;
        let attempt = session
            .start_turn_attempt(TurnRequestId::parse("message"))
            .await
            .unwrap();
        for _ in 0..2 {
            let active = !Session::active_turn_attempts(session.database())
                .await
                .unwrap()
                .is_empty();
            super::update_typing(&mut typing, active, |state| {
                sent.push(state);
                async { Ok::<(), ()>(()) }
            })
            .await
            .unwrap();
        }
        session.complete_turn_attempt(&attempt, None).await.unwrap();
        let active = !Session::active_turn_attempts(session.database())
            .await
            .unwrap()
            .is_empty();
        super::update_typing(&mut typing, active, |state| {
            sent.push(state);
            async { Ok::<(), ()>(()) }
        })
        .await
        .unwrap();
        assert_eq!(sent, [true, true, false]);
        assert!(!typing);
        let result = super::update_typing(&mut typing, true, |_| async { Err::<(), _>(()) }).await;
        assert!(result.is_err());
        assert!(
            !typing,
            "a failed Matrix request must be retried on the next tick"
        );
    }
    use super::{MATRIX_MESSAGE_LIMIT, is_transient_matrix_send_error};
    use chaz_core::bridge::outbound::chunk_message_bytes as chunk_message;
    use matrix_sdk::{Error as MatrixSdkError, HttpError};
    use ruma::api::{
        client::uiaa::UiaaResponse,
        error::{
            Error as RumaApiError, ErrorBody, ErrorKind, FromHttpResponseError, StandardErrorBody,
        },
    };
    use ruma::events::room::message::{MessageFormat, MessageType, RoomMessageEventContent};

    /// Build a `matrix_sdk::Error` shaped exactly as the SDK surfaces a
    /// homeserver error response: `Http(Api(Server(MatrixError)))` carrying the
    /// given status. Built from real ruma types so an SDK upgrade that reshapes
    /// these enums breaks this suite instead of silently un-pinning the retry
    /// classification.
    fn matrix_api_error(status: reqwest::StatusCode) -> MatrixSdkError {
        let ruma_error = RumaApiError::new(
            status,
            ErrorBody::Standard(StandardErrorBody::new(
                ErrorKind::Unknown,
                "synthesized homeserver error".to_owned(),
            )),
        );
        MatrixSdkError::Http(Box::new(HttpError::Api(Box::new(
            FromHttpResponseError::Server(UiaaResponse::MatrixError(ruma_error)),
        ))))
    }

    #[test]
    fn matrix_chunks_are_transport_sized_and_lossless() {
        let body = "🦀".repeat(MATRIX_MESSAGE_LIMIT / 4 + 1);
        let chunks = chunk_message(&body, MATRIX_MESSAGE_LIMIT);
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MATRIX_MESSAGE_LIMIT)
        );
        assert_eq!(chunks.concat(), body);
    }

    #[test]
    fn markdown_spans_crossing_chunks_render_independently() {
        let body = format!(
            "```rust\n{}\n```",
            "x".repeat(MATRIX_MESSAGE_LIMIT - "```rust\n".len())
        );
        let chunks = chunk_message(&body, MATRIX_MESSAGE_LIMIT);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), body);

        // Matrix receives separate events, so its Markdown parser sees an
        // unclosed fence in the first event and its close in the second.
        let rendered: Vec<_> = chunks
            .iter()
            .map(RoomMessageEventContent::text_markdown)
            .collect();
        match &rendered[0].msgtype {
            MessageType::Text(text) => {
                assert_eq!(text.body, chunks[0]);
                let formatted = text
                    .formatted
                    .as_ref()
                    .expect("unclosed fence opener renders as HTML");
                assert_eq!(formatted.format, MessageFormat::Html);
                assert_eq!(
                    formatted.body,
                    "<pre><code class=\"language-rust\"></code></pre>\n"
                );
            }
            _ => panic!("text_markdown must produce a text event"),
        }
        match &rendered[1].msgtype {
            MessageType::Text(text) => {
                assert_eq!(text.body, chunks[1]);
                // The stray closing fence does not extend chunk 1's code
                // block; the x's render as a paragraph and the fence as an
                // empty code block. Pinned to current ruma output.
                let formatted = text
                    .formatted
                    .as_ref()
                    .expect("stray closing fence renders as HTML");
                assert_eq!(formatted.format, MessageFormat::Html);
                assert_eq!(
                    formatted.body,
                    format!(
                        "<p>{}</p>\n<pre><code></code></pre>\n",
                        "x".repeat(MATRIX_MESSAGE_LIMIT - "```rust\n".len())
                    )
                );
            }
            _ => panic!("text_markdown must produce a text event"),
        }
    }

    #[test]
    fn homeserver_overload_statuses_are_transient() {
        for status in [429, 503, 504] {
            assert!(
                is_transient_matrix_send_error(&matrix_api_error(
                    reqwest::StatusCode::from_u16(status).expect("valid status"),
                )),
                "status {status} should be transient"
            );
        }
    }

    #[test]
    fn permanent_http_statuses_are_not_transient() {
        for status in [400, 401, 403, 404, 500, 502] {
            assert!(
                !is_transient_matrix_send_error(&matrix_api_error(
                    reqwest::StatusCode::from_u16(status).expect("valid status"),
                )),
                "status {status} should be permanent"
            );
        }
    }

    #[test]
    fn an_error_outside_the_http_layer_is_permanent() {
        assert!(!is_transient_matrix_send_error(
            &MatrixSdkError::AuthenticationRequired
        ));
    }

    #[tokio::test]
    async fn a_reqwest_connect_error_is_transient() {
        // `reqwest::Error` has no public constructor, so the connect arm can
        // only be pinned with a real failed request; nothing listens on port 1
        // and the refusal is immediate even inside the Nix sandbox.
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("test client");
        let error = client.get("http://127.0.0.1:1/").send().await.unwrap_err();
        let error = MatrixSdkError::Http(Box::new(HttpError::Reqwest(error)));
        assert!(is_transient_matrix_send_error(&error));
    }

    #[tokio::test]
    async fn a_reqwest_request_timeout_is_transient() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(1)))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .expect("test client");
        let error = client
            .get(server.uri())
            .send()
            .await
            .expect_err("a stalled send must hit the request timeout");
        assert!(
            error.is_timeout(),
            "expected a request timeout, got: {error:?}"
        );
        assert!(!error.is_connect(), "must exercise the timeout-only arm");
        let error = MatrixSdkError::Http(Box::new(HttpError::Reqwest(error)));
        assert!(is_transient_matrix_send_error(&error));
    }

    #[tokio::test]
    async fn other_reqwest_transport_errors_stay_permanent() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Redirect-policy failures must not consume the transient retry budget.
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/elsewhere"))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::limited(0))
            .build()
            .expect("test client");
        let error = client
            .get(server.uri())
            .send()
            .await
            .expect_err("a denied redirect must fail the request");
        assert!(
            error.is_redirect(),
            "expected a redirect error, got: {error:?}"
        );
        let error = MatrixSdkError::Http(Box::new(HttpError::Reqwest(error)));
        assert!(!is_transient_matrix_send_error(&error));
    }
}
