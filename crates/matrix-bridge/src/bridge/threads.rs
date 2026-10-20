//! Matrix thread/session addressing: the per-room mode, the composite channel
//! key that gives one Matrix thread its own durable session, and the outbound
//! target that keeps a reply inside the thread it belongs to.
//!
//! # Mental model
//!
//! chaz-core binds a session to a `(transport, login_id, channel)` triple and
//! treats `channel` as an opaque string (see
//! [`chaz_core::session::bind_transport`]). A `normal` room uses the room ID
//! verbatim as that channel, which is what every existing install already has
//! on disk. A `threaded-sessions` room instead keys one session per Matrix
//! thread, using the **room ID plus the thread's root event ID** as the channel
//! — so the durable session identity is `(login, room, root event)`, not
//! in-memory event bookkeeping that a restart would lose.
//!
//! Nothing Matrix-specific leaks into chaz-core: the composite is encoded and
//! decoded here, and the room ID remains the transport destination while the
//! thread root travels alongside it in [`RoomTarget`].

use matrix_sdk::Room;
use matrix_sdk::ruma::events::relation::Thread;
use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContent};
use matrix_sdk::ruma::{EventId, OwnedEventId, TransactionId};

use chaz_core::bridge_db::BridgeDb;

/// How one Matrix room routes inbound messages. Durable per `(login, room)`,
/// chosen in-room with `!chaz room mode`, and defaulting to [`Self::Normal`] so
/// every pre-existing room keeps its previous behavior untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RoomMode {
    /// One room-wide session, replies on the main timeline. The historical
    /// behavior and the default.
    #[default]
    Normal,
    /// One session per top-level message, each answered in that message's
    /// Matrix thread. No room-wide session is used for new traffic.
    ThreadedSessions,
}

impl RoomMode {
    /// The wire/storage spelling, which is also the command spelling.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RoomMode::Normal => "normal",
            RoomMode::ThreadedSessions => "threaded-sessions",
        }
    }

    /// Parse a command or stored mode; reject modes this build does not support.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "normal" => Some(RoomMode::Normal),
            "threaded-sessions" => Some(RoomMode::ThreadedSessions),
            _ => None,
        }
    }
}

/// Settings key for a room's mode, under the bridge DB's unencrypted settings
/// store. JSON-encoded because MXIDs and room IDs both contain `:`, so a
/// `:`-joined key would be ambiguous between logins and rooms. Only ever
/// constructed, never parsed.
fn room_mode_key(login_id: &str, room_id: &str) -> String {
    serde_json::to_string(&("room_mode", login_id, room_id))
        .expect("serializing a tuple of &str is infallible")
}

/// An unreadable setting must not silently route a live thread onto the timeline.
pub(crate) async fn read_room_mode(
    bridge_db: &BridgeDb,
    login_id: &str,
    room_id: &str,
) -> anyhow::Result<RoomMode> {
    match bridge_db
        .get_setting(&room_mode_key(login_id, room_id))
        .await?
    {
        None => Ok(RoomMode::Normal),
        Some(raw) => {
            RoomMode::parse(&raw).ok_or_else(|| anyhow::anyhow!("unknown stored room mode: {raw}"))
        }
    }
}

/// Persist a room's mode for this login.
pub(crate) async fn write_room_mode(
    bridge_db: &BridgeDb,
    login_id: &str,
    room_id: &str,
    mode: RoomMode,
) -> anyhow::Result<()> {
    bridge_db
        .set_setting(&room_mode_key(login_id, room_id), mode.as_str())
        .await
}

/// Encode the Matrix root inside the transport's opaque channel field.
/// The prefix distinguishes threads from bare room IDs, whose opaque localpart
/// may also contain `#` and `$`. Escape the separator in both components.
pub(crate) fn thread_channel(room_id: &str, root: &EventId) -> String {
    let escape = |raw: &str| raw.replace('%', "%25").replace('#', "%23");
    format!("thread:{}#{}", escape(room_id), escape(root.as_str()))
}

pub(crate) fn parse_thread_channel(channel: &str) -> Option<(String, OwnedEventId)> {
    let (room, root) = channel.strip_prefix("thread:")?.split_once('#')?;
    let unescape = |raw: &str| raw.replace("%23", "#").replace("%25", "%");
    let room = unescape(room);
    matrix_sdk::ruma::RoomId::parse(&room).ok()?;
    Some((room, EventId::parse(unescape(root)).ok()?))
}

/// Whether deciding an inbound event's route needs a registry lookup for "does
/// this thread already have a session".
///
/// Only a `normal` room does: there, a thread reply is continued if and only if
/// the bridge already manages that thread — a conversation opened before the
/// room was switched back. A `threaded-sessions` room keys every thread reply on
/// its root unconditionally, so it needs no lookup at all.
pub(crate) fn needs_managed_lookup(mode: RoomMode, inbound_thread_root: Option<&EventId>) -> bool {
    mode == RoomMode::Normal && inbound_thread_root.is_some()
}

