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
//! Fail-closed everywhere: if the DB can't be watched or the request can't be
//! written, the pending slot is dropped — which closes the oneshot and makes
//! `request_approval` default to [`ApprovalDecision::Deny`]. If no decision
//! lands within the configured
//! [`ApprovalsConfig::timeout`](crate::bridge::ApprovalsConfig), the slot
//! resolves to [`ApprovalDecision::TimedOut`], which is equally not an
//! approval but says why.
//!
//! The clock lives here and only here. Each request entry carries its own
//! ceiling, so a bridge renders the deadline it was given instead of reading a
//! config key that could disagree with this one.

use crate::bridge::{
    ApprovalDecision, ApprovalExchange, approval_decision_entry, approval_request_entry,
    resolved_decisions,
};
use crate::session::Session;
use crate::types::ConversationId;
use eidetica::Database;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>;

/// Spawn the approval proxy for one bridge-exposed session and return the
/// `approval_tx` to hand to [`Server::register_session`](crate::server::Server::register_session).
///
/// `timeout` is the fail-closed ceiling: a request with no decision by then is
/// denied, so a down or silent bridge cannot hang the agent's ReAct loop.
pub async fn spawn_session_db_approval_proxy(
    session_db: Database,
    agent_name: String,
    timeout: Duration,
) -> mpsc::Sender<ApprovalExchange> {
    let (tx, mut rx) = mpsc::channel::<ApprovalExchange>(8);
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let sid = session_db.root_id().to_string();

    // Watch the session DB; every write pings the resolver to rescan decisions.
    let (ping_tx, mut ping_rx) = mpsc::channel::<()>(32);
    match session_db
        .on_write(move |event, _db| {
            // Source-agnostic on purpose: the decision this proxy is waiting
            // for is written by the bridge, which on a split deployment is a
            // separate peer — so it reaches this DB as a `Remote` write.
            debug!(source = ?event.source(), "Session write; rescanning approval decisions");
            let ping_tx = ping_tx.clone();
            Box::pin(async move {
                let _ = ping_tx.send(()).await;
                Ok(())
            })
        })
        .await
    {
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
                let (request_id, entry) =
                    approval_request_entry(&agent_name, &exchange.info, timeout);
                // Record pending before the write so a (synced) decision that
                // races the write still finds a slot to resolve.
                pending
                    .lock()
                    .await
                    .insert(request_id.clone(), exchange.decision_tx);
                let mut session = Session::new(ConversationId(sid.clone()), db.clone()).await;
                if let Err(e) = session.add_entry(entry).await {
                    // Nobody will ever see this request, so waiting out the
                    // timeout only delays the inevitable: deny now.
                    if pending.lock().await.remove(&request_id).is_some() {
                        warn!(session = %sid, %request_id, "Approval request could not be written ({e}); denying");
                    }
                    continue;
                }
                debug!(session = %sid, %request_id, tool = %exchange.info.name, "Approval request proxied to session DB");

                // Fail-closed timeout. Claiming the slot is what makes this
                // atomic against a decision landing in the same instant:
                // whoever removes it first owns the outcome.
                let pending = pending.clone();
                let request_id = request_id.clone();
                let sid = sid.clone();
                let db = db.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(timeout).await;
                    let Some(slot) = pending.lock().await.remove(&request_id) else {
                        return; // answered in time
                    };
                    warn!(session = %sid, %request_id, "Approval timed out with no decision");
                    // Unblock the runtime first; it has waited long enough, and
                    // the record below is not something it needs to wait on.
                    let _ = slot.send(ApprovalDecision::TimedOut);
                    // Record the outcome in the session so the tree describes
                    // what happened to its own request, and so every attached
                    // bridge can tell its channel — without any of them running
                    // a clock of their own.
                    let mut session = Session::new(ConversationId(sid.clone()), db.clone()).await;
                    if let Err(e) = session
                        .add_entry(approval_decision_entry(
                            "system",
                            &request_id,
                            ApprovalDecision::TimedOut,
                        ))
                        .await
                    {
                        warn!(session = %sid, %request_id, "Could not record the approval timeout ({e}); the runtime was unblocked regardless");
                    }
                });
            }
        });
    }

    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{approval_decision_entry, parse_approval_request};
    use crate::security::{LeakDetector, LeakPolicy, SecurityContext};
    use crate::session::test_helpers::test_session_db;
    use crate::tool::{RiskLevel, ToolApprovalInfo};

    fn ask() -> ToolApprovalInfo {
        ToolApprovalInfo {
            name: "shell".to_string(),
            arguments_display: "rm -rf /".to_string(),
            risk_level: RiskLevel::High,
        }
    }

    fn security(tx: mpsc::Sender<ApprovalExchange>) -> SecurityContext {
        SecurityContext {
            leak_detector: LeakDetector::new(LeakPolicy::Redact),
            auto_approved_tools: Default::default(),
            approval_callback: Some(tx),
        }
    }

    /// Poll the session DB until an approval request lands, then return its id.
    async fn await_request_id(db: &eidetica::Database) -> String {
        let sid = db.root_id().to_string();
        for _ in 0..200 {
            let session = Session::new(ConversationId(sid.clone()), db.clone()).await;
            if let Some(req) = session.entries().iter().find_map(parse_approval_request) {
                return req.request_id;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no approval request was written");
    }

    /// The headline guarantee: an unanswered request expires, and does so as a
    /// `TimedOut` rather than a bare `Deny` — the runtime is told nobody
    /// answered, not that someone refused. The 5s guard is what makes this a
    /// real probe: against a proxy that waits out a fixed 30-minute ceiling
    /// instead of the configured one, the request is still pending here and
    /// the test fails.
    #[tokio::test]
    async fn an_unanswered_request_expires_into_a_timeout() {
        let (_inst, _user, db) = test_session_db().await;
        let tx =
            spawn_session_db_approval_proxy(db, "chaz".to_string(), Duration::from_millis(250))
                .await;

        let decision =
            tokio::time::timeout(Duration::from_secs(5), security(tx).request_approval(ask()))
                .await
                .expect("expiry must resolve the request, not hang it");
        assert_eq!(decision, ApprovalDecision::TimedOut);
        assert!(!decision.is_approval(), "a timeout must never run the tool");
    }

    /// The daemon records its own expiry in the session, so the tree describes
    /// what happened to its request and every attached bridge can render it
    /// without keeping a clock. This is the entry the reaper used to write.
    #[tokio::test]
    async fn an_expiry_is_recorded_in_the_session() {
        let (_inst, _user, db) = test_session_db().await;
        let tx = spawn_session_db_approval_proxy(
            db.clone(),
            "chaz".to_string(),
            Duration::from_millis(250),
        )
        .await;
        let sec = security(tx);

        let pending = tokio::spawn(async move { sec.request_approval(ask()).await });
        let request_id = await_request_id(&db).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .expect("expiry must resolve the request")
                .unwrap(),
            ApprovalDecision::TimedOut
        );

        for _ in 0..200 {
            let session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
            let decisions = resolved_decisions(session.entries());
            if let Some(d) = decisions.get(&request_id) {
                assert_eq!(*d, ApprovalDecision::TimedOut);
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the expiry was never written to the session");
    }

    /// The ceiling travels on the request, so a bridge can tell the channel how
    /// long it has without reading the daemon's config.
    #[tokio::test]
    async fn a_request_carries_its_own_ceiling() {
        let (_inst, _user, db) = test_session_db().await;
        let tx = spawn_session_db_approval_proxy(
            db.clone(),
            "chaz".to_string(),
            Duration::from_secs(1234),
        )
        .await;
        let sec = security(tx);

        let _pending = tokio::spawn(async move { sec.request_approval(ask()).await });
        await_request_id(&db).await;

        let session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        let payload = session
            .entries()
            .iter()
            .find_map(parse_approval_request)
            .expect("a request entry");
        assert_eq!(payload.timeout_secs, 1234);
    }

    /// An answer arriving after the daemon gave up must not resolve the
    /// request: the runtime was already told `TimedOut` and the tool never ran.
    /// This is what lets a bridge write its answer without consulting a clock.
    #[tokio::test]
    async fn an_answer_after_the_expiry_does_not_resolve_the_request() {
        let (_inst, _user, db) = test_session_db().await;
        let tx = spawn_session_db_approval_proxy(
            db.clone(),
            "chaz".to_string(),
            Duration::from_millis(250),
        )
        .await;
        let sec = security(tx);

        let pending = tokio::spawn(async move { sec.request_approval(ask()).await });
        let request_id = await_request_id(&db).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .expect("expiry must resolve the request")
                .unwrap(),
            ApprovalDecision::TimedOut
        );

        // The late ✅ a bridge had no way to know was pointless.
        let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        session
            .add_entry(approval_decision_entry(
                "@patrick:example",
                &request_id,
                ApprovalDecision::Approve,
            ))
            .await
            .unwrap();

        let session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        assert_eq!(
            resolved_decisions(session.entries()).get(&request_id),
            Some(&ApprovalDecision::TimedOut),
            "the session must resolve to what the daemon acted on"
        );
    }

    /// A decision written by a bridge still wins the race against the ceiling.
    #[tokio::test]
    async fn a_decision_written_in_time_resolves_the_request() {
        let (_inst, _user, db) = test_session_db().await;
        let tx = spawn_session_db_approval_proxy(
            db.clone(),
            "chaz".to_string(),
            Duration::from_secs(60),
        )
        .await;
        let sec = security(tx);

        let pending = tokio::spawn(async move { sec.request_approval(ask()).await });
        let request_id = await_request_id(&db).await;

        let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
        session
            .add_entry(approval_decision_entry(
                "@patrick:example",
                &request_id,
                ApprovalDecision::Approve,
            ))
            .await
            .unwrap();

        let decision = tokio::time::timeout(Duration::from_secs(10), pending)
            .await
            .expect("decision must unblock the request")
            .unwrap();
        assert_eq!(decision, ApprovalDecision::Approve);
    }

    /// A request that could not be written can never be answered, so it must
    /// deny immediately rather than sit out the full ceiling. The 60s ceiling
    /// against a 5s guard is the assertion: only a surfaced write error can
    /// get here in time.
    #[tokio::test]
    async fn a_request_that_cannot_be_written_denies_at_once() {
        let (instance, _user, db) = test_session_db().await;
        // Same tree, opened without a signing key: reads work, commits don't.
        let unwritable = eidetica::Database::open(&instance, db.root_id())
            .await
            .unwrap();
        let tx = spawn_session_db_approval_proxy(
            unwritable,
            "chaz".to_string(),
            Duration::from_secs(60),
        )
        .await;

        let decision =
            tokio::time::timeout(Duration::from_secs(5), security(tx).request_approval(ask()))
                .await
                .expect("an unwritable request must deny without waiting out the ceiling");
        assert_eq!(decision, ApprovalDecision::Deny);
    }
}
