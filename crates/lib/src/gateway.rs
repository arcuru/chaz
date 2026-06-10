//! Cross-crate gateway surface.
//!
//! The [`Gateway`] trait, [`ApprovalExchange`] struct, and
//! [`ApprovalDecision`] enum live in the library so runtime / security /
//! server code can reference them. Concrete gateway implementations
//! (Matrix, TUI, CLI) live in the binary crate.
//!
//! The session DB *is* the conversation; a gateway is a pure bidirectional
//! translator between one session DB and one transport. The reconcile
//! helpers below ([`inbound_user_entry`], [`render_outbound`],
//! [`undelivered_agent_messages`], [`attach_reconciler`]) are the
//! transport-generic half of that contract — an external gateway binary
//! (`chaz-discord`, …) links them rather than re-deriving the DB↔surface
//! invariants. The Matrix gateway in the binary crate is itself written on
//! top of them, so they stay honest about being transport-agnostic.

use crate::agent::AgentRegistry;
use crate::server::Server;
use crate::session::{EntryRouting, EntryType, Session, SessionEntry, TransportRef};
use crate::tool::ToolApprovalInfo;
use crate::types::ConversationId;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tracing::error;

/// Trait for transport gateways (Matrix, TUI, etc.)
///
/// A gateway owns a transport connection and bridges platform events
/// into session database entries. The server processes entries via
/// callbacks and delivers responses through the response channel.
pub trait Gateway {
    fn run(
        self,
        server: Arc<Server>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

/// An approval exchange: the runtime sends tool info and a channel to receive the decision.
pub struct ApprovalExchange {
    pub info: ToolApprovalInfo,
    pub decision_tx: oneshot::Sender<ApprovalDecision>,
}

/// User's decision on a tool approval request
#[derive(Clone, Debug, PartialEq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
    /// Approve this and all remaining tool calls this turn
    ApproveAll,
}

// ---------------------------------------------------------------------------
// Reconcile / translate helpers (transport-generic)
// ---------------------------------------------------------------------------

/// Build the inbound user entry for a message received on a transport.
///
/// Stamps the invariants every gateway ingester must get right: an
/// `EntryType::Message`, the receipt timestamp, and an [`EntryRouting`]
/// `source` recording which `(transport, login_id, channel)` it arrived on
/// and from whom. An agent's reply is routed back to this channel by
/// resolving that `source`, so a gateway author writes one call here instead
/// of re-deriving the routing shape.
pub fn inbound_user_entry(
    transport: &str,
    login_id: &str,
    channel: &str,
    sender: &str,
    sender_display: Option<String>,
    content: &str,
    message_id: Option<String>,
) -> SessionEntry {
    SessionEntry {
        sender: sender.to_string(),
        content: content.to_string(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: Some(EntryRouting {
            source: Some(TransportRef {
                transport: transport.to_string(),
                login_id: login_id.to_string(),
                channel: channel.to_string(),
                sender: Some(sender.to_string()),
                sender_display,
                message_id,
            }),
            ..Default::default()
        }),
    }
}

/// Render an agent's session write for delivery to a transport channel.
///
/// A login belongs to one agent. The owning agent speaks as the channel's
/// transport identity, so its writes go out plain; any other agent writing
/// into this session is a guest, shown with an `[AgentName]` prefix so
/// readers can tell speakers apart under the single identity.
pub fn render_outbound(owning_agent: &str, sender: &str, content: &str) -> String {
    if sender == owning_agent {
        content.to_string()
    } else {
        format!("[{sender}] {content}")
    }
}

/// Identity of a delivered entry: `(timestamp, content)` — the same key the
/// session backfill dedupes on.
type DeliveredKey = (DateTime<Utc>, String);
/// Per-channel set of agent messages already sent to the transport.
type DeliveredSet = Arc<Mutex<HashSet<DeliveredKey>>>;

/// The agent `Message` entries in `entries` not yet in `delivered`.
///
/// Pure, so the reconcile rule is unit-testable without a live DB or
/// transport. `is_agent` selects agent senders; human participants' messages
/// are already on the transport, so they are never echoed back.
pub fn undelivered_agent_messages<'a>(
    entries: &'a [SessionEntry],
    is_agent: impl Fn(&str) -> bool,
    delivered: &HashSet<DeliveredKey>,
) -> Vec<&'a SessionEntry> {
    entries
        .iter()
        .filter(|e| {
            e.entry_type == EntryType::Message
                && is_agent(&e.sender)
                && !delivered.contains(&(e.timestamp, e.content.clone()))
        })
        .collect()
}

