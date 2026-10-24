//! Hermetic workspace tests: a fake homeserver and fake session registry,
//! with the real persisted record in an in-memory Eidetica DB.

use super::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex as StdMutex;

use eidetica::backend::database::InMemory;
use eidetica::{Instance, NewAccount};

const MANAGER: &str = "@patrick:example.org";
const BOT: &str = "@chaz:example.org";
const LOGIN: &str = "@chaz:example.org";
const AGENT: &str = "chaz";

#[derive(Clone, Copy, Debug)]
enum Create {
    Ok,
    Reject,
    /// The room is created but the response is lost.
    LostResponse,
    /// The request fails without creating anything, ambiguously.
    LostRequest,
}

#[derive(Default)]
struct HomeServer {
    version: String,
    rooms: Vec<(String, RoomSpec)>,
    script: VecDeque<Create>,
    posts: usize,
    fail_once: HashSet<&'static str>,
    invites: Vec<(String, String)>,
    children: Vec<(String, String)>,
    parents: Vec<(String, String)>,
    names: HashMap<String, String>,
    /// Extra rooms that carry a marker (another copy, for ambiguity tests).
    foreign: Vec<(String, Marker)>,
}

#[derive(Clone, Default)]
struct FakeMatrix(Arc<StdMutex<HomeServer>>);

impl FakeMatrix {
    fn fail(&self, op: &'static str) -> anyhow::Result<()> {
        if self.0.lock().unwrap().fail_once.remove(op) {
            anyhow::bail!("injected {op} failure");
        }
        Ok(())
    }
}

impl MatrixOps for FakeMatrix {
    async fn default_room_version(&self) -> anyhow::Result<String> {
        Ok(self.0.lock().unwrap().version.clone())
    }
    async fn create_room(&self, spec: &RoomSpec) -> Result<String, CreateError> {
        let mut hs = self.0.lock().unwrap();
        hs.posts += 1;
        let id = format!("!room{}:example.org", hs.posts);
        match hs.script.pop_front().unwrap_or(Create::Ok) {
            Create::Ok => {
                hs.rooms.push((id.clone(), spec.clone()));
                Ok(id)
            }
            Create::Reject => Err(CreateError::Rejected("M_FORBIDDEN".into())),
            Create::LostResponse => {
                hs.rooms.push((id, spec.clone()));
                Err(CreateError::Uncertain("connection reset".into()))
            }
            Create::LostRequest => Err(CreateError::Uncertain("timed out".into())),
        }
    }
    async fn find_marked_rooms(&self, marker: &Marker) -> anyhow::Result<Vec<String>> {
        let hs = self.0.lock().unwrap();
        Ok(hs
            .rooms
            .iter()
            .filter(|(_, s)| &s.marker == marker)
            .map(|(id, _)| id.clone())
            .chain(
                hs.foreign
                    .iter()
                    .filter(|(_, m)| m == marker)
                    .map(|(id, _)| id.clone()),
            )
            .collect())
    }
    async fn invite(&self, room: &str, user: &str) -> anyhow::Result<()> {
        self.fail("invite")?;
        let mut hs = self.0.lock().unwrap();
        let entry = (room.to_string(), user.to_string());
        if !hs.invites.contains(&entry) {
            hs.invites.push(entry);
        }
        Ok(())
    }
    async fn link_child(&self, space: &str, child: &str) -> anyhow::Result<()> {
        self.fail("link_child")?;
        let mut hs = self.0.lock().unwrap();
        hs.children.push((space.into(), child.into()));
        Ok(())
    }
    async fn link_parent(&self, child: &str, space: &str) -> anyhow::Result<()> {
        self.fail("link_parent")?;
        let mut hs = self.0.lock().unwrap();
        hs.parents.push((child.into(), space.into()));
        Ok(())
    }
    async fn set_name(&self, room: &str, name: &str) -> anyhow::Result<()> {
        self.fail("set_name")?;
        let mut hs = self.0.lock().unwrap();
        *hs.names.entry(room.into()).or_default() = name.into();
        Ok(())
    }
}

#[derive(Default)]
struct Registry {
    by_source: HashMap<String, String>,
    bindings: HashMap<String, String>,
    labels: HashMap<String, String>,
    delivery: HashSet<(String, String)>,
    creates: usize,
    fail_once: HashSet<&'static str>,
}

