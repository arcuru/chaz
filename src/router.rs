use crate::agent::AgentRegistry;
use crate::gateway::{ChatRequest, ChatResponse};
use crate::runtime;
use crate::security::SecurityContext;
use crate::session::{Session, SessionMessage, SessionRegistry};
use crate::tool::{ToolContext, ToolRegistry};
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::error;

/// Maximum number of concurrent LLM calls across all conversations.
const MAX_CONCURRENT_LLM_CALLS: usize = 10;

/// Run the router, dispatching chat requests to per-message handler tasks.
///
/// Each incoming message spawns a tokio task that loads the session from eidetica,
/// processes the turn, and writes back. Different conversations run fully in parallel,
/// bounded by a global semaphore on LLM calls.
///
/// **Known limitation**: concurrent messages to the same conversation may produce
/// duplicate LLM responses, since there is no per-conversation lock. Each task
/// independently reads and writes the session DB. Eidetica's CRDTs ensure the
/// writes don't conflict, but both tasks will generate responses. This is acceptable
/// for now — per-conversation serialization can be added later if needed.
pub async fn run(
    mut event_rx: mpsc::Receiver<ChatRequest>,
    registry: Arc<SessionRegistry>,
    tools: Arc<ToolRegistry>,
    agent_registry: Arc<AgentRegistry>,
    security: SecurityContext,
) {
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_LLM_CALLS));

    while let Some(request) = event_rx.recv().await {
        // Spawn a task per incoming message
        let registry = registry.clone();
        let tools = tools.clone();
        let agent_registry = agent_registry.clone();
        let security = security.clone();
        let semaphore = semaphore.clone();

        tokio::spawn(async move {
            if let Err(e) =
                handle_message(request, &registry, &tools, &agent_registry, &security, &semaphore)
                    .await
            {
                error!("Message handler error: {e}");
            }
        });
    }
}

/// Handle a single incoming chat message.
///
/// Loads (or creates) the session from eidetica, resolves the agent,
/// runs the ReAct loop, and sends the response back via the oneshot channel.
async fn handle_message(
    request: ChatRequest,
    registry: &SessionRegistry,
    tools: &Arc<ToolRegistry>,
    agent_registry: &Arc<AgentRegistry>,
    security: &SecurityContext,
    semaphore: &Semaphore,
) -> anyhow::Result<()> {
    // Resolve transport_id to a session DB (creates on first use)
    let (conversation_id, session_db) = registry
        .get_or_create_session_db(&request.transport_id)
        .await?;

    // Load session from DB
    let mut session = Session::new(conversation_id, session_db.clone()).await;

    // Resolve agent
    let agent = registry
        .resolve_agent(&request.transport_id, request.agent_override.as_deref())
        .await;
    let agent_name = agent.name.clone();
    let default_role = agent.default_role.clone();
    let default_model = agent.default_model.clone();
    let allowed_tools = agent.allowed_tools.clone();
    let max_call_depth = agent.max_iterations as usize;

    // Backfill from gateway history if provided
    if let Some(history) = request.backfill_history {
        session.backfill(history).await;
    }

    // Add user message to session
    session
        .add_message(SessionMessage {
            role: "user".into(),
            content: request.body.clone(),
            sender: request.sender.clone(),
            timestamp: Utc::now(),
        })
        .await;

    // Acquire semaphore before LLM call
    let _permit = semaphore.acquire().await.expect("semaphore closed");

    // Build context and run LLM
    let role = request.role_override.or(default_role);
    let model = request.model_override.or(default_model);
    let context = session.build_context(role, model);

    let filtered = tools.filtered_view(allowed_tools.as_deref());

    let request_security = SecurityContext {
        leak_detector: security.leak_detector.clone(),
        auto_approved_tools: security.auto_approved_tools.clone(),
        approval_callback: request.approval_tx,
    };

    let tool_ctx = ToolContext {
        agent_name: agent_name.clone(),
        call_depth: 0,
        max_call_depth,
        agent_registry: agent_registry.clone(),
        tool_registry: tools.clone(),
        backend: request.backend.clone(),
        security: request_security.clone(),
        database: session_db,
    };

    let result =
        runtime::execute(&context, &request.backend, &filtered, &request_security, &tool_ctx)
            .await;

    // Release permit before session write + response send
    drop(_permit);

    match result {
        Ok(body) => {
            session
                .add_message(SessionMessage {
                    role: "assistant".into(),
                    content: body.clone(),
                    sender: agent_name,
                    timestamp: Utc::now(),
                })
                .await;
            let _ = request.response_tx.send(ChatResponse::Message {
                body,
                is_markdown: true,
            });
        }
        Err(err) => {
            error!("Runtime error: {err}");
            let _ = request.response_tx.send(ChatResponse::Error { error: err });
        }
    }

    Ok(())
}
