use crate::gateway::{ChatRequest, ChatResponse};
use crate::runtime;
use crate::session::{SessionManager, SessionMessage};
use crate::tool::ToolRegistry;
use chrono::Utc;
use tokio::sync::mpsc;
use tracing::error;

/// Run the router, processing chat requests sequentially with session management.
///
/// Each request: add to session → build context → execute (with ReAct loop) → store response → reply.
pub async fn run(
    mut event_rx: mpsc::Receiver<ChatRequest>,
    mut sessions: SessionManager,
    tools: ToolRegistry,
) {
    while let Some(request) = event_rx.recv().await {
        // Extract agent defaults before borrowing sessions mutably
        let default_role = sessions.agent.default_role.clone();
        let default_model = sessions.agent.default_model.clone();

        let session = sessions.get_or_create(&request.conversation_id).await;

        // Add user message to session
        session
            .add_message(SessionMessage {
                role: "user".into(),
                content: request.body.clone(),
                sender: request.sender.clone(),
                timestamp: Utc::now(),
            })
            .await;

        // Build context from session history + gateway overrides + agent defaults
        let role = request.role_override.or(default_role);
        let model = request.model_override.or(default_model);
        let context = session.build_context(role, model);

        // Execute via runtime (handles ReAct loop if tools available)
        let result = runtime::execute(&context, &request.backend, &tools).await;

        match result {
            Ok(body) => {
                session
                    .add_message(SessionMessage {
                        role: "assistant".into(),
                        content: body.clone(),
                        sender: "chaz".into(),
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
