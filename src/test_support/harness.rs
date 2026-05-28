//! Minimal builders for assembling a runtime test environment.
//!
//! These helpers produce the smallest valid `SecretStore`, `Session`,
//! `SecurityContext`, and `ToolContext` an integration test can drive
//! `runtime::execute` with. Everything is in-memory and ephemeral.

use std::sync::Arc;

use eidetica::Instance;
use eidetica::backend::database::InMemory;
use eidetica::user::User;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::gateway::{ApprovalDecision, ApprovalExchange};
use crate::security::{LeakDetector, LeakPolicy, SecretStore, SecurityContext};
use crate::session::Session;
use crate::tool::{ScopedTools, ToolContext, ToolProfile, ToolRegistry};
use crate::tool_host::NativeToolHost;
use crate::types::ConversationId;

/// Open a fresh in-memory eidetica instance with one logged-in user.
pub(crate) async fn fresh_eidetica() -> (Instance, User) {
    let instance = Instance::open(Box::new(InMemory::new())).await.unwrap();
    let _ = instance.create_user("test", None).await;
    let user = instance.login_user("test", None).await.unwrap();
    (instance, user)
}

/// Build an empty `SecretStore` backed by a fresh in-memory eidetica DB.
/// Used by `BackendManager::with_mock` (which holds the store for type
/// compatibility but never reads it on the mock dispatch path).
pub(crate) async fn empty_secrets() -> SecretStore {
    let (_instance, mut user) = fresh_eidetica().await;
    let key = user.get_default_key().unwrap();
    let mut doc = eidetica::crdt::Doc::new();
    doc.set("name", "secrets");
    let db = user.create_database(doc, &key).await.unwrap();
    SecretStore::new(db).await
}

/// Build a fresh in-memory `Session` wrapped in an `Arc<Mutex<_>>` shaped for
/// `ToolContext::session`. Returns the `Instance` alongside so callers can
/// hold it (dropping the instance closes the backend and invalidates the DB).
pub(crate) async fn fresh_session() -> (Instance, Arc<TokioMutex<Session>>) {
    let (instance, mut user) = fresh_eidetica().await;
    let key = user.get_default_key().unwrap();
    let mut doc = eidetica::crdt::Doc::new();
    doc.set("name", "test-session");
    let db = user.create_database(doc, &key).await.unwrap();
    let conv_id = ConversationId(db.root_id().to_string());
    let session = Arc::new(TokioMutex::new(Session::new(conv_id, db).await));
    (instance, session)
}

/// Permissive `SecurityContext`: redact-mode leak scanning, no approval gate,
/// no auto-approved tool list. Combined with tools whose `ApprovalRequirement`
/// is `Never`, all tool calls go through without prompting.
pub(crate) fn permissive_security() -> SecurityContext {
    SecurityContext {
        leak_detector: LeakDetector::new(LeakPolicy::Redact),
        auto_approved_tools: Default::default(),
        approval_callback: None,
    }
}

/// `SecurityContext` wired to an auto-decision approval task. Every
/// approval request from the runtime is answered with `decision` until the
/// channel closes. Returns the context alongside the spawned task handle so
/// callers can abort it at test end (or let it die with the process).
pub(crate) fn security_with_decision(
    decision: ApprovalDecision,
) -> (SecurityContext, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<ApprovalExchange>(8);
    let handle = tokio::spawn(async move {
        while let Some(exchange) = rx.recv().await {
            let _ = exchange.decision_tx.send(decision.clone());
        }
    });
    let ctx = SecurityContext {
        leak_detector: LeakDetector::new(LeakPolicy::Redact),
        auto_approved_tools: Default::default(),
        approval_callback: Some(tx),
    };
    (ctx, handle)
}

/// Assemble a minimal `ToolContext` against the given session and tool
/// registry. All tools in the registry are in scope; no agent grants;
/// uses the native (unsandboxed) tool host.
pub(crate) fn tool_context(
    session: Arc<TokioMutex<Session>>,
    registry: Arc<ToolRegistry>,
) -> ToolContext {
    ToolContext {
        agent_name: "test-agent".to_string(),
        call_depth: 0,
        max_call_depth: 5,
        tools: ScopedTools::new(registry, None),
        profile: ToolProfile::default(),
        session,
        active_extensions: Default::default(),
        grants: Default::default(),
        agent_grants: Default::default(),
        host: Arc::new(NativeToolHost::new()),
    }
}
