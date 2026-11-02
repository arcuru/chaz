//! Personal Matrix workspace: one bot-managed private Space, a command-only
//! Lobby, and fresh named chat rooms that each own exactly one session.
//!
//! Everything here is management metadata. The Space and Lobby never get a
//! session or a model turn; each chat's session is created by the explicit
//! `!chaz chat new` operation, never by generic join bootstrap. Room names and
//! Space edges are labels, not identity: a chat is identified by its recorded
//! room ID and session ID, and authorization is the exact configured manager
//! MXID plus the existing allow list.
//!
//! Room creation and session-DB creation cannot be made exactly-once, so every
//! request is a durable [`Operation`] with saved creation attempts and completed
//! steps. A room-creation POST whose outcome is unknown is never
//! repeated: the operation becomes *uncertain* and an explicit retry
//! reconciles it against rooms carrying its marker state. Session creation is
//! recovered through a marker written into the DB's initial settings.
//!
//! The Matrix and session effects sit behind [`MatrixOps`] and [`SessionOps`]
//! so the state machine is tested hermetically; [`live`] holds the SDK and
//! registry implementations.

pub(crate) mod live;

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, RwLock};

use eidetica::Database;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Custom state event type carried by every room the workspace creates. Its
/// content names the workspace, operation and role; it holds no secret and,
/// like all room state, is not end-to-end encrypted.
pub(crate) const MARKER_TYPE: &str = "dev.chaz.workspace";

/// Peer-local (`chaz_peer`) store holding one record per login.
const STORE: &str = "matrix_workspace";

/// Name given to the Space and Lobby at setup.
const SPACE_NAME: &str = "Chaz";
const LOBBY_NAME: &str = "Lobby";
const LOBBY_TOPIC: &str = "Create and find chats here; talk in a chat room.";

const SETUP_STEPS: [&str; 6] = [
    "space.create",
    "lobby.create",
    "space.child",
    "lobby.parent",
    "invite.space",
    "invite.lobby",
];
const CHAT_STEPS: [&str; 7] = [
    "room.create",
    "session.provision",
    "session.label",
    "delivery",
    "space.child",
    "room.parent",
    "invite",
];
const RENAME_STEPS: [&str; 2] = ["matrix.name", "session.label"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    Space,
    Lobby,
    Chat,
}

impl Role {
    #[cfg(test)]
    fn as_str(self) -> &'static str {
        match self {
            Role::Space => "space",
            Role::Lobby => "lobby",
            Role::Chat => "chat",
        }
    }
}

/// Content of the [`MARKER_TYPE`] state event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Marker {
    pub workspace: String,
    pub operation: String,
    pub role: Role,
}

/// Everything needed to create one workspace room.
#[derive(Clone, Debug)]
pub(crate) struct RoomSpec {
    pub role: Role,
    pub name: String,
    pub topic: Option<String>,
    pub room_version: String,
    pub manager: String,
    pub marker: Marker,
}

/// Outcome of a room-creation POST that did not return a room ID.
#[derive(Debug)]
pub(crate) enum CreateError {
    /// The homeserver answered with an error: no room was created.
    Rejected(String),
    /// Transport failure, timeout or server error: a room may exist.
    Uncertain(String),
}

