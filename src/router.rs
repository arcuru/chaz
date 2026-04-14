use crate::agent::AgentRegistry;
use crate::gateway::{ChatRequest, ChatResponse};
use crate::runtime;
use crate::security::SecurityContext;
use crate::session::{SessionManager, SessionMessage};
use crate::tool::{ToolContext, ToolRegistry};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

/// Run the router, processing chat requests sequentially with session management.
///
/// Drains any immediately-buffered requests after receiving one, grouping by
/// conversation. For each conversation in the batch, all messages are added to
/// the session but only the last message triggers an LLM response. Earlier
/// messages get `ChatResponse::Skipped`. This prevents duplicate responses
/// when multiple messages arrive at once (e.g., after a restart).
pub async fn run(
    mut event_rx: mpsc::Receiver<ChatRequest>,
    mut sessions: SessionManager,
    tools: Arc<ToolRegistry>,
    agent_registry: Arc<AgentRegistry>,
    security: SecurityContext,
) {
    while let Some(first) = event_rx.recv().await {
        // Drain any immediately-available requests to batch them
        let mut batch = vec![first];
        while let Ok(req) = event_rx.try_recv() {
            batch.push(req);
        }

        if batch.len() > 1 {
            info!("Batching {} messages across conversations", batch.len());
        }

        // Group requests by transport_id (conversation)
        let mut by_conversation: HashMap<String, Vec<ChatRequest>> = HashMap::new();
        for req in batch {
            by_conversation
                .entry(req.transport_id.clone())
                .or_default()
                .push(req);
        }

        // Process each conversation's batch
        for (_, requests) in by_conversation {
            let last_idx = requests.len() - 1;

            for (i, request) in requests.into_iter().enumerate() {
                let is_last = i == last_idx;

                // Resolve transport ID to a conversation
                let conversation_id = sessions.resolve_conversation(&request.transport_id);

                // Select agent: explicit override > conversation binding > default
                let agent = sessions.resolve_agent(
                    &conversation_id,
                    request.agent_override.as_deref(),
                );
                let default_role = agent.default_role.clone();
                let default_model = agent.default_model.clone();
                let allowed_tools = agent.allowed_tools.clone();
                let agent_name = agent.name.clone();
                let max_call_depth = agent.max_iterations as usize; // use max_iterations as depth limit for now
                let database = sessions.database().clone();
                let session = sessions.get_or_create(&conversation_id).await;

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

                if !is_last {
                    // Not the last message in this batch — skip LLM, just add to session
                    info!(
                        "Batched message from {} (will respond to later message)",
                        request.sender
                    );
                    let _ = request.response_tx.send(ChatResponse::Skipped);
                    continue;
                }

                // Last message in batch — run LLM with full session context
                let role = request.role_override.or(default_role);
                let model = request.model_override.or(default_model);
                let context = session.build_context(role, model);

                let filtered = tools.filtered_view(allowed_tools.as_deref());

                // Build per-request security context with the gateway's approval channel
                let request_security = SecurityContext {
                    leak_detector: security.leak_detector.clone(),
                    auto_approved_tools: security.auto_approved_tools.clone(),
                    approval_callback: request.approval_tx,
                };

                // Build tool context for this request
                let tool_ctx = ToolContext {
                    agent_name: agent_name.clone(),
                    call_depth: 0,
                    max_call_depth,
                    agent_registry: agent_registry.clone(),
                    tool_registry: tools.clone(),
                    backend: request.backend.clone(),
                    security: request_security.clone(),
                    database: database.clone(),
                };

                let result =
                    runtime::execute(&context, &request.backend, &filtered, &request_security, &tool_ctx)
                        .await;

                match result {
                    Ok(body) => {
                        session
                            .add_message(SessionMessage {
                                role: "assistant".into(),
                                content: body.clone(),
                                sender: agent_name.clone(),
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
            }
        }
    }
}