/// Deliver `pending` agent entries to a transport in order, marking each one
/// delivered only *after* its `send` succeeds. A failed send logs and stops the
/// pass, leaving that entry and the tail behind it unmarked so a later
/// reconcile retries them — in order. Marking before (or despite) a failed send
/// is what silently drops a message; this never does. A `send` never writes the
/// session DB, so a persistently failing transport can't spin this into a
/// resend loop — the retry only fires when fresh session activity arrives.
///
/// Returns the number delivered this pass. Split out from [`attach_reconciler`]
/// so the mark-after-success contract is unit-testable without a live DB.
async fn deliver_in_order<S, Fut>(
    pending: &[&SessionEntry],
    owning_agent: &str,
    delivered: &mut HashSet<DeliveredKey>,
    send: &S,
) -> usize
where
    S: Fn(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let mut delivered_now = 0;
    for entry in pending {
        let body = render_outbound(owning_agent, &entry.sender, &entry.content);
        if let Err(e) = send(body).await {
            error!(
                "Failed to deliver to transport: {e}; \
                 leaving it for retry on the next write"
            );
            break;
        }
        delivered.insert((entry.timestamp, entry.content.clone()));
        delivered_now += 1;
    }
    delivered_now
}

/// Install a reconciling response callback on a session DB: on every write,
/// converge the transport channel to DB state by sending any agent `Message`
/// entries it doesn't already show. Unlike forwarding `latest_entry()` alone,
/// this emits the full channel-vs-DB delta, so a write that lands several
/// messages at once (a remote sync, or several agents) loses none.
///
/// The history present at install time already shows on the transport, so it
/// seeds the delivered-set and is never re-emitted. Idempotent across the
/// caller's `attached` gate and across duplicate or out-of-order `on_write`
/// fires.
///
/// Delivery is at-least-once: an entry is added to the delivered-set only
/// after `send` reports success, and a failing send stops the pass so the
/// undelivered tail is retried — in order — on the next write. A transport
/// that delivers a multi-part body non-atomically (e.g. chunked to fit a
/// length cap) may therefore re-emit the already-sent prefix on retry; that
/// is the cost of never silently dropping a message.
///
/// `send` is the transport's delivery closure — it receives each rendered
/// body and is responsible for delivering it (and any transport-specific
/// logging). Matrix passes `room.send`; Discord passes `ChannelId::say`. It
/// is held across the delivered-set lock so concurrent writes can't
/// interleave or double-emit.
pub async fn attach_reconciler<S, Fut>(
    session_db: &eidetica::Database,
    agents: Arc<AgentRegistry>,
    owning_agent: String,
    send: S,
) -> anyhow::Result<()>
where
    S: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
{
    let sid = session_db.root_id().to_string();
    // Seed with everything already committed — the transport shows it already.
    let delivered: DeliveredSet = {
        let session = Session::new(ConversationId(sid.clone()), session_db.clone()).await;
        let seed = session
            .entries()
            .iter()
            .map(|e| (e.timestamp, e.content.clone()))
            .collect();
        Arc::new(Mutex::new(seed))
    };
    let send = Arc::new(send);
    session_db
        .on_write(move |_event, db| {
            let agents = agents.clone();
            let owning_agent = owning_agent.clone();
            let db = db.clone();
            let sid = sid.clone();
            let delivered = delivered.clone();
            let send = send.clone();
            Box::pin(async move {
                let session = Session::new(ConversationId(sid), db).await;
                // Hold the lock across the sends so concurrent writes can't
                // interleave or double-emit; delivery is serialized.
                let mut delivered = delivered.lock().await;
                let pending = undelivered_agent_messages(
                    session.entries(),
                    |s| agents.get(s).is_some(),
                    &delivered,
                );
                deliver_in_order(&pending, &owning_agent, &mut delivered, send.as_ref()).await;
                Ok(())
            })
        })?
        .detach();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sender: &str, content: &str, ts: i64, entry_type: EntryType) -> SessionEntry {
        SessionEntry {
            sender: sender.to_string(),
            content: content.to_string(),
            timestamp: DateTime::from_timestamp(ts, 0).unwrap(),
            entry_type,
            metadata: None,
            routing: None,
        }
    }

    #[test]
    fn owning_agent_writes_go_out_plain() {
        assert_eq!(render_outbound("ava", "ava", "clear skies"), "clear skies");
    }

    #[test]
    fn guest_agent_writes_are_prefixed() {
        assert_eq!(
            render_outbound("ava", "scout", "logs look clean"),
            "[scout] logs look clean"
        );
    }

    #[test]
    fn prefix_is_exact_name_match() {
        // A different-cased or partial name is a distinct agent: still a guest.
        assert_eq!(render_outbound("ava", "Ava", "hi"), "[Ava] hi");
    }

    #[test]
    fn reconcile_delivers_only_undelivered_agent_messages() {
        let entries = vec![
            entry("@human:s", "hi", 1, EntryType::Message), // human → already shown
            entry("ava", "hello", 2, EntryType::Message),   // agent, new → send
            entry("ava", "ls()", 3, EntryType::ToolCall),   // audit trail → skip
            entry("scout", "done", 4, EntryType::Message),  // guest agent, new → send
            entry("ava", "old", 5, EntryType::Message),     // agent, already delivered → skip
        ];
        let is_agent = |s: &str| s == "ava" || s == "scout";
        let mut delivered = HashSet::new();
        delivered.insert((entries[4].timestamp, "old".to_string()));

        let got: Vec<&str> = undelivered_agent_messages(&entries, is_agent, &delivered)
            .iter()
            .map(|e| e.content.as_str())
            .collect();
        assert_eq!(got, vec!["hello", "done"]);
    }

    #[test]
    fn reconcile_is_idempotent_once_delivered() {
        let entries = vec![entry("ava", "hello", 2, EntryType::Message)];
        let is_agent = |s: &str| s == "ava";

        let mut delivered = HashSet::new();
        assert_eq!(
            undelivered_agent_messages(&entries, is_agent, &delivered).len(),
            1
        );
        // Mark it delivered (what the callback does) → a second pass is a no-op.
        delivered.insert((entries[0].timestamp, "hello".to_string()));
        assert!(undelivered_agent_messages(&entries, is_agent, &delivered).is_empty());
    }

    #[test]
    fn inbound_entry_stamps_transport_provenance() {
        let e = inbound_user_entry(
            "discord",
            "login-1",
            "chan-42",
            "@user:x",
            None,
            "hello there",
            Some("msg-7".to_string()),
        );
        assert_eq!(e.entry_type, EntryType::Message);
        assert_eq!(e.sender, "@user:x");
        assert_eq!(e.content, "hello there");
        let src = e.routing.unwrap().source.unwrap();
        assert_eq!(src.transport, "discord");
        assert_eq!(src.login_id, "login-1");
        assert_eq!(src.channel, "chan-42");
        assert_eq!(src.sender.as_deref(), Some("@user:x"));
        assert_eq!(src.message_id.as_deref(), Some("msg-7"));
    }

    /// A send closure backed by shared state: it records every body it
    /// *successfully* delivers, and fails the configured bodies on their first
    /// attempt only (so a retry of the same body goes through).
    fn flaky_send(
        sent: Arc<Mutex<Vec<String>>>,
        fail_once: &[&str],
    ) -> impl Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>>>>
    {
        let fail_once: HashSet<String> = fail_once.iter().map(|s| s.to_string()).collect();
        let failed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        move |body: String| {
            let sent = sent.clone();
            let failed = failed.clone();
            let fail_once = fail_once.clone();
            Box::pin(async move {
                if fail_once.contains(&body) && failed.lock().await.insert(body.clone()) {
                    anyhow::bail!("simulated transport failure delivering {body:?}");
                }
                sent.lock().await.push(body);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn failed_send_is_not_marked_then_retries() {
        let entries = vec![entry("ava", "hello", 2, EntryType::Message)];
        let pending: Vec<&SessionEntry> = entries.iter().collect();
        let mut delivered = HashSet::new();

        let sent = Arc::new(Mutex::new(Vec::new()));
        let send = flaky_send(sent.clone(), &["hello"]);

        // First pass: the only send fails → nothing delivered, nothing marked.
        let n = deliver_in_order(&pending, "ava", &mut delivered, &send).await;
        assert_eq!(n, 0);
        assert!(delivered.is_empty(), "a failed send must not be marked");
        assert!(sent.lock().await.is_empty());

        // The next write retries the same still-undelivered entry; now it lands.
        let n = deliver_in_order(&pending, "ava", &mut delivered, &send).await;
        assert_eq!(n, 1);
        assert!(delivered.contains(&(entries[0].timestamp, "hello".to_string())));
        assert_eq!(sent.lock().await.as_slice(), &["hello".to_string()]);
    }

    #[tokio::test]
    async fn stops_at_first_failure_and_resumes_in_order() {
        let entries = vec![
            entry("ava", "a", 1, EntryType::Message),
            entry("ava", "b", 2, EntryType::Message),
            entry("ava", "c", 3, EntryType::Message),
        ];
        let pending: Vec<&SessionEntry> = entries.iter().collect();
        let mut delivered = HashSet::new();

        let sent = Arc::new(Mutex::new(Vec::new()));
        let send = flaky_send(sent.clone(), &["b"]); // "b" fails its first attempt

        // First pass: "a" lands, "b" fails → stop. "c" is never attempted.
        let n = deliver_in_order(&pending, "ava", &mut delivered, &send).await;
        assert_eq!(n, 1);
        assert_eq!(sent.lock().await.as_slice(), &["a".to_string()]);
        assert!(delivered.contains(&(entries[0].timestamp, "a".to_string())));
        assert!(!delivered.contains(&(entries[1].timestamp, "b".to_string())));

        // The reconciler recomputes pending from the delivered-set: "a" drops
        // out, leaving ["b", "c"]. The retry delivers both, in order.
        let retry: Vec<&SessionEntry> = pending
            .iter()
            .copied()
            .filter(|e| !delivered.contains(&(e.timestamp, e.content.clone())))
            .collect();
        assert_eq!(
            retry.iter().map(|e| e.content.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        let n = deliver_in_order(&retry, "ava", &mut delivered, &send).await;
        assert_eq!(n, 2);
        assert_eq!(
            sent.lock().await.as_slice(),
            &["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[tokio::test]
    async fn clean_run_marks_every_entry() {
        let entries = vec![
            entry("ava", "one", 1, EntryType::Message),
            entry("scout", "two", 2, EntryType::Message), // guest → prefixed body
        ];
        let pending: Vec<&SessionEntry> = entries.iter().collect();
        let mut delivered = HashSet::new();

        let sent = Arc::new(Mutex::new(Vec::new()));
        let send = flaky_send(sent.clone(), &[]); // nothing fails

        let n = deliver_in_order(&pending, "ava", &mut delivered, &send).await;
        assert_eq!(n, 2);
        // Owning agent plain; guest prefixed (render_outbound contract).
        assert_eq!(
            sent.lock().await.as_slice(),
            &["one".to_string(), "[scout] two".to_string()]
        );
        assert_eq!(delivered.len(), 2);
    }
}