/// The thread whose session this event *continues*, or `None` when it is not
/// inside a thread the bridge routes.
///
/// Applies to chat and commands alike — a session command sent in a managed
/// thread acts on that thread's session and answers there. A thread reply in a
/// `threaded-sessions` room continues its root unconditionally: `get_or_create`
/// is idempotent per channel, and demanding proof of an existing session first
/// would misroute a reply that raced its own root's session creation.
///
/// `thread_is_managed` is only consulted when [`needs_managed_lookup`] says so;
/// callers may pass `false` otherwise.
pub(crate) fn continued_thread_root(
    mode: RoomMode,
    inbound_thread_root: Option<&EventId>,
    thread_is_managed: bool,
) -> Option<OwnedEventId> {
    match inbound_thread_root {
        Some(root) if mode == RoomMode::ThreadedSessions || thread_is_managed => {
            Some(root.to_owned())
        }
        _ => None,
    }
}

/// The thread a top-level message *opens*, when a `threaded-sessions` room sees
/// one: rooted at that very event.
///
/// Only ever called for a non-command message. A top-level command manages or
/// queries the room and must not silently open a thread session, which is why
/// this is separate from [`continued_thread_root`] rather than one decision.
pub(crate) fn opened_thread_root(
    mode: RoomMode,
    inbound_thread_root: Option<&EventId>,
    event_id: &EventId,
) -> Option<OwnedEventId> {
    (mode == RoomMode::ThreadedSessions && inbound_thread_root.is_none())
        .then(|| event_id.to_owned())
}

/// Where an outbound Matrix event goes: always a room, optionally inside one of
/// its threads.
///
/// Every reply path the bridge owns — agent responses, command output, approval
/// prompts and their acknowledgements, expiry notices — sends through this, so a
/// thread's traffic cannot leak onto the main timeline by a caller forgetting to
/// thread one of them.
#[derive(Clone)]
pub(crate) struct RoomTarget {
    room: Room,
    thread_root: Option<OwnedEventId>,
}

impl RoomTarget {
    /// The room's main timeline — an ordinary room, or a threaded room's
    /// top-level.
    pub(crate) fn timeline(room: Room) -> Self {
        Self {
            room,
            thread_root: None,
        }
    }

    /// Inside the thread rooted at `root`.
    pub(crate) fn thread(room: Room, root: OwnedEventId) -> Self {
        Self {
            room,
            thread_root: Some(root),
        }
    }

    pub(crate) fn room(&self) -> &Room {
        &self.room
    }

    pub(crate) fn thread_root(&self) -> Option<&EventId> {
        self.thread_root.as_deref()
    }

    /// The chaz-core channel for this target: the composite key inside a
    /// thread, the bare room ID on the timeline.
    pub(crate) fn channel(&self) -> String {
        match &self.thread_root {
            Some(root) => thread_channel(self.room.room_id().as_str(), root),
            None => self.room.room_id().to_string(),
        }
    }

    /// Stamp this target's thread relation onto outbound content.
    ///
    /// `without_fallback` rather than `plain`: the reply-fallback form has to
    /// name the thread's latest event, which the bridge does not track, and a
    /// stale pointer there is worse than none.
    fn route(&self, mut content: RoomMessageEventContent) -> RoomMessageEventContent {
        if let Some(root) = &self.thread_root {
            content.relates_to = Some(Relation::Thread(Thread::without_fallback(root.clone())));
        }
        content
    }

    /// Send to this target, returning the new event's ID.
    pub(crate) async fn send(
        &self,
        content: RoomMessageEventContent,
    ) -> Result<OwnedEventId, matrix_sdk::Error> {
        Ok(self.room.send(self.route(content)).await?.response.event_id)
    }

