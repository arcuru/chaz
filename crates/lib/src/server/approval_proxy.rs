//! Per-session tool-approval proxy over the session DB.
//!
//! A dumb bridge runs no agent, so the runtime's in-process approval callback
//! has no transport to reach the human on. This proxy is the daemon half of the
//! session-DB approval protocol (the transport-generic payloads live in
//! [`crate::bridge`]): the runtime blocks on `request_approval`, which sends an
//! [`ApprovalExchange`] here; the proxy writes an
//! [`EntryType::ApprovalRequest`](crate::session::EntryType) entry into the
//! session DB; a bridge renders it, captures the human's choice, and writes an
//! `ApprovalDecision` entry; the proxy matches it back by `request_id` and
//! unblocks the runtime.
//!
//! Fail-closed everywhere: if the DB can't be watched, the request can't be
//! written, or no decision lands within [`APPROVAL_TIMEOUT`], the pending slot
//! is dropped — which closes the oneshot and makes `request_approval` default
//! to [`ApprovalDecision::Deny`].

use crate::bridge::{
    ApprovalDecision, ApprovalExchange, approval_request_entry, resolved_decisions,
};
use crate::session::Session;
use crate::types::ConversationId;
use eidetica::Database;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

/// Fail-closed ceiling: deny if no decision lands within this window so a
/// down/silent bridge can't hang the agent's ReAct loop indefinitely.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>;

/// Spawn the approval proxy for one bridge-exposed session and return the
/// `approval_tx` to hand to [`Server::register_session`](crate::server::Server::register_session).
pub fn spawn_session_db_approval_proxy(
    session_db: Database,
    agent_name: String,
) -> mpsc::Sender<ApprovalExchange> {
    let (tx, mut rx) = mpsc::channel::<ApprovalExchange>(8);
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let sid = session_db.root_id().to_string();

    // Watch the session DB; every write pings the resolver to rescan decisions.
    let (ping_tx, mut ping_rx) = mpsc::channel::<()>(32);
    match session_db.on_write(move |_event, _db| {
        let ping_tx = ping_tx.clone();
        Box::pin(async move {
            let _ = ping_tx.send(()).await;
            Ok(())
        })
    }) {
        Ok(sub) => sub.detach(),
        Err(e) => warn!(
            session = %sid,
            "approval proxy could not watch session DB ({e}); approvals will time out to deny"
        ),
    }

    // Resolver: on each write, match landed decisions to blocked requests.
    {
        let pending = pending.clone();
        let db = session_db.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            while ping_rx.recv().await.is_some() {
                while ping_rx.try_recv().is_ok() {} // debounce a burst
                let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
                let decisions = resolved_decisions(session.entries());
                if decisions.is_empty() {
                    continue;
                }
                let mut p = pending.lock().await;
                for (request_id, decision) in decisions {
                    if let Some(slot) = p.remove(&request_id) {
                        info!(session = %sid, %request_id, ?decision, "Approval resolved via session DB");
                        let _ = slot.send(decision);
                    }
                }
            }
        });
    }

    // Requester: write a request entry per exchange and track it as pending.
    {
        let db = session_db;
        tokio::spawn(async move {
            while let Some(exchange) = rx.recv().await {
                let (request_id, entry) = approval_request_entry(&agent_name, &exchange.info);
                // Record pending before the write so a (synced) decision that
                // races the write still finds a slot to resolve.
                pending
                    .lock()
                    .await
                    .insert(request_id.clone(), exchange.decision_tx);
                let mut session = Session::new(ConversationId(sid.clone()), db.clone()).await;
                session.add_entry(entry).await;
                debug!(session = %sid, %request_id, tool = %exchange.info.name, "Approval request proxied to session DB");

                // Fail-closed timeout: drop the slot (→ closed oneshot → Deny).
                let pending = pending.clone();
                let request_id = request_id.clone();
                let sid = sid.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(APPROVAL_TIMEOUT).await;
                    if pending.lock().await.remove(&request_id).is_some() {
                        warn!(session = %sid, %request_id, "Approval timed out with no decision; denying");
                    }
                });
            }
        });
    }

    tx
}