#[derive(Clone, Default)]
struct FakeSessions(Arc<StdMutex<Registry>>);

impl FakeSessions {
    fn fail(&self, op: &'static str) -> anyhow::Result<()> {
        if self.0.lock().unwrap().fail_once.remove(op) {
            anyhow::bail!("injected {op} failure");
        }
        Ok(())
    }
}

impl SessionOps for FakeSessions {
    async fn provision(&self, room: &str, source: &str) -> anyhow::Result<String> {
        self.fail("provision")?;
        let mut reg = self.0.lock().unwrap();
        let sid = match reg.by_source.get(source) {
            Some(sid) => sid.clone(),
            None => {
                reg.creates += 1;
                let sid = format!("session-{}", reg.creates);
                reg.by_source.insert(source.into(), sid.clone());
                sid
            }
        };
        if let Some(bound) = reg.bindings.get(room) {
            anyhow::ensure!(bound == &sid, "room already bound elsewhere");
        }
        reg.bindings.insert(room.into(), sid.clone());
        Ok(sid)
    }
    async fn install_delivery(&self, room: &str, session: &str) -> anyhow::Result<()> {
        self.fail("delivery")?;
        let mut reg = self.0.lock().unwrap();
        anyhow::ensure!(reg.bindings.get(room).map(String::as_str) == Some(session));
        reg.delivery.insert((room.into(), session.into()));
        Ok(())
    }
    async fn set_label(&self, session: &str, label: &str) -> anyhow::Result<()> {
        self.fail("label")?;
        self.0
            .lock()
            .unwrap()
            .labels
            .insert(session.into(), label.into());
        Ok(())
    }
}

struct Harness {
    _instance: Instance,
    store: WorkspaceStore,
    hs: FakeMatrix,
    reg: FakeSessions,
    ws: Workspace<FakeMatrix, FakeSessions>,
    events: std::sync::atomic::AtomicUsize,
}

async fn harness(version: &str) -> Harness {
    let (instance, mut user) =
        Instance::create_backend(Box::new(InMemory::new()), NewAccount::passwordless("t"))
            .await
            .unwrap();
    let mut settings = eidetica::crdt::Doc::new();
    settings.set("name", "chaz_peer");
    let key = user.get_default_key().unwrap();
    let db = user.create_database(settings, &key).await.unwrap();
    let store = WorkspaceStore::new(db, LOGIN);
    let hs = FakeMatrix::default();
    hs.0.lock().unwrap().version = version.into();
    let reg = FakeSessions::default();
    let ws = open(&store, &hs, &reg, AGENT, Some(MANAGER)).await.unwrap();
    Harness {
        _instance: instance,
        store,
        hs,
        reg,
        ws,
        events: Default::default(),
    }
}

async fn open(
    store: &WorkspaceStore,
    hs: &FakeMatrix,
    reg: &FakeSessions,
    agent: &str,
    manager: Option<&str>,
) -> anyhow::Result<Workspace<FakeMatrix, FakeSessions>> {
    Workspace::open(
        store.clone(),
        hs.clone(),
        reg.clone(),
        LOGIN.into(),
        agent.into(),
        manager.map(str::to_string),
    )
    .await
}

impl Harness {
    fn next_event(&self) -> String {
        let n = self
            .events
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("$event{n}")
    }

    async fn send_as(&self, sender: &str, origin: Origin, command: &str) -> String {
        let event = self.next_event();
        self.send_event(sender, origin, command, &event).await
    }

    async fn send_event(&self, sender: &str, origin: Origin, command: &str, event: &str) -> String {
        self.ws
            .handle(Request {
                sender,
                event_id: event,
                room_id: "!dm:example.org",
                origin,
                command,
            })
            .await
            .unwrap()
    }

    async fn send(&self, origin: Origin, command: &str) -> String {
        self.send_as(MANAGER, origin, command).await
    }

    async fn lobby(&self, command: &str) -> String {
        self.send(Origin::Managed(Place::Lobby), command).await
    }

    async fn state(&self) -> WorkspaceState {
        self.store.load().await.unwrap().unwrap()
    }

    async fn setup(&self) -> WorkspaceState {
        let preview = self.send(Origin::Direct, "workspace setup").await;
        assert!(preview.contains("workspace confirm op-1"), "{preview}");
        let done = self.send(Origin::Direct, "workspace confirm op-1").await;
        assert!(
            done.contains("op-1: workspace setup \u{2014} done"),
            "{done}"
        );
        self.state().await
    }