/// Matrix effects the workspace performs, all as the bot's own account.
pub(crate) trait MatrixOps: Send + Sync + 'static {
    fn bot_id(&self) -> anyhow::Result<String>;
    /// The homeserver's default room version for new rooms.
    fn default_room_version(&self) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// One `createRoom` POST, never retried below this call.
    fn create_room(
        &self,
        spec: &RoomSpec,
    ) -> impl Future<Output = Result<String, CreateError>> + Send;
    /// Joined rooms created by the bot that carry exactly `marker`.
    fn find_marked_rooms(
        &self,
        marker: &Marker,
    ) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send;
    /// Invite `user`; already invited or joined counts as done.
    fn invite(&self, room: &str, user: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// `m.space.child` in `space` pointing at `child`.
    fn link_child(
        &self,
        space: &str,
        child: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// `m.space.parent` in `child` pointing at `space`.
    fn link_parent(
        &self,
        child: &str,
        space: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn set_name(&self, room: &str, name: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Session effects for a workspace chat.
pub(crate) trait SessionOps: Send + Sync + 'static {
    fn agent_id(&self) -> anyhow::Result<String>;
    /// Create or recover the one session carrying `source` in its initial
    /// settings, bound to `room`, hosted by the owning agent and published on
    /// this login. Returns the session DB ID.
    fn provision(
        &self,
        room: &str,
        source: &str,
        allow_create: bool,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// Install addressed outbound delivery and the approval watcher.
    fn install_delivery(
        &self,
        room: &str,
        session: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Require this exact room/session binding in the owning agent's exposed
    /// sessions; unreadable or competing bindings refuse management too.
    fn check_binding(
        &self,
        room: &str,
        session: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Literal browse-only label on the session (its description).
    fn set_label(
        &self,
        session: &str,
        label: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

// ---------------------------------------------------------------------------
// Persisted state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Chat {
    pub handle: String,
    pub room_id: String,
    #[serde(default)]
    pub session_db_id: Option<String>,
    pub label: String,
    /// Session bound, published and delivery installed.
    pub ready: bool,
    pub operation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OpKind {
    Setup,
    NewChat { handle: String, label: String },
    Rename { handle: String, label: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OpStatus {
    /// Setup previewed, awaiting `confirm`.
    Preview,
    /// Stopped at a failed step; `retry` continues it.
    Incomplete,
    /// A room-creation outcome is unknown; `retry` reconciles it.
    Uncertain,
    Done,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub(crate) enum Step {
    Pending,
    /// Room/DB creation or Matrix rename admitted; outcome not recorded.
    Attempted,
    Done(Option<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Operation {
    pub id: String,
    pub kind: OpKind,
    pub requester: String,
    pub event_id: String,
    pub origin_room: String,
    pub status: OpStatus,
    pub steps: BTreeMap<String, Step>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceState {
    pub workspace_id: String,
    pub login_id: String,
    pub owning_agent: String,
    pub agent_db_id: String,
    pub bot_mxid: String,
    pub manager: String,
    pub room_version: String,
    #[serde(default)]
    pub space: Option<String>,
    #[serde(default)]
    pub lobby: Option<String>,
    #[serde(default)]
    pub chats: Vec<Chat>,
    #[serde(default)]
    pub operations: Vec<Operation>,
    #[serde(default)]
    pub next_op: u64,
    #[serde(default)]
    pub next_chat: u64,
}

impl WorkspaceState {
    fn chat(&self, handle: &str) -> Option<&Chat> {
        self.chats.iter().find(|c| c.handle == handle)
    }
    fn chat_mut(&mut self, handle: &str) -> Option<&mut Chat> {
        self.chats.iter_mut().find(|c| c.handle == handle)
    }
    fn op(&self, id: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.id == id)
    }
    fn unfinished(&self) -> Option<&Operation> {
        self.operations
            .iter()
            .find(|o| matches!(o.status, OpStatus::Incomplete | OpStatus::Uncertain))
    }
    fn setup_done(&self) -> bool {
        self.operations
            .iter()
            .any(|o| o.kind == OpKind::Setup && o.status == OpStatus::Done)
    }
    fn alloc_op(&mut self) -> String {
        self.next_op += 1;
        format!("op-{}", self.next_op)
    }
}

/// Durable record in the peer-local, never-synced `chaz_peer` DB.
#[derive(Clone)]
pub(crate) struct WorkspaceStore {
    db: Database,
    key: String,
}

impl WorkspaceStore {
    pub(crate) fn new(db: Database, login_id: &str) -> Self {
        use sha2::Digest;
        // Login IDs contain dots, which DocStore would read as nested paths.
        let digest = sha2::Sha256::digest(login_id.as_bytes());
        let key: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        Self {
            db,
            key: format!("ws-{key}"),
        }
    }

    async fn load(&self) -> anyhow::Result<Option<WorkspaceState>> {
        let txn = self.db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE).await?;
        match store.get_string(&self.key).await {
            Ok(json) => Ok(Some(serde_json::from_str(&json).map_err(|e| {
                anyhow::anyhow!("workspace record is malformed ({e}); refusing to guess")
            })?)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn save(&self, state: &WorkspaceState) -> anyhow::Result<()> {
        let txn = self.db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE).await?;
        store
            .set_string(&self.key, serde_json::to_string(state)?)
            .await?;
        txn.commit().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Room roles visible to the message handler
// ---------------------------------------------------------------------------

/// What a room is to the workspace, for routing before any session work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    Space,
    Lobby,
    Chat {
        handle: String,
        ready: bool,
        session_db_id: Option<String>,
    },
}

/// Room ID → role for every room the workspace recorded. Shared with join
/// bootstrap and the message handler, which must not create a session for a
/// workspace room or route its messages as an ordinary chat.
#[derive(Clone, Default)]
pub(crate) struct RoomIndex(Arc<RwLock<HashMap<String, Place>>>);

impl RoomIndex {
    pub(crate) fn get(&self, room_id: &str) -> Option<Place> {
        self.0.read().unwrap().get(room_id).cloned()
    }

    fn refresh(&self, state: Option<&WorkspaceState>) {
        let mut map = HashMap::new();
        if let Some(state) = state {
            if let Some(space) = &state.space {
                map.insert(space.clone(), Place::Space);
            }
            if let Some(lobby) = &state.lobby {
                map.insert(lobby.clone(), Place::Lobby);
            }
            for chat in &state.chats {
                map.insert(
                    chat.room_id.clone(),
                    Place::Chat {
                        handle: chat.handle.clone(),
                        ready: chat.ready,
                        session_db_id: chat.session_db_id.clone(),
                    },
                );
            }
        }
        *self.0.write().unwrap() = map;
    }
}

/// Where a management command arrived, as the handler classified it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Managed(Place),
    /// An unmanaged room with the bot and at most one other member.
    Direct,
    /// Any other unmanaged room.
    Other,
}

/// One `!chaz workspace …` / `!chaz chat …` request.
pub(crate) struct Request<'a> {
    pub sender: &'a str,
    pub event_id: &'a str,
    pub room_id: &'a str,
    pub origin: Origin,
    /// The command text after `!chaz`, starting with `workspace` or `chat`.
    pub command: &'a str,
}

/// Whether `inner` (text after `!chaz`) is a workspace management command.
pub(crate) fn is_workspace_command(inner: &str) -> bool {
    matches!(inner.split_whitespace().next(), Some("workspace" | "chat"))
}

/// How the message handler must treat an event in a room, decided before any
/// history bootstrap, session lookup or model wake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// No reply and no session work (the Space, or a marked room the record
    /// does not list).
    Ignore,
    /// Hand to [`Workspace::handle`].
    Workspace,
    /// Reply with this notice and do nothing else.
    Notice(&'static str),
    /// Ordinary chat handling.
    Normal,
}

pub(crate) const LOBBY_ONLY: &str = "The Lobby is command-only: no agent reads it. \
     Use !chaz chat new <name> to start a chat, or !chaz workspace help.";
pub(crate) const NOT_READY: &str = "This workspace chat is not ready yet, so nothing was sent \
     to the agent. See !chaz workspace status in the Lobby.";
pub(crate) const LITERAL_RENAME: &str = "Workspace chats keep the name you give them; \
     use !chaz chat rename <name> instead of the model-generated rename.";
pub(crate) const FIXED_BINDING: &str = "This is a workspace chat; its session binding is fixed \
     and cannot be attached, detached or switched here.";

/// Route one event. `command` is the text after `!chaz` for a command, or
/// `None` for a plain message; `marked` says the room carries a workspace
/// marker in its state.
pub(crate) fn route(place: Option<&Place>, marked: bool, command: Option<&str>) -> Route {
    let management = command.is_some_and(is_workspace_command);
    let verb = command
        .and_then(|c| c.split_whitespace().next())
        .unwrap_or("");
    match place {
        Some(Place::Space) => Route::Ignore,
        None if marked => Route::Ignore,
        _ if management => Route::Workspace,
        Some(Place::Lobby) => Route::Notice(LOBBY_ONLY),
        Some(Place::Chat { ready: false, .. }) => Route::Notice(NOT_READY),
        Some(Place::Chat { ready: true, .. })
            if command.is_some() && matches!(verb, "attach" | "detach" | "participation") =>
        {
            Route::Notice(FIXED_BINDING)
        }
        Some(Place::Chat { ready: true, .. }) if command.is_some() && verb == "rename" => {
            Route::Notice(LITERAL_RENAME)
        }
        _ => Route::Normal,
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

pub(crate) struct Workspace<M, S> {
    store: WorkspaceStore,
    matrix: M,
    sessions: S,
    login_id: String,
    owning_agent: String,
    manager: Option<String>,
    index: RoomIndex,
    /// Serializes every operation and holds the authoritative cached copy:
    /// the peer-local record is written only through this lock.
    state: Mutex<Option<WorkspaceState>>,
}

fn new_workspace_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("system RNG unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn link(room_id: &str) -> String {
    format!("https://matrix.to/#/{room_id}")
}

/// Creator semantics for a room version, or `None` for a version this code
/// does not recognize (setup then refuses instead of guessing).
pub(crate) fn creator_rule(version: &str) -> Option<&'static str> {
    let n: u32 = version.parse().ok()?;
    if !(1..=12).contains(&n) {
        return None;
    }
    Some(if n == 12 {
        "the bot is the room creator with permanent, unlimited power that no one can revoke"
    } else {
        "the bot creates each room at power 100; another admin can later lower it"
    })
}

const HELP: &str = "**workspace commands** (prefix with `!chaz `):\n\
\n\
In a direct chat with the bot: `workspace setup` · `workspace confirm <op>`\n\
In the Lobby: `chat new <name>` · `chat list` · `chat rename <chat> <name>`\n\
In a workspace chat: `chat rename <name>`\n\
Anywhere above: `workspace status [<op>]` · `workspace retry <op>` · `workspace cancel <op>`\n\
\n\
The Lobby is command-only; talk to the agent in a chat room.";

impl<M: MatrixOps, S: SessionOps> Workspace<M, S> {
    pub(crate) async fn open(
        store: WorkspaceStore,
        matrix: M,
        sessions: S,
        login_id: String,
        owning_agent: String,
        manager: Option<String>,
    ) -> anyhow::Result<Self> {
        let state = store.load().await?;
        if let Some(state) = &state {
            anyhow::ensure!(
                state.login_id == login_id
                    && state.owning_agent == owning_agent
                    && Some(state.manager.as_str()) == manager.as_deref()
                    && state.bot_mxid == matrix.bot_id()?
                    && state.agent_db_id == sessions.agent_id()?,
                "workspace identity differs from this login/bot/agent/manager; refusing startup"
            );
        }
        let index = RoomIndex::default();
        index.refresh(state.as_ref());
        Ok(Self {
            store,
            matrix,
            sessions,
            login_id,
            owning_agent,
            manager,
            index,
            state: Mutex::new(state),
        })
    }

    pub(crate) fn index(&self) -> RoomIndex {
        self.index.clone()
    }

    async fn save(&self, state: &WorkspaceState) -> anyhow::Result<()> {
        self.store.save(state).await?;
        self.index.refresh(Some(state));
        Ok(())
    }

    /// Handle one management command; returns the notice for the origin room,
    /// or `None` when the room gets no reply (the Space).
    pub(crate) async fn handle(&self, req: Request<'_>) -> Option<String> {
        if req.origin == Origin::Managed(Place::Space) {
            return None;
        }
        Some(match self.handle_inner(&req).await {
            Ok(text) => text,
            Err(e) => format!("!chaz Error: {e}"),
        })
    }

    async fn handle_inner(&self, req: &Request<'_>) -> anyhow::Result<String> {
        let words: Vec<&str> = req.command.split_whitespace().collect();
        let (family, verb) = (words.first().copied(), words.get(1).copied());
        if family == Some("workspace") && matches!(verb, None | Some("help")) {
            return Ok(HELP.to_string());
        }
        // Authorization: the exact configured manager, never membership.
        let Some(manager) = self.manager.as_deref() else {
            anyhow::bail!("the workspace is not configured for this login (no workspace_manager)");
        };
        anyhow::ensure!(
            req.sender == manager,
            "workspace management is limited to the configured manager"
        );
        let mut guard = self.state.lock().await;
        if let Some(state) = guard.as_ref() {
            anyhow::ensure!(
                state.login_id == self.login_id
                    && state.owning_agent == self.owning_agent
                    && state.manager == manager
                    && state.bot_mxid == self.matrix.bot_id()?
                    && state.agent_db_id == self.sessions.agent_id()?,
                "the workspace record belongs to login {} / agent {} / manager {}, \
                 not this configuration; refusing",
                state.login_id,
                state.owning_agent,
                state.manager
            );
            // Repeated delivery of one event returns the same receipt.
            if let Some(op) = state.operations.iter().find(|o| o.event_id == req.event_id) {
                return Ok(describe(state, op));
            }
        }
        // The literal remainder after `n` command words, internal spacing kept.
        let rest = |n: usize| {
            let mut tail = req.command.trim_start();
            for _ in 0..n {
                tail = tail
                    .trim_start()
                    .split_once(char::is_whitespace)
                    .map_or("", |(_, r)| r);
            }
            tail.trim().to_string()
        };
        let origin = &req.origin;
        match (family, verb) {
            (Some("workspace"), Some("setup")) => {
                anyhow::ensure!(
                    *origin == Origin::Direct,
                    "start setup from a direct chat with the bot"
                );
                self.setup_preview(&mut guard, req, manager).await
            }
            (Some("workspace"), Some("confirm")) => {
                anyhow::ensure!(
                    *origin == Origin::Direct,
                    "confirm setup from a direct chat with the bot"
                );
                let state = guard
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no setup is pending"))?;
                let id = words.get(2).copied().unwrap_or("");
                let op = state
                    .operations
                    .iter_mut()
                    .find(|o| o.id == id && o.kind == OpKind::Setup)
                    .ok_or_else(|| anyhow::anyhow!("no setup operation {id:?}"))?;
                if op.status != OpStatus::Preview {
                    return Ok(describe(state, state.op(id).unwrap()));
                }
                op.status = OpStatus::Incomplete;
                self.save(state).await?;
                self.drive(state, id).await
            }
            (Some("workspace"), Some(cmd @ ("status" | "retry" | "cancel"))) => {
                anyhow::ensure!(
                    !matches!(origin, Origin::Other),
                    "use the Lobby or a direct chat with the bot"
                );
                let state = guard
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no workspace has been set up"))?;
                let id = words.get(2).copied();
                match (cmd, id) {
                    ("status", None) => Ok(summary(state)),
                    ("status", Some(id)) => state
                        .op(id)
                        .map(|op| describe(state, op))
                        .ok_or_else(|| anyhow::anyhow!("no operation {id:?}")),
                    ("retry", Some(id)) => {
                        let op = state
                            .op(id)
                            .ok_or_else(|| anyhow::anyhow!("no operation {id:?}"))?;
                        if !matches!(op.status, OpStatus::Incomplete | OpStatus::Uncertain) {
                            return Ok(describe(state, op));
                        }
                        self.drive(state, id).await
                    }
                    ("cancel", Some(id)) => self.cancel(state, id).await,
                    _ => anyhow::bail!("usage: !chaz workspace {cmd} <op>"),
                }
            }
            (Some("chat"), Some("new")) => {
                anyhow::ensure!(
                    *origin == Origin::Managed(Place::Lobby),
                    "create chats from the workspace Lobby"
                );
                let label = valid_label(&rest(2))?;
                let state = guard
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no workspace has been set up"))?;
                self.ensure_idle(state)?;
                state.next_chat += 1;
                let handle = format!("c{}", state.next_chat);
                let id = state.alloc_op();
                let mut steps = BTreeMap::new();
                for step in CHAT_STEPS {
                    steps.insert(step.to_string(), Step::Pending);
                }
                state.operations.push(Operation {
                    id: id.clone(),
                    kind: OpKind::NewChat { handle, label },
                    requester: req.sender.to_string(),
                    event_id: req.event_id.to_string(),
                    origin_room: req.room_id.to_string(),
                    status: OpStatus::Incomplete,
                    steps,
                    last_error: None,
                });
                self.save(state).await?;
                self.drive(state, &id).await
            }
            (Some("chat"), Some("list")) => {
                anyhow::ensure!(
                    *origin == Origin::Managed(Place::Lobby),
                    "list chats from the workspace Lobby"
                );
                guard
                    .as_ref()
                    .map(list)
                    .ok_or_else(|| anyhow::anyhow!("no workspace has been set up"))
            }
            (Some("chat"), Some("rename")) => {
                let (handle, label) = match origin {
                    Origin::Managed(Place::Chat { handle, .. }) => (handle.clone(), rest(2)),
                    Origin::Managed(Place::Lobby) => {
                        let handle = words.get(2).copied().unwrap_or("");
                        (handle.to_string(), rest(3))
                    }
                    _ => anyhow::bail!("rename from the workspace Lobby or inside the chat"),
                };
                let label = valid_label(&label)?;
                let state = guard
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no workspace has been set up"))?;
                let chat = state.chat(&handle).ok_or_else(|| {
                    anyhow::anyhow!("no workspace chat {handle:?}; see !chaz chat list")
                })?;
                anyhow::ensure!(
                    chat.ready,
                    "chat {handle} is not ready; finish its creation first"
                );
                self.ensure_idle(state)?;
                let id = state.alloc_op();
                let steps = RENAME_STEPS
                    .into_iter()
                    .map(|s| (s.to_string(), Step::Pending))
                    .collect();
                state.operations.push(Operation {
                    id: id.clone(),
                    kind: OpKind::Rename { handle, label },
                    requester: req.sender.to_string(),
                    event_id: req.event_id.to_string(),
                    origin_room: req.room_id.to_string(),
                    status: OpStatus::Incomplete,
                    steps,
                    last_error: None,
                });
                self.save(state).await?;
                self.drive(state, &id).await
            }
            _ => anyhow::bail!("unknown workspace command; see !chaz workspace help"),
        }
    }

    fn ensure_idle(&self, state: &WorkspaceState) -> anyhow::Result<()> {
        if let Some(op) = state.unfinished() {
            anyhow::bail!(
                "{} is unfinished ({}); use !chaz workspace retry {} or cancel {} first",
                op.id,
                status_word(op.status),
                op.id,
                op.id
            );
        }
        Ok(())
    }

    async fn setup_preview(
        &self,
        guard: &mut Option<WorkspaceState>,
        req: &Request<'_>,
        manager: &str,
    ) -> anyhow::Result<String> {
        if let Some(state) = guard.as_ref() {
            if state.setup_done() {
                return Ok(format!("The workspace already exists.\n{}", summary(state)));
            }
            if let Some(op) = state
                .operations
                .iter()
                .find(|o| o.kind == OpKind::Setup && o.status != OpStatus::Cancelled)
            {
                return Ok(describe(state, op));
            }
        }
        let version = self
            .matrix
            .default_room_version()
            .await
            .map_err(|e| anyhow::anyhow!("cannot read the homeserver's room version: {e}"))?;
        let Some(rule) = creator_rule(&version) else {
            anyhow::bail!(
                "the homeserver's default room version {version:?} is not recognized; \
                 setup is not offered"
            );
        };
        let state = match guard {
            Some(state) => state,
            None => guard.insert(WorkspaceState {
                workspace_id: new_workspace_id()?,
                login_id: self.login_id.clone(),
                owning_agent: self.owning_agent.clone(),
                agent_db_id: self.sessions.agent_id()?,
                bot_mxid: self.matrix.bot_id()?,
                manager: manager.to_string(),
                room_version: version.clone(),
                space: None,
                lobby: None,
                chats: Vec::new(),
                operations: Vec::new(),
                next_op: 0,
                next_chat: 0,
            }),
        };
        state.room_version = version.clone();
        let id = state.alloc_op();
        let steps = SETUP_STEPS
            .into_iter()
            .map(|s| (s.to_string(), Step::Pending))
            .collect();
        state.operations.push(Operation {
            id: id.clone(),
            kind: OpKind::Setup,
            requester: req.sender.to_string(),
            event_id: req.event_id.to_string(),
            origin_room: req.room_id.to_string(),
            status: OpStatus::Preview,
            steps,
            last_error: None,
        });
        self.save(state).await?;
        Ok(format!(
            "Proposed workspace (operation {id}):\n\
             - Bot {bot} on login {login}, owning agent {agent} ({agent_id}), manager {manager}.\n\
             - A private Space \u{201c}{SPACE_NAME}\u{201d} and a command-only \u{201c}{LOBBY_NAME}\u{201d} room.\n\
             - Lobby and chats: invite-only, encrypted, history visible to joined members only, \
             no guests, not in the room directory. Only {manager} is invited.\n\
             - The Space is invite-only and unlisted; its names and links are room state, \
             which Matrix does not end-to-end encrypt.\n\
             - Room version {version}: {rule}. {manager} gets power 100 in every room and is not a creator.\n\
             - No existing sessions or rooms are listed, moved or imported.\n\
             Reply `!chaz workspace confirm {id}` to create these rooms.",
            login = self.login_id,
            agent = self.owning_agent,
            bot = state.bot_mxid,
            agent_id = state.agent_db_id,
        ))
    }

    async fn cancel(&self, state: &mut WorkspaceState, id: &str) -> anyhow::Result<String> {
        let op = state
            .op(id)
            .ok_or_else(|| anyhow::anyhow!("no operation {id:?}"))?;
        anyhow::ensure!(
            matches!(
                op.status,
                OpStatus::Preview | OpStatus::Incomplete | OpStatus::Uncertain
            ),
            "{id} is {}; nothing to cancel",
            status_word(op.status)
        );
        if op.kind == OpKind::Setup && (state.space.is_some() || state.lobby.is_some()) {
            anyhow::bail!("{id} already created workspace rooms; retry it to finish setup");
        }
        if matches!(op.kind, OpKind::NewChat { .. })
            && state.chats.iter().any(|c| c.operation == id)
        {
            anyhow::bail!("{id} already created a chat room; retry it to finish provisioning");
        }
        if matches!(op.kind, OpKind::Rename { .. })
            && matches!(
                op.steps.get("matrix.name"),
                Some(Step::Attempted | Step::Done(_))
            )
        {
            anyhow::bail!(
                "{id} may have changed the room name; retry it to finish the session label"
            );
        }
        let op = state.operations.iter_mut().find(|o| o.id == id).unwrap();
        op.status = OpStatus::Cancelled;
        self.save(state).await?;
        Ok(format!(
            "Cancelled {id}. Nothing was deleted: any room it created stays as it is \
             and is not listed as ready."
        ))
    }

    /// Run an operation's pending steps in order, persisting after each.
    async fn drive(&self, state: &mut WorkspaceState, id: &str) -> anyhow::Result<String> {
        let result = self.run_steps(state, id).await;
        let op = state.operations.iter_mut().find(|o| o.id == id).unwrap();
        match result {
            Ok(()) => {
                op.status = OpStatus::Done;
                op.last_error = None;
            }
            Err(StepFailure::Uncertain(e)) => {
                op.status = OpStatus::Uncertain;
                op.last_error = Some(e);
            }
            Err(StepFailure::Failed(e)) => {
                op.status = OpStatus::Incomplete;
                op.last_error = Some(e);
            }
        }
        self.save(state).await?;
        Ok(describe(state, state.op(id).unwrap()))
    }

    async fn run_steps(&self, state: &mut WorkspaceState, id: &str) -> Result<(), StepFailure> {
        let kind = state.op(id).unwrap().kind.clone();
        let mut names: Vec<String> = state.op(id).unwrap().steps.keys().cloned().collect();
        // BTreeMap order is alphabetical; run in the declared order instead.
        names.sort_by_key(|n| step_order(&kind, n));
        for name in names {
            if matches!(state.op(id).unwrap().steps[&name], Step::Done(_)) {
                continue;
            }
            let value = self.run_step(state, id, &kind, &name).await?;
            set_step(state, id, &name, Step::Done(value));
            self.after_step(state, &kind, &name);
            self.save(state).await.map_err(StepFailure::failed)?;
        }
        if let OpKind::Rename { handle, label } = &kind {
            state.chat_mut(handle).unwrap().label = label.clone();
        }
        Ok(())
    }

    /// Record the effect of a completed step on the workspace record.
    fn after_step(&self, state: &mut WorkspaceState, kind: &OpKind, name: &str) {
        let OpKind::NewChat { handle, .. } = kind else {
            return;
        };
        if name == "delivery"
            && let Some(chat) = state.chat_mut(handle)
        {
            chat.ready = true;
        }
    }

    async fn run_step(
        &self,
        state: &mut WorkspaceState,
        id: &str,
        kind: &OpKind,
        name: &str,
    ) -> Result<Option<String>, StepFailure> {
        let wid = state.workspace_id.clone();
        let manager = state.manager.clone();
        let failed = |e: anyhow::Error| StepFailure::Failed(format!("{name}: {e}"));
        match (kind, name) {
            (OpKind::Setup, "space.create") => {
                let room = self
                    .create(state, id, name, Role::Space, SPACE_NAME, None)
                    .await?;
                state.space = Some(room.clone());
                Ok(Some(room))
            }
            (OpKind::Setup, "lobby.create") => {
                let room = self
                    .create(state, id, name, Role::Lobby, LOBBY_NAME, Some(LOBBY_TOPIC))
                    .await?;
                state.lobby = Some(room.clone());
                Ok(Some(room))
            }
            (OpKind::Setup, step) => {
                let (space, lobby) = (state.space.clone().unwrap(), state.lobby.clone().unwrap());
                match step {
                    "space.child" => self.matrix.link_child(&space, &lobby).await,
                    "lobby.parent" => self.matrix.link_parent(&lobby, &space).await,
                    "invite.space" => self.matrix.invite(&space, &manager).await,
                    "invite.lobby" => self.matrix.invite(&lobby, &manager).await,
                    _ => unreachable!("unknown setup step {step}"),
                }
                .map_err(failed)?;
                Ok(None)
            }
            (OpKind::NewChat { handle, label }, "room.create") => {
                let room = self
                    .create(state, id, name, Role::Chat, label, None)
                    .await?;
                state.chats.push(Chat {
                    handle: handle.clone(),
                    room_id: room.clone(),
                    session_db_id: None,
                    label: label.clone(),
                    ready: false,
                    operation: id.to_string(),
                });
                Ok(Some(room))
            }
            (OpKind::NewChat { handle, label }, step) => {
                let chat = state.chat(handle).unwrap().clone();
                let space = state.space.clone().unwrap();
                match step {
                    "session.provision" => {
                        let source = format!("matrix-workspace:{wid}:{id}");
                        let allow_create = state.op(id).unwrap().steps[name] == Step::Pending;
                        set_step(state, id, name, Step::Attempted);
                        self.save(state).await.map_err(StepFailure::failed)?;
                        let sid = self
                            .sessions
                            .provision(&chat.room_id, &source, allow_create)
                            .await
                            .map_err(failed)?;
                        state.chat_mut(handle).unwrap().session_db_id = Some(sid.clone());
                        return Ok(Some(sid));
                    }
                    "session.label" => {
                        self.sessions
                            .set_label(chat.session_db_id.as_deref().unwrap(), label)
                            .await
                    }
                    "delivery" => {
                        self.sessions
                            .install_delivery(&chat.room_id, chat.session_db_id.as_deref().unwrap())
                            .await
                    }
                    "space.child" => self.matrix.link_child(&space, &chat.room_id).await,
                    "room.parent" => self.matrix.link_parent(&chat.room_id, &space).await,
                    "invite" => self.matrix.invite(&chat.room_id, &manager).await,
                    _ => unreachable!("unknown chat step {step}"),
                }
                .map_err(failed)?;
                Ok(None)
            }
            (OpKind::Rename { handle, label }, step) => {
                let chat = state.chat(handle).unwrap().clone();
                self.sessions
                    .check_binding(&chat.room_id, chat.session_db_id.as_deref().unwrap())
                    .await
                    .map_err(failed)?;
                match step {
                    "matrix.name" => {
                        // A lost response or cancellation may follow a successful
                        // state write. Retain that uncertainty and require retry
                        // of the same literal name, not abandonment of half a rename.
                        set_step(state, id, name, Step::Attempted);
                        self.save(state).await.map_err(StepFailure::failed)?;
                        self.matrix.set_name(&chat.room_id, label).await
                    }
                    "session.label" => {
                        self.sessions
                            .set_label(chat.session_db_id.as_deref().unwrap(), label)
                            .await
                    }
                    _ => unreachable!("unknown rename step {step}"),
                }
                .map_err(failed)?;
                Ok(None)
            }
        }
    }

    /// Create (or reconcile) one room. The step is persisted as attempted
    /// before the POST, so an interrupted request reconciles instead of
    /// repeating.
    async fn create(
        &self,
        state: &mut WorkspaceState,
        id: &str,
        step: &str,
        role: Role,
        name: &str,
        topic: Option<&str>,
    ) -> Result<String, StepFailure> {
        let marker = Marker {
            workspace: state.workspace_id.clone(),
            operation: id.to_string(),
            role,
        };
        if state.op(id).unwrap().steps[step] == Step::Attempted {
            let found =
                self.matrix.find_marked_rooms(&marker).await.map_err(|e| {
                    StepFailure::Uncertain(format!("{step}: cannot check rooms: {e}"))
                })?;
            return match found.as_slice() {
                [room] => Ok(room.clone()),
                [] => Err(StepFailure::Uncertain(format!(
                    "{step}: the earlier create request has an unknown outcome and no room \
                     with its marker is visible yet; it will not be sent again"
                ))),
                rooms => Err(StepFailure::Uncertain(format!(
                    "{step}: {} rooms carry this operation's marker; refusing to choose",
                    rooms.len()
                ))),
            };
        }
        set_step(state, id, step, Step::Attempted);
        self.save(state).await.map_err(StepFailure::failed)?;
        let spec = RoomSpec {
            role,
            name: name.to_string(),
            topic: topic.map(str::to_string),
            room_version: state.room_version.clone(),
            manager: state.manager.clone(),
            marker,
        };
        match self.matrix.create_room(&spec).await {
            Ok(room) => Ok(room),
            Err(CreateError::Rejected(e)) => {
                set_step(state, id, step, Step::Pending);
                Err(StepFailure::Failed(format!(
                    "{step}: the homeserver refused: {e}"
                )))
            }
            Err(CreateError::Uncertain(e)) => Err(StepFailure::Uncertain(format!(
                "{step}: outcome unknown ({e}); it will not be sent again"
            ))),
        }
    }
}

enum StepFailure {
    Failed(String),
    Uncertain(String),
}

impl StepFailure {
    fn failed(e: anyhow::Error) -> Self {
        StepFailure::Failed(e.to_string())
    }
}

fn set_step(state: &mut WorkspaceState, id: &str, name: &str, step: Step) {
    let op = state.operations.iter_mut().find(|o| o.id == id).unwrap();
    op.steps.insert(name.to_string(), step);
}

fn step_order(kind: &OpKind, name: &str) -> usize {
    let order: &[&str] = match kind {
        OpKind::Setup => &SETUP_STEPS,
        OpKind::NewChat { .. } => &CHAT_STEPS,
        OpKind::Rename { .. } => &RENAME_STEPS,
    };
    order.iter().position(|n| *n == name).unwrap_or(usize::MAX)
}

fn valid_label(raw: &str) -> anyhow::Result<String> {
    chaz_core::session::validate_session_description(raw)
        .map_err(|e| anyhow::anyhow!("invalid chat name: {e}"))
}

fn status_word(status: OpStatus) -> &'static str {
    match status {
        OpStatus::Preview => "awaiting confirmation",
        OpStatus::Incomplete => "incomplete",
        OpStatus::Uncertain => "uncertain",
        OpStatus::Done => "done",
        OpStatus::Cancelled => "cancelled",
    }
}

fn step_label(name: &str) -> &'static str {
    match name {
        "space.create" => "Space created",
        "lobby.create" => "Lobby created",
        "space.child" => "listed in the Space",
        "lobby.parent" | "room.parent" => "linked back to the Space",
        "invite.space" => "invited to the Space",
        "invite.lobby" => "invited to the Lobby",
        "room.create" => "room created",
        "session.provision" => "session created and bound",
        "session.label" => "session label set",
        "delivery" => "reply delivery installed",
        "invite" => "invitation sent",
        "matrix.name" => "room name set",
        _ => "step",
    }
}

/// Exact state of one operation: what happened, what is pending, what next.
fn describe(state: &WorkspaceState, op: &Operation) -> String {
    let what = match &op.kind {
        OpKind::Setup => "workspace setup".to_string(),
        OpKind::NewChat { handle, label } => format!("new chat \u{201c}{label}\u{201d} ({handle})"),
        OpKind::Rename { handle, label } => format!("rename {handle} to \u{201c}{label}\u{201d}"),
    };
    let mut ordered: Vec<(&String, &Step)> = op.steps.iter().collect();
    ordered.sort_by_key(|(n, _)| step_order(&op.kind, n));
    let done: Vec<&str> = ordered
        .iter()
        .filter(|(_, s)| matches!(s, Step::Done(_)))
        .map(|(n, _)| step_label(n))
        .collect();
    let pending: Vec<&str> = ordered
        .iter()
        .filter(|(_, s)| !matches!(s, Step::Done(_)))
        .map(|(n, s)| {
            if **s == Step::Attempted && n.ends_with(".create") {
                "room creation (outcome unknown)"
            } else if **s == Step::Attempted && n.as_str() == "matrix.name" {
                "room name (outcome unknown)"
            } else {
                step_label(n)
            }
        })
        .collect();
    let mut text = format!("{}: {what} \u{2014} {}.", op.id, status_word(op.status));
    if !done.is_empty() {
        text.push_str(&format!("\nDone: {}.", done.join(", ")));
    }
    if !pending.is_empty() && op.status != OpStatus::Done {
        text.push_str(&format!("\nPending: {}.", pending.join(", ")));
    }
    if let Some(e) = &op.last_error
        && op.status != OpStatus::Done
    {
        text.push_str(&format!("\nLast error: {e}"));
    }
    match (&op.kind, op.status) {
        (OpKind::Setup, OpStatus::Done) => {
            if let (Some(space), Some(lobby)) = (&state.space, &state.lobby) {
                text.push_str(&format!(
                    "\nSpace: {}\nLobby: {}\nAccept both invitations; create chats from the Lobby.",
                    link(space),
                    link(lobby)
                ));
            }
        }
        (OpKind::NewChat { handle, .. }, OpStatus::Done) => {
            if let Some(chat) = state.chat(handle) {
                text.push_str(&format!(
                    "\nOpen chat: {}\nA new session is ready; no other chat history was copied.",
                    link(&chat.room_id)
                ));
            }
        }
        (OpKind::NewChat { handle, .. }, _) => {
            if let Some(chat) = state.chat(handle) {
                text.push_str(&format!(
                    "\nRoom {} exists; {}. No second room will be created.",
                    chat.room_id,
                    if chat.ready {
                        "its session is ready"
                    } else {
                        "it is not ready for chat"
                    }
                ));
            }
        }
        (OpKind::Rename { .. }, OpStatus::Done) => {
            text.push_str("\nRoom and session unchanged; only the label changed.")
        }
        _ => {}
    }
    match op.status {
        OpStatus::Preview => {
            text.push_str(&format!("\nConfirm: !chaz workspace confirm {}", op.id))
        }
        OpStatus::Incomplete => text.push_str(&format!(
            "\nContinue with !chaz workspace retry {} (only pending steps run).",
            op.id
        )),
        OpStatus::Uncertain => text.push_str(&format!(
            "\n!chaz workspace retry {} checks again for the room. If you are sure no room was \
             created, !chaz workspace cancel {} stops it.",
            op.id, op.id
        )),
        _ => {}
    }
    text
}

fn summary(state: &WorkspaceState) -> String {
    let ready = state.chats.iter().filter(|c| c.ready).count();
    let mut text = format!(
        "Workspace for {} (agent {}, manager {}), room version {}.\nSpace: {}\nLobby: {}\n\
         Chats: {ready} ready, {} not ready.",
        state.login_id,
        state.owning_agent,
        state.manager,
        state.room_version,
        state
            .space
            .as_deref()
            .map(link)
            .unwrap_or_else(|| "not created".into()),
        state
            .lobby
            .as_deref()
            .map(link)
            .unwrap_or_else(|| "not created".into()),
        state.chats.len() - ready,
    );
    if let Some(op) = state.unfinished() {
        text.push_str(&format!(
            "\nUnfinished: {} ({}).",
            op.id,
            status_word(op.status)
        ));
    }
    text
}

fn list(state: &WorkspaceState) -> String {
    if state.chats.is_empty() {
        return "No chats yet. Start one with !chaz chat new <name>.".to_string();
    }
    let mut text = String::from("Chats in this workspace:");
    for chat in &state.chats {
        text.push_str(&format!(
            "\n{} \u{201c}{}\u{201d} \u{2014} {}{}",
            chat.handle,
            chat.label,
            link(&chat.room_id),
            if chat.ready { "" } else { " (not ready)" }
        ));
    }
    text
}

#[cfg(test)]
mod tests;
