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
                for entry in pending {
                    delivered.insert((entry.timestamp, entry.content.clone()));
                    let body = render_outbound(&owning_agent, &entry.sender, &entry.content);
                    if let Err(e) = send(body).await {
                        error!("Failed to deliver to transport: {e}");
                    }
                }
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
}