    fn posts(&self) -> usize {
        self.hs.0.lock().unwrap().posts
    }
}

#[tokio::test]
async fn setup_previews_then_creates_a_private_space_and_lobby() {
    let h = harness("12").await;
    let preview = h.send(Origin::Direct, "workspace setup").await;
    for expected in [
        "Bot login @chaz:example.org, owning agent chaz, manager @patrick:example.org",
        "Room version 12",
        "permanent, unlimited power",
        "not end-to-end encrypt",
        "No existing sessions or rooms are listed",
    ] {
        assert!(
            preview.contains(expected),
            "missing {expected:?}: {preview}"
        );
    }
    assert_eq!(h.posts(), 0, "a preview creates nothing");
    // Asking again returns the same pending preview, not a second one.
    let again = h.send(Origin::Direct, "workspace setup").await;
    assert!(again.starts_with("op-1:"), "{again}");

    let state = h.setup().await;
    let (space, lobby) = (state.space.clone().unwrap(), state.lobby.clone().unwrap());
    {
        let hs = h.hs.0.lock().unwrap();
        let roles: Vec<Role> = hs.rooms.iter().map(|(_, s)| s.role).collect();
        assert_eq!(roles, vec![Role::Space, Role::Lobby]);
        assert!(hs.rooms.iter().all(|(_, s)| s.room_version == "12"));
        assert_eq!(hs.children, vec![(space.clone(), lobby.clone())]);
        assert_eq!(hs.parents, vec![(lobby.clone(), space.clone())]);
        assert_eq!(
            hs.invites,
            vec![
                (space.clone(), MANAGER.into()),
                (lobby.clone(), MANAGER.into())
            ],
            "only the manager is invited"
        );
    }
    assert_eq!(h.ws.index().get(&space), Some(Place::Space));
    assert_eq!(h.ws.index().get(&lobby), Some(Place::Lobby));
    assert_eq!(
        h.reg.0.lock().unwrap().creates,
        0,
        "no session for Space or Lobby"
    );
    let again = h.send(Origin::Direct, "workspace setup").await;
    assert!(again.contains("already exists"), "{again}");
}

#[tokio::test]
async fn unrecognized_room_version_refuses_setup() {
    let h = harness("org.example.custom").await;
    let reply = h.send(Origin::Direct, "workspace setup").await;
    assert!(reply.contains("not recognized"), "{reply}");
    assert!(h.store.load().await.unwrap().is_none());
}

#[tokio::test]
async fn unauthorized_or_misplaced_commands_change_nothing() {
    let h = harness("12").await;
    let other = "@friend:example.org";
    let reply = h.send_as(other, Origin::Direct, "workspace setup").await;
    assert!(
        reply.contains("limited to the configured manager"),
        "{reply}"
    );
    let reply = h.send(Origin::Other, "workspace setup").await;
    assert!(reply.contains("direct chat"), "{reply}");
    assert!(h.store.load().await.unwrap().is_none());

    let before = h.setup().await;
    let posts = h.posts();
    for (sender, origin, cmd) in [
        (other, Origin::Managed(Place::Lobby), "chat new Stolen"),
        (MANAGER, Origin::Direct, "chat new Not here"),
        (MANAGER, Origin::Other, "chat new Not here"),
        (MANAGER, Origin::Other, "workspace status"),
        (
            MANAGER,
            Origin::Managed(Place::Lobby),
            "workspace confirm op-1",
        ),
        (MANAGER, Origin::Managed(Place::Lobby), "chat new"),
        (
            MANAGER,
            Origin::Managed(Place::Lobby),
            "chat new two\u{7}lines",
        ),
    ] {
        let reply = h.send_as(sender, origin, cmd).await;
        assert!(reply.starts_with("!chaz Error:"), "{cmd}: {reply}");
    }
    assert_eq!(h.posts(), posts);
    assert_eq!(h.state().await, before);
    // The Space never replies, even to the manager.
    let none =
        h.ws.handle(Request {
            sender: MANAGER,
            event_id: "$space",
            room_id: "!space",
            origin: Origin::Managed(Place::Space),
            command: "chat list",
        })
        .await;
    assert_eq!(none, None);

    // No manager configured: everything except help refuses.
    let ws = open(&h.store, &h.hs, &h.reg, AGENT, None).await.unwrap();
    let reply = ws
        .handle(Request {
            sender: MANAGER,
            event_id: "$x",
            room_id: "!lobby",
            origin: Origin::Managed(Place::Lobby),
            command: "chat list",
        })
        .await
        .unwrap();
    assert!(reply.contains("not configured"), "{reply}");
}

