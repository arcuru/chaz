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
use crate::session::{Session, current_session_writer};
use crate::types::ConversationId;
use eidetica::Database;
use eidetica::auth::crypto::PrivateKey;
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
    signing_key: Option<PrivateKey>,
) -> (
    mpsc::Sender<ApprovalExchange>,
    Vec<tokio::task::JoinHandle<()>>,
) {
    let mut tasks = Vec::new();
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
        tasks.push(tokio::spawn(async move {
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
        }));
    }

    // Requester: write a request entry per exchange and track it as pending.
    {
        let db = session_db;
        tasks.push(tokio::spawn(async move {
            while let Some(exchange) = rx.recv().await {
                let (request_id, entry) =
                    approval_request_entry(&agent_name, &exchange.info, timeout);
                // Record pending before the write so a (synced) decision that
                // races the write still finds a slot to resolve.
                pending
                    .lock()
                    .await
                    .insert(request_id.clone(), exchange.decision_tx);
                let written = async {
                    let writer = current_session_writer(&db, signing_key.as_ref()).await?;
                    let mut session = Session::try_new(ConversationId(sid.clone()), writer).await?;
                    session.add_entry(entry).await
                }.await;
                if let Err(e) = written {
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
                let signing_key = signing_key.clone();
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
                    let written = async {
                        let writer = current_session_writer(&db, signing_key.as_ref()).await?;
                        let mut session = Session::try_new(ConversationId(sid.clone()), writer).await?;
                        session.add_entry(approval_decision_entry("system", &request_id, ApprovalDecision::TimedOut)).await
                    }.await;
                    if let Err(e) = written {
                        warn!(session = %sid, %request_id, "Could not record the approval timeout ({e}); the runtime was unblocked regardless");
                    }
                });
            }
        }));
    }

    (tx, tasks)
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

    async fn delegated_proxy_fixture() -> (
        eidetica::Instance,
        eidetica::AccountSession,
        Database,
        Database,
        PrivateKey,
    ) {
        use eidetica::auth::types::{
            AuthKey, DelegatedTreeRef, Permission, PermissionBounds, TreeReference,
        };
        let (instance, mut user) = eidetica::Instance::create_backend(
            Box::new(eidetica::backend::database::InMemory::new()),
            eidetica::NewAccount::passwordless("proxy"),
        )
        .await
        .unwrap();
        let controller = user.get_default_key().unwrap();
        let member = user.add_private_key(Some("approval-writer")).await.unwrap();
        let identity = user
            .create_database(eidetica::crdt::Doc::new(), &controller)
            .await
            .unwrap();
        let txn = identity.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(
                &member,
                AuthKey::active(Some("writer"), Permission::Write(10)),
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let target = Database::create(
            &instance,
            PrivateKey::generate(),
            eidetica::crdt::Doc::new(),
        )
        .await
        .unwrap();
        let txn = target.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .add_delegated_tree(DelegatedTreeRef {
                permission_bounds: PermissionBounds {
                    max: Permission::Write(10),
                    min: None,
                },
                tree: TreeReference {
                    root: identity.root_id().clone(),
                    tips: identity.snapshot().await.unwrap().into_tips(),
                },
            })
            .await
            .unwrap();
        txn.commit().await.unwrap();
        user.track_database(
            target.root_id().clone(),
            &member,
            eidetica::account::types::SyncSettings::disabled(),
        )
        .await
        .unwrap();
        let old = user
            .open_database_with_key(target.root_id(), &member)
            .await
            .unwrap();
        let key = user.get_signing_key(&member).unwrap();
        (instance, user, identity, old, key)
    }

    async fn raise_approval_floor(
        identity: &Database,
        db: &Database,
        key: &PrivateKey,
        name: &str,
    ) {
        use eidetica::store::DocStore;
        let txn = identity.new_transaction().await.unwrap();
        txn.get_settings().unwrap().set_name(name).await.unwrap();
        txn.commit().await.unwrap();
        let fresh = current_session_writer(db, Some(key)).await.unwrap();
        let txn = fresh.new_transaction().await.unwrap();
        txn.get_store::<DocStore>("floor")
            .await
            .unwrap()
            .set_string("value", name)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    #[tokio::test]
    async fn context_selection_refreshes_the_held_signer_without_reloading_context() {
        use crate::context::ContextBuilder;
        use crate::runtime::RuntimeMessage;
        use eidetica::store::DocStore;
        let (_instance, _user, identity, old, key) = delegated_proxy_fixture().await;
        let mut session = Session::new(ConversationId(old.root_id().to_string()), old.clone())
            .await
            .with_signing_key(Some(key.clone()));
        let entry = |content: &str| crate::session::SessionEntry {
            sender: "human".into(),
            content: content.into(),
            timestamp: chrono::Utc::now(),
            entry_type: crate::session::EntryType::Message,
            metadata: None,
            routing: None,
        };
        session.add_entry(entry("held context")).await.unwrap();
        let hub = crate::test_support::context_hub().await;
        hub.record_active(&old).await.unwrap();
        // The fresh heartbeat can raise the inherited floor after the turn
        // captures its writer, before the first context selection is persisted.
        raise_approval_floor(&identity, &old, &key, "before context selection").await;
        let mut fresh = Session::new(
            ConversationId(old.root_id().to_string()),
            current_session_writer(&old, Some(&key)).await.unwrap(),
        )
        .await;
        fresh.add_entry(entry("later message")).await.unwrap();
        let config = Default::default();
        let active = std::collections::HashSet::from(["baseline_context".to_string()]);
        let assembled = ContextBuilder::new(session.entries(), "host", "", &config)
            .with_extension_hub(Arc::new(hub))
            .with_session_db(session.database())
            .with_session_signing_key(session.signing_key())
            .with_invocation(&active, None)
            .try_build()
            .await
            .unwrap();
        assert!(
            assembled
                .messages
                .iter()
                .any(|m| matches!(m, RuntimeMessage::User(text) if text == "held context"))
        );
        assert!(
            !assembled
                .messages
                .iter()
                .any(|m| matches!(m, RuntimeMessage::User(text) if text == "later message"))
        );
        let txn = old.new_transaction().await.unwrap();
        let selected = txn
            .get_store::<DocStore>("context_selections")
            .await
            .unwrap();
        assert!(
            selected
                .get_string("[\"host\",\"baseline_context\"]")
                .await
                .is_ok()
        );
        assert!(Session::renew_turn_activity(&old, "missing").await.is_err());
    }

    #[tokio::test]
    async fn active_turn_survives_floor_advances_across_approval_and_settlement() {
        use crate::runtime::{RuntimeRecord, RuntimeRecorder, ToolResultOutcome};
        use crate::server::SessionRuntimeRecorder;
        use crate::session::{EntryType, SessionEntry};
        use std::sync::atomic::{AtomicBool, AtomicU32};
        let (_instance, _user, identity, old, key) = delegated_proxy_fixture().await;
        let sid = ConversationId(old.root_id().to_string());
        let entry = |content: &str| SessionEntry {
            sender: "host".into(),
            content: content.into(),
            timestamp: chrono::Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        };
        Session::initialize_turn_schema(&old).await.unwrap();
        let mut session = Session::new(sid.clone(), old.clone())
            .await
            .with_signing_key(Some(key.clone()));
        let request = session.add_entry(entry("request")).await.unwrap();
        let attempt = session.start_turn_attempt(request.clone()).await.unwrap();
        let held = Arc::new(Mutex::new(session));
        let recorder = SessionRuntimeRecorder {
            session: held.clone(),
            request_id: request,
            attempt_id: attempt.attempt_id.clone(),
            sequence: AtomicU32::new(0),
            terminal: Default::default(),
            explicit_bridge_send: AtomicBool::new(false),
            agent_name: "host".into(),
            requested_model: "stub".into(),
            context_snapshot: None,
            can_suspend: false,
            branch: None,
            fail_after: Default::default(),
        };
        let (tx, tasks) = spawn_session_db_approval_proxy(
            old.clone(),
            "host".into(),
            Duration::from_secs(10),
            Some(key.clone()),
        )
        .await;
        raise_approval_floor(&identity, &old, &key, "another chat").await;
        let pending = tokio::spawn(async move { security(tx).request_approval(ask()).await });
        let id = await_request_id(&old).await;
        let mut bridge = Session::new(
            sid.clone(),
            current_session_writer(&old, Some(&key)).await.unwrap(),
        )
        .await;
        bridge
            .add_entry(approval_decision_entry(
                "human",
                &id,
                ApprovalDecision::Deny,
            ))
            .await
            .unwrap();
        assert_eq!(pending.await.unwrap(), ApprovalDecision::Deny);
        // A further advancement during the active turn rules out a one-time
        // post-approval refresh. No transcript reload may replace held context.
        for sequence in 0..2 {
            raise_approval_floor(&identity, &old, &key, &format!("advance {sequence}")).await;
            recorder
                .record(RuntimeRecord::ToolResult {
                    model_sequence: sequence,
                    call_index: 0,
                    call_id: format!("call-{sequence}"),
                    name: "shell".into(),
                    output: "denied".into(),
                    outcome: ToolResultOutcome::Denied,
                })
                .await
                .unwrap();
            assert!(
                Session::renew_turn_activity(&old, &attempt.attempt_id)
                    .await
                    .is_err(),
                "native floor must still reject stale handles"
            );
            let heartbeat = current_session_writer(&old, Some(&key)).await.unwrap();
            Session::renew_turn_activity(&heartbeat, &attempt.attempt_id)
                .await
                .unwrap();
        }
        raise_approval_floor(&identity, &old, &key, "before final settlement").await;
        let mut session = held.lock().await;
        session.refresh_writer().await.unwrap();
        assert!(
            !session
                .entries()
                .iter()
                .any(|e| matches!(e.entry_type, EntryType::ApprovalRequest)),
            "refresh must not reload the held transcript"
        );
        session
            .complete_turn_attempt_with_transcript(&attempt, Some(entry("FINAL")), None)
            .await
            .unwrap();
        let observed = Session::new(sid, old.clone()).await;
        assert_eq!(
            observed
                .turn_transcript(&attempt.attempt_id)
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            observed
                .entries()
                .iter()
                .filter(|e| e.content == "FINAL")
                .count(),
            1
        );
        assert!(
            Session::active_turn_attempts(&old)
                .await
                .unwrap()
                .is_empty()
        );
        for task in tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn delegated_proxy_refreshes_same_signer_before_publishing_request() {
        let (_instance, _user, identity, old, key) = delegated_proxy_fixture().await;
        let (tx, tasks) = spawn_session_db_approval_proxy(
            old.clone(),
            "chaz".into(),
            Duration::from_secs(60),
            Some(key.clone()),
        )
        .await;
        raise_approval_floor(&identity, &old, &key, "later chat").await;
        // The native regression guard remains effective. The old writer must
        // not be usable after a fresh ancestor raised the inherited floor.
        let mut stale = Session::new(ConversationId(old.root_id().to_string()), old.clone()).await;
        assert!(
            stale
                .add_entry(approval_decision_entry(
                    "system",
                    "control",
                    ApprovalDecision::Deny
                ))
                .await
                .is_err()
        );
        let mut pending = tokio::spawn(async move { security(tx).request_approval(ask()).await });
        let request = await_request_id(&old).await;
        let fresh = current_session_writer(&old, Some(&key)).await.unwrap();
        let mut session = Session::new(ConversationId(old.root_id().to_string()), fresh).await;
        // A different request ID cannot resolve this slot.
        session
            .add_entry(approval_decision_entry(
                "human",
                "unrelated",
                ApprovalDecision::Deny,
            ))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut pending)
                .await
                .is_err()
        );
        session
            .add_entry(approval_decision_entry(
                "human",
                &request,
                ApprovalDecision::Deny,
            ))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .unwrap()
                .unwrap(),
            ApprovalDecision::Deny
        );
        for task in tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn delegated_proxy_refreshes_again_before_recording_timeout() {
        let (_instance, _user, identity, old, key) = delegated_proxy_fixture().await;
        let (tx, tasks) = spawn_session_db_approval_proxy(
            old.clone(),
            "chaz".into(),
            Duration::from_millis(400),
            Some(key.clone()),
        )
        .await;
        let pending = tokio::spawn(async move { security(tx).request_approval(ask()).await });
        let request = await_request_id(&old).await;
        raise_approval_floor(&identity, &old, &key, "advance while pending").await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .unwrap()
                .unwrap(),
            ApprovalDecision::TimedOut
        );
        for _ in 0..200 {
            let session =
                Session::new(ConversationId(old.root_id().to_string()), old.clone()).await;
            if resolved_decisions(session.entries()).get(&request)
                == Some(&ApprovalDecision::TimedOut)
            {
                for task in tasks {
                    task.abort();
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timeout audit did not land after delegated floor advance");
    }

    #[tokio::test]
    async fn approval_writer_does_not_replace_revoked_or_read_signer() {
        use eidetica::auth::types::{AuthKey, Permission};
        let (_instance, user, identity, old, key) = delegated_proxy_fixture().await;
        // Another key really qualifies, but is never a publication fallback.
        let other = user
            .get_signing_key(&user.get_default_key().unwrap())
            .unwrap();
        assert!(current_session_writer(&old, Some(&other)).await.is_ok());
        for grant in [
            AuthKey::active(Some("writer"), Permission::Read),
            AuthKey::new(
                Some("writer"),
                Permission::Write(10),
                eidetica::auth::types::KeyStatus::Revoked,
            ),
        ] {
            let txn = identity.new_transaction().await.unwrap();
            txn.get_settings()
                .unwrap()
                .set_auth_key(&key.public_key(), grant)
                .await
                .unwrap();
            txn.commit().await.unwrap();
            assert!(current_session_writer(&old, Some(&key)).await.is_err());
            assert!(current_session_writer(&old, Some(&other)).await.is_ok());
        }
    }

    /// The headline guarantee: an unanswered request expires, and does so as a
    /// `TimedOut` rather than a bare `Deny` — the runtime is told nobody
    /// answered, not that someone refused. The 5s guard is what makes this a
    /// real probe: against a proxy that waits out a fixed 30-minute ceiling
    /// instead of the configured one, the request is still pending here and
    /// the test fails.
    #[tokio::test]
    async fn an_unanswered_request_expires_into_a_timeout() {
        let (_inst, user, db) = test_session_db().await;
        let (tx, _tasks) = spawn_session_db_approval_proxy(
            db,
            "chaz".to_string(),
            Duration::from_millis(250),
            Some(
                user.get_signing_key(&user.get_default_key().unwrap())
                    .unwrap(),
            ),
        )
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
        let (_inst, user, db) = test_session_db().await;
        let (tx, _tasks) = spawn_session_db_approval_proxy(
            db.clone(),
            "chaz".to_string(),
            Duration::from_millis(250),
            Some(
                user.get_signing_key(&user.get_default_key().unwrap())
                    .unwrap(),
            ),
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
        let (_inst, user, db) = test_session_db().await;
        let (tx, _tasks) = spawn_session_db_approval_proxy(
            db.clone(),
            "chaz".to_string(),
            Duration::from_secs(1234),
            Some(
                user.get_signing_key(&user.get_default_key().unwrap())
                    .unwrap(),
            ),
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
        let (_inst, user, db) = test_session_db().await;
        let (tx, _tasks) = spawn_session_db_approval_proxy(
            db.clone(),
            "chaz".to_string(),
            Duration::from_millis(250),
            Some(
                user.get_signing_key(&user.get_default_key().unwrap())
                    .unwrap(),
            ),
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
        let (_inst, user, db) = test_session_db().await;
        let (tx, _tasks) = spawn_session_db_approval_proxy(
            db.clone(),
            "chaz".to_string(),
            Duration::from_secs(60),
            Some(
                user.get_signing_key(&user.get_default_key().unwrap())
                    .unwrap(),
            ),
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

    #[tokio::test]
    async fn bound_service_writer_without_captured_key_can_publish_and_expire() {
        let (instance, _user, local_db) = test_session_db().await;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("approval.sock");
        let service = eidetica::service::ServiceServer::bind(instance, &socket)
            .await
            .unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(());
        let service_task = tokio::spawn(service.run(receiver));
        let connection = crate::instance::connect_with(
            &crate::config::EideticaConfig {
                connection: format!("unix://{}", socket.display()),
                login: crate::config::EideticaLoginConfig {
                    username: "test".into(),
                    password: None,
                    passwordless: true,
                },
                sync: None,
            },
            crate::config::ExecutionRole::Client,
        )
        .await
        .unwrap();
        let db = connection
            .user
            .open_database(local_db.root_id())
            .await
            .unwrap();
        assert!(db.instance().unwrap().backend().local_engine().is_none());
        // The service-bound handle already owns a signer. No embedded private
        // key is captured; publishing must still use that exact native handle.
        let (tx, tasks) =
            spawn_session_db_approval_proxy(db, "chaz".into(), Duration::from_millis(250), None)
                .await;
        let pending = tokio::spawn(async move { security(tx).request_approval(ask()).await });
        let request = await_request_id(&local_db).await;
        assert_eq!(pending.await.unwrap(), ApprovalDecision::TimedOut);
        for _ in 0..200 {
            let session = Session::new(
                ConversationId(local_db.root_id().to_string()),
                local_db.clone(),
            )
            .await;
            if resolved_decisions(session.entries()).get(&request)
                == Some(&ApprovalDecision::TimedOut)
            {
                for task in tasks {
                    task.abort();
                }
                drop(shutdown);
                service_task.await.unwrap().unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("bound service writer did not record expiry");
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
        let (tx, _tasks) = spawn_session_db_approval_proxy(
            unwritable,
            "chaz".to_string(),
            Duration::from_secs(60),
            None,
        )
        .await;

        let decision =
            tokio::time::timeout(Duration::from_secs(5), security(tx).request_approval(ask()))
                .await
                .expect("an unwritable request must deny without waiting out the ceiling");
        assert_eq!(decision, ApprovalDecision::Deny);
    }
}