    /// Send to this target under a caller-chosen transaction ID, so a retry the
    /// homeserver already accepted is deduplicated rather than duplicated.
    pub(crate) async fn send_with_transaction_id(
        &self,
        content: RoomMessageEventContent,
        txn_id: &TransactionId,
    ) -> Result<OwnedEventId, matrix_sdk::Error> {
        self.room
            .send(self.route(content))
            .with_transaction_id(txn_id.to_owned())
            .await
            .map(|response| response.response.event_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn durable_thread_binding_and_room_mode() {
        let (_instance, mut user) = eidetica::Instance::create_backend(
            Box::new(eidetica::backend::database::InMemory::new()),
            eidetica::NewAccount::passwordless("test"),
        )
        .await
        .unwrap();
        let (bridge_db, _) = chaz_core::bridge_db::create_bridge_db(&mut user, "matrix")
            .await
            .unwrap();
        let db = bridge_db.database();
        let channel = thread_channel("!room:e2e.test", &event_id("$root"));
        chaz_core::session::bind_transport(db, "matrix", "@agent:e2e.test", &channel)
            .await
            .unwrap();
        assert_eq!(
            chaz_core::session::transport_bindings(db).await.unwrap(),
            vec![("matrix".into(), "@agent:e2e.test".into(), channel.clone())]
        );
        write_room_mode(
            &bridge_db,
            "@agent:e2e.test",
            "!room:e2e.test",
            RoomMode::ThreadedSessions,
        )
        .await
        .unwrap();
        assert_eq!(
            read_room_mode(&bridge_db, "@agent:e2e.test", "!room:e2e.test")
                .await
                .unwrap(),
            RoomMode::ThreadedSessions
        );
        assert_eq!(
            read_room_mode(&bridge_db, "@other:e2e.test", "!room:e2e.test")
                .await
                .unwrap(),
            RoomMode::Normal
        );
    }

    fn event_id(raw: &str) -> OwnedEventId {
        EventId::parse(raw).expect("valid event id")
    }

    #[test]
    fn mode_spellings_round_trip_and_reject_unimplemented_modes() {
        for mode in [RoomMode::Normal, RoomMode::ThreadedSessions] {
            assert_eq!(RoomMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(RoomMode::parse("  normal  "), Some(RoomMode::Normal));
        assert_eq!(RoomMode::default(), RoomMode::Normal);

        // Never store a mode the bridge cannot honor.
        assert_eq!(RoomMode::parse("threaded-replies"), None);
        assert_eq!(RoomMode::parse(""), None);
        assert_eq!(RoomMode::parse("Normal"), None);
    }

    #[test]
    fn room_mode_keys_separate_logins_and_rooms_despite_colons() {
        let a = room_mode_key("@bot:example.com", "!room:example.com");
        let b = room_mode_key("@bot:example.com", "!other:example.com");
        let c = room_mode_key("@other:example.com", "!room:example.com");
        assert_ne!(a, b);
        assert_ne!(a, c);
        // The pathological case a `:`-joined key would collide on.
        assert_ne!(
            room_mode_key("@bot:example.com", "!r:s"),
            room_mode_key("@bot", "example.com:!r:s")
        );
    }

    #[test]
    fn thread_channels_round_trip() {
        for (room, root) in [
            ("!room:example.com", "$abcDEF123"),
            ("!od#d%part:example.com", "$od#d%root:example.com"),
        ] {
            let channel = thread_channel(room, &event_id(root));
            assert_eq!(
                parse_thread_channel(&channel).expect("parses as a thread channel"),
                (room.to_string(), event_id(root)),
                "channel: {channel}"
            );
        }
    }

    /// A `threaded-sessions` room: top-level opens a thread at the event, a
    /// thread reply continues its root, and no registry lookup is needed.
    #[test]
    fn threaded_room_opens_one_thread_per_top_level_message() {
        let mode = RoomMode::ThreadedSessions;
        let top_level = event_id("$top");
        let root = event_id("$root");

        assert!(!needs_managed_lookup(mode, None));
        assert_eq!(
            continued_thread_root(mode, None, false),
            None,
            "a top-level message continues nothing"
        );
        assert_eq!(
            opened_thread_root(mode, None, &top_level),
            Some(top_level.clone()),
            "a top-level message roots a thread at itself"
        );
        // Two distinct top-level messages therefore key two distinct sessions.
        assert_ne!(
            thread_channel("!r:s", &top_level),
            thread_channel("!r:s", &event_id("$other-top")),
        );

        assert!(!needs_managed_lookup(mode, Some(&root)));
        assert_eq!(
            continued_thread_root(mode, Some(&root), false),
            Some(root.clone()),
            "a thread reply continues its root's session without needing proof it exists"
        );
        assert_eq!(
            opened_thread_root(mode, Some(&root), &event_id("$reply")),
            None,
            "a reply inside a thread never forks a second one"
        );
    }

    /// A `normal` room is unchanged except for one thing: a thread the bridge
    /// already manages keeps working, so switching a room back does not strand
    /// conversations opened while it was threaded.
    #[test]
    fn normal_room_keeps_timeline_routing_but_does_not_strand_managed_threads() {
        let mode = RoomMode::Normal;
        let root = event_id("$root");

        assert!(!needs_managed_lookup(mode, None));
        assert_eq!(continued_thread_root(mode, None, false), None);
        assert_eq!(
            opened_thread_root(mode, None, &event_id("$top")),
            None,
            "a top-level message in an ordinary room uses the room-wide session"
        );

        // A thread reply is the one case that needs the lookup.
        assert!(needs_managed_lookup(mode, Some(&root)));
        assert_eq!(
            continued_thread_root(mode, Some(&root), true),
            Some(root.clone()),
            "a managed thread stays reachable after the room reverts to normal"
        );
        assert_eq!(
            continued_thread_root(mode, Some(&root), false),
            None,
            "an unrelated thread in an ordinary room keeps its previous behavior"
        );
    }

    #[test]
    fn a_room_level_channel_is_never_read_as_a_thread() {
        // The bare room ID every `normal` room already has bound on disk.
        assert!(parse_thread_channel("!room:example.com").is_none());
        // Opaque room localparts can contain both the separator and `$`.
        assert!(parse_thread_channel("!od#d:example.com").is_none());
        assert!(parse_thread_channel("!odd#$legacy:example.com").is_none());
        // And foreign shapes are rejected rather than half-parsed.
        assert!(parse_thread_channel("no-sigils#at-all").is_none());
        assert!(parse_thread_channel("!room:example.com#not-an-event").is_none());
        assert!(parse_thread_channel("").is_none());
    }
}