#[tokio::test]
async fn a_record_for_another_identity_or_a_malformed_record_refuses() {
    let h = harness("12").await;
    h.setup().await;
    let ws = open(&h.store, &h.hs, &h.reg, "other-agent", Some(MANAGER))
        .await
        .unwrap();
    let reply = ws
        .handle(Request {
            sender: MANAGER,
            event_id: "$x",
            room_id: "!lobby",
            origin: Origin::Managed(Place::Lobby),
            command: "chat new Hi",
        })
        .await
        .unwrap();
    assert!(reply.contains("refusing"), "{reply}");
    let ws = open(&h.store, &h.hs, &h.reg, AGENT, Some("@else:example.org"))
        .await
        .unwrap();
    let reply = ws
        .handle(Request {
            sender: "@else:example.org",
            event_id: "$y",
            room_id: "!lobby",
            origin: Origin::Managed(Place::Lobby),
            command: "chat list",
        })
        .await
        .unwrap();
    assert!(reply.contains("refusing"), "{reply}");

    let txn = h.store.db.new_transaction().await.unwrap();
    txn.get_store::<DocStore>(STORE)
        .await
        .unwrap()
        .set_string(&h.store.key, "{not json")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let err = open(&h.store, &h.hs, &h.reg, AGENT, Some(MANAGER))
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("malformed"), "{err}");
}

#[tokio::test]
async fn each_new_chat_gets_its_own_room_and_session() {
    let h = harness("12").await;
    let state = h.setup().await;
    let space = state.space.unwrap();
    let a = h.lobby("chat new Bike repair").await;
    assert!(
        a.contains("op-2: new chat \u{201c}Bike repair\u{201d} (c1) \u{2014} done"),
        "{a}"
    );
    assert!(a.contains("Open chat: https://matrix.to/#/"), "{a}");
    let b = h.lobby("chat new Trip plans").await;
    assert!(b.contains("(c2) \u{2014} done"), "{b}");

    let state = h.state().await;
    assert_eq!(state.chats.len(), 2);
    let (c1, c2) = (&state.chats[0], &state.chats[1]);
    assert_ne!(c1.room_id, c2.room_id);
    assert_ne!(c1.session_db_id, c2.session_db_id);
    assert!(c1.ready && c2.ready);
    {
        let reg = h.reg.0.lock().unwrap();
        assert_eq!(reg.creates, 2);
        assert_eq!(
            reg.bindings[&c1.room_id],
            *c1.session_db_id.as_ref().unwrap()
        );
        assert_eq!(
            reg.labels[c1.session_db_id.as_ref().unwrap()],
            "Bike repair"
        );
        assert!(
            reg.delivery
                .contains(&(c1.room_id.clone(), c1.session_db_id.clone().unwrap()))
        );
    }
    {
        let hs = h.hs.0.lock().unwrap();
        let chat_spec = &hs.rooms.iter().find(|(id, _)| id == &c1.room_id).unwrap().1;
        assert_eq!(chat_spec.role, Role::Chat);
        assert_eq!(chat_spec.name, "Bike repair");
        assert!(hs.children.contains(&(space.clone(), c1.room_id.clone())));
        assert!(hs.invites.contains(&(c1.room_id.clone(), MANAGER.into())));
    }
    assert_eq!(
        h.ws.index().get(&c1.room_id),
        Some(Place::Chat {
            handle: "c1".into(),
            ready: true
        })
    );
    // The directory lists exactly the workspace's chats.
    let list = h.lobby("chat list").await;
    assert!(
        list.contains("c1 \u{201c}Bike repair\u{201d}") && list.contains("c2"),
        "{list}"
    );
    assert_eq!(list.lines().count(), 3, "{list}");
}

#[tokio::test]
async fn repeated_and_concurrent_delivery_of_one_request_creates_one_chat() {
    let h = harness("12").await;
    h.setup().await;
    let origin = Origin::Managed(Place::Lobby);
    let (a, b) = tokio::join!(
        h.send_event(MANAGER, origin.clone(), "chat new Once", "$same"),
        h.send_event(MANAGER, origin.clone(), "chat new Once", "$same"),
    );
    assert_eq!(a, b);
    let c = h
        .send_event(MANAGER, origin, "chat new Once", "$same")
        .await;
    assert_eq!(a, c);
    assert_eq!(h.state().await.chats.len(), 1);
    assert_eq!(h.posts(), 3, "Space, Lobby, one chat");
    assert_eq!(h.reg.0.lock().unwrap().creates, 1);
}

#[tokio::test]
async fn a_lost_create_response_is_reconciled_never_reposted() {
    let h = harness("12").await;
    h.setup().await;
    h.hs.0
        .lock()
        .unwrap()
        .script
        .push_back(Create::LostResponse);
    let reply = h.lobby("chat new Bike").await;
    assert!(
        reply.contains("uncertain") && reply.contains("will not be sent again"),
        "{reply}"
    );
    let state = h.state().await;
    assert!(state.chats.is_empty(), "no room ID is known yet");
    assert_eq!(
        state.op("op-2").unwrap().steps["room.create"],
        Step::Attempted
    );
    // Other management waits for the uncertain operation.
    let busy = h.lobby("chat new Other").await;
    assert!(busy.contains("op-2 is unfinished"), "{busy}");

    // Restart: a fresh engine reloads the record and reconciles by marker.
    let posts = h.posts();
    let ws = open(&h.store, &h.hs, &h.reg, AGENT, Some(MANAGER))
        .await
        .unwrap();
    let reply = ws
        .handle(Request {
            sender: MANAGER,
            event_id: "$retry",
            room_id: "!lobby",
            origin: Origin::Managed(Place::Lobby),
            command: "workspace retry op-2",
        })
        .await
        .unwrap();
    assert!(reply.contains("done"), "{reply}");
    assert_eq!(h.posts(), posts, "the create was not sent again");
    let state = h.state().await;
    assert_eq!(state.chats.len(), 1);
    assert!(state.chats[0].ready);
}

#[tokio::test]
async fn an_invisible_or_ambiguous_create_stays_held_until_cancelled() {
    let h = harness("12").await;
    h.setup().await;
    h.hs.0.lock().unwrap().script.push_back(Create::LostRequest);
    h.lobby("chat new Bike").await;
    let posts = h.posts();
    for _ in 0..2 {
        let reply = h.lobby("workspace retry op-2").await;
        assert!(
            reply.contains("no room with its marker is visible"),
            "{reply}"
        );
    }
    assert_eq!(h.posts(), posts);

    // Two rooms with this operation's marker: refuse to choose.
    let marker = Marker {
        workspace: h.state().await.workspace_id,
        operation: "op-2".into(),
        role: Role::Chat,
    };
    {
        let mut hs = h.hs.0.lock().unwrap();
        hs.foreign.push(("!a:example.org".into(), marker.clone()));
        hs.foreign.push(("!b:example.org".into(), marker));
    }
    let reply = h.lobby("workspace retry op-2").await;
    assert!(
        reply.contains("2 rooms carry this operation's marker"),
        "{reply}"
    );
    assert!(h.state().await.chats.is_empty());

    let reply = h.lobby("workspace cancel op-2").await;
    assert!(reply.contains("Nothing was deleted"), "{reply}");
    let reply = h.lobby("chat new Bike").await;
    assert!(reply.contains("op-3") && reply.contains("done"), "{reply}");
    assert_eq!(h.posts(), posts + 1);
}

#[tokio::test]
async fn a_rejected_create_can_be_retried_explicitly() {
    let h = harness("12").await;
    h.setup().await;
    h.hs.0.lock().unwrap().script.push_back(Create::Reject);
    let reply = h.lobby("chat new Bike").await;
    assert!(
        reply.contains("incomplete") && reply.contains("refused"),
        "{reply}"
    );
    let reply = h.lobby("workspace retry op-2").await;
    assert!(reply.contains("done"), "{reply}");
    assert_eq!(h.state().await.chats.len(), 1);
}

#[tokio::test]
async fn partial_provisioning_reports_state_and_resumes_only_pending_steps() {
    let h = harness("12").await;
    h.setup().await;
    h.reg.0.lock().unwrap().fail_once.insert("provision");
    let reply = h.lobby("chat new Bike").await;
    assert!(reply.contains("incomplete"), "{reply}");
    assert!(reply.contains("Done: room created."), "{reply}");
    assert!(reply.contains("it is not ready for chat"), "{reply}");
    let state = h.state().await;
    let room = state.chats[0].room_id.clone();
    assert_eq!(
        h.ws.index().get(&room),
        Some(Place::Chat {
            handle: "c1".into(),
            ready: false
        })
    );
    assert!(
        !h.hs
            .0
            .lock()
            .unwrap()
            .invites
            .iter()
            .any(|(r, _)| r == &room),
        "nobody is invited before the session is ready"
    );

    h.hs.0.lock().unwrap().fail_once.insert("invite");
    let reply = h.lobby("workspace retry op-2").await;
    assert!(reply.contains("its session is ready"), "{reply}");
    assert!(reply.contains("Pending: invitation sent."), "{reply}");
    let posts = h.posts();
    let reply = h.lobby("workspace retry op-2").await;
    assert!(reply.contains("done"), "{reply}");
    assert_eq!(h.posts(), posts);
    assert_eq!(h.reg.0.lock().unwrap().creates, 1);
    let hs = h.hs.0.lock().unwrap();
    assert_eq!(hs.children.iter().filter(|(_, c)| c == &room).count(), 1);
}

#[tokio::test]
async fn setup_with_a_failed_link_finishes_on_retry_and_cannot_be_cancelled() {
    let h = harness("11").await;
    h.hs.0.lock().unwrap().fail_once.insert("link_parent");
    h.send(Origin::Direct, "workspace setup").await;
    let reply = h.send(Origin::Direct, "workspace confirm op-1").await;
    assert!(
        reply.contains("incomplete") && reply.contains("Pending: linked back"),
        "{reply}"
    );
    let reply = h.send(Origin::Direct, "workspace cancel op-1").await;
    assert!(reply.contains("retry it to finish"), "{reply}");
    let reply = h.send(Origin::Direct, "workspace retry op-1").await;
    assert!(reply.contains("done"), "{reply}");
    assert_eq!(h.posts(), 2);
}

#[tokio::test]
async fn rename_is_literal_and_reports_partial_success() {
    let h = harness("12").await;
    h.setup().await;
    h.lobby("chat new Bike repair").await;
    h.reg.0.lock().unwrap().fail_once.insert("label");
    let chat = Origin::Managed(Place::Chat {
        handle: "c1".into(),
        ready: true,
    });
    let reply = h.send(chat.clone(), "chat rename Winter bike").await;
    assert!(reply.contains("Done: room name set."), "{reply}");
    assert!(reply.contains("Pending: session label set."), "{reply}");
    let state = h.state().await;
    let room = state.chats[0].room_id.clone();
    assert_eq!(
        state.chats[0].label, "Bike repair",
        "record waits for completion"
    );
    assert_eq!(h.hs.0.lock().unwrap().names[&room], "Winter bike");

    let reply = h.lobby("workspace retry op-3").await;
    assert!(reply.contains("only the label changed"), "{reply}");
    let state = h.state().await;
    assert_eq!(state.chats[0].label, "Winter bike");
    assert_eq!(state.chats[0].room_id, room);
    let sid = state.chats[0].session_db_id.clone().unwrap();
    assert_eq!(h.reg.0.lock().unwrap().labels[&sid], "Winter bike");

    // Lobby form names the chat by handle.
    let reply = h.lobby("chat rename c1 Spring bike").await;
    assert!(reply.contains("done"), "{reply}");
    let reply = h.lobby("chat rename c9 Nope").await;
    assert!(reply.contains("no workspace chat"), "{reply}");
    assert_eq!(h.state().await.chats[0].label, "Spring bike");
}

#[test]
fn routing_keeps_workspace_rooms_out_of_chat_dispatch() {
    let lobby = Place::Lobby;
    let pending = Place::Chat {
        handle: "c1".into(),
        ready: false,
    };
    let ready = Place::Chat {
        handle: "c1".into(),
        ready: true,
    };
    assert_eq!(route(Some(&Place::Space), true, None), Route::Ignore);
    assert_eq!(
        route(Some(&Place::Space), true, Some("chat list")),
        Route::Ignore
    );
    assert_eq!(
        route(None, true, None),
        Route::Ignore,
        "unlisted marked room"
    );
    assert_eq!(route(None, true, Some("workspace status")), Route::Ignore);
    assert_eq!(route(Some(&lobby), true, None), Route::Notice(LOBBY_ONLY));
    assert_eq!(
        route(Some(&lobby), true, Some("info")),
        Route::Notice(LOBBY_ONLY)
    );
    assert_eq!(
        route(Some(&lobby), true, Some("chat new X")),
        Route::Workspace
    );
    assert_eq!(route(Some(&pending), true, None), Route::Notice(NOT_READY));
    assert_eq!(
        route(Some(&pending), true, Some("model x")),
        Route::Notice(NOT_READY)
    );
    assert_eq!(route(Some(&ready), true, None), Route::Normal);
    assert_eq!(route(Some(&ready), true, Some("info")), Route::Normal);
    assert_eq!(
        route(Some(&ready), true, Some("attach s")),
        Route::Notice(FIXED_BINDING)
    );
    assert_eq!(
        route(Some(&ready), true, Some("detach")),
        Route::Notice(FIXED_BINDING)
    );
    assert_eq!(
        route(Some(&ready), true, Some("rename")),
        Route::Notice(LITERAL_RENAME)
    );
    assert_eq!(
        route(Some(&ready), true, Some("chat rename Y")),
        Route::Workspace
    );
    assert_eq!(route(None, false, None), Route::Normal);
    assert_eq!(route(None, false, Some("attach s")), Route::Normal);
    assert_eq!(
        route(None, false, Some("workspace setup")),
        Route::Workspace
    );
}

#[test]
fn created_rooms_are_private_encrypted_and_invite_nobody() {
    let spec = |role, version: &str| RoomSpec {
        role,
        name: "Bike".into(),
        topic: None,
        room_version: version.into(),
        manager: MANAGER.into(),
        marker: Marker {
            workspace: "ws".into(),
            operation: "op-2".into(),
            role,
        },
    };
    let state_of = |req: &matrix_sdk::ruma::api::client::room::create_room::v3::Request| {
        req.initial_state
            .iter()
            .map(|raw| serde_json::from_str::<serde_json::Value>(raw.json().get()).unwrap())
            .map(|v| {
                (
                    v["type"].as_str().unwrap().to_string(),
                    v["content"].clone(),
                )
            })
            .collect::<HashMap<_, _>>()
    };
    for (role, version) in [(Role::Chat, "12"), (Role::Lobby, "11"), (Role::Space, "12")] {
        let req = live::create_request(&spec(role, version), BOT).unwrap();
        assert!(req.invite.is_empty(), "invitations are a later step");
        assert!(!req.is_direct);
        assert_eq!(
            req.visibility,
            matrix_sdk::ruma::api::client::room::Visibility::Private
        );
        assert_eq!(req.room_version.as_ref().unwrap().as_str(), version);
        let state = state_of(&req);
        assert_eq!(state["m.room.join_rules"]["join_rule"], "invite");
        assert_eq!(
            state["m.room.history_visibility"]["history_visibility"],
            "joined"
        );
        assert_eq!(state["m.room.guest_access"]["guest_access"], "forbidden");
        assert_eq!(state[MARKER_TYPE]["operation"], "op-2");
        assert_eq!(state[MARKER_TYPE]["role"], role.as_str());
        assert_eq!(
            state.contains_key("m.room.encryption"),
            role != Role::Space,
            "{role:?}"
        );
        let power: serde_json::Value = serde_json::from_str(
            req.power_level_content_override
                .as_ref()
                .unwrap()
                .json()
                .get(),
        )
        .unwrap();
        assert_eq!(power["users"][MANAGER], 100);
        assert_eq!(
            power["users"].get(BOT).is_some(),
            version == "11",
            "v12 creators must not be listed"
        );
        let creation = req
            .creation_content
            .as_ref()
            .map(|raw| serde_json::from_str::<serde_json::Value>(raw.json().get()).unwrap());
        assert_eq!(
            creation.map(|c| c["type"].clone()),
            (role == Role::Space).then(|| serde_json::json!("m.space"))
        );
    }
}
