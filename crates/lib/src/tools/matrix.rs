use crate::session::{
    BridgeEventRole, EntryRouting, SessionEntry, TransportRef, is_bound, session_attachment,
};
use crate::session::{Session, SessionRegistry};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The owning agent must be attached to this session and have registered the
/// bound Matrix login. A room binding alone never grants sending authority.
pub(crate) async fn ensure_agent_owns_login(
    registry: &SessionRegistry,
    session: &Session,
    agent_name: &str,
    transport: &str,
    login: &str,
) -> Result<(), ToolError> {
    let meta = session.read_meta().await;
    let agent = meta
        .agents
        .iter()
        .find(|agent| agent.display_name == agent_name)
        .ok_or_else(|| {
            ToolError::Execution("executing agent is not attached to this session".into())
        })?;
    let agent_id = eidetica::entry::ID::parse(&agent.db_id)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let adb = registry
        .open_agent_db(&agent_id, None)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?
        .ok_or_else(|| ToolError::Execution("agent identity is unavailable".into()))?;
    let owns_login = adb
        .list_logins()
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?
        .iter()
        .any(|candidate| candidate.kind == transport && candidate.identifier == login);
    if !owns_login {
        return Err(ToolError::Execution(
            "this transport login does not belong to the executing agent".into(),
        ));
    }
    Ok(())
}

/// One addressed durable outbox row, whether produced by an explicit tool
/// or by an external-origin normal final at turn completion.
pub(crate) fn addressed_send_entry(
    agent_name: &str,
    body: &str,
    transport: &str,
    login: &str,
    room: &str,
) -> (SessionEntry, String) {
    let id = uuid::Uuid::new_v4().to_string();
    (
        SessionEntry::new_bridge_event(
            agent_name,
            BridgeEventRole::Outbound,
            body,
            Some(EntryRouting {
                destinations: vec![TransportRef {
                    transport: transport.to_owned(),
                    login_id: login.to_owned(),
                    channel: room.to_owned(),
                    sender: None,
                    sender_display: None,
                    message_id: Some(id.clone()),
                }],
                ..Default::default()
            }),
        ),
        id,
    )
}
/// Enqueue a send to the single Matrix room attached to this session.
/// The separate Matrix peer performs the actual transport operation.
pub struct MatrixSend {
    pub registry: Arc<SessionRegistry>,
}

impl Tool for MatrixSend {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "matrix__send".into(),
            description: "Post an explicit or proactive message to this session's attached Matrix room under your own Matrix login. A normal final on a Matrix-origin turn is already posted automatically; using this tool for that turn counts as its room reply and keeps the later final local. Fails when unattached or when the login is not yours.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"body": {"type": "string", "description": "Text to post to Matrix"}},
                "required": ["body"],
                "additionalProperties": false
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let body = arguments
                .get("body")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| ToolError::InvalidArgument("body must be nonempty text".into()))?;
            let mut session = ctx.session.lock().await;
            let db = session.database();
            let attachment = session_attachment(db)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?
                .filter(|a| a.transport == "matrix" && a.conversational_replies)
                .ok_or_else(|| {
                    ToolError::Execution(
                        "session must have exactly one conversational Matrix attachment".into(),
                    )
                })?;
            let login = &attachment.login_id;
            let room = &attachment.channel;
            if !is_bound(db, "matrix", login, room)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?
            {
                return Err(ToolError::Execution(
                    "Matrix attachment is no longer valid".into(),
                ));
            }
            ensure_agent_owns_login(&self.registry, &session, &ctx.agent_name, "matrix", login)
                .await?;
            let (entry, id) = addressed_send_entry(&ctx.agent_name, body, "matrix", login, room);
            session
                .add_entry(entry)
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            Ok(format!(
                "Matrix send queued ({id}); a separate bridge confirms delivery."
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_db::LoginRef;
    use crate::session::test_helpers::{make_agent_entry, make_registry};
    use crate::test_support::{fresh_session, tool_context};
    use crate::tool::ToolRegistry;

    #[tokio::test]
    async fn send_is_owned_attached_and_durable_without_a_chat_wake() {
        let (_instance, registry) = make_registry().await;
        let alpha = make_agent_entry(&registry, "alpha").await;
        let beta = make_agent_entry(&registry, "beta").await;
        let (_cv, db) = registry.create_session(Some("local")).await.unwrap();
        let sid = db.root_id().to_string();
        registry
            .attach_agent_to_session(&sid, &alpha)
            .await
            .unwrap();
        registry.attach_agent_to_session(&sid, &beta).await.unwrap();
        let alpha_db = registry
            .open_agent_db(&alpha.db_id, None)
            .await
            .unwrap()
            .unwrap();
        alpha_db
            .register_login(LoginRef {
                kind: "matrix".into(),
                identifier: "@alpha:s".into(),
                bridge_db_id: db.root_id().to_string(),
                peer_pubkey: None,
                agent_pubkey: None,
                sync_addresses: Vec::new(),
            })
            .await
            .unwrap();
        let (_other, session) = fresh_session().await;
        let mut ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        ctx.agent_name = "alpha".into();
        ctx.session = Arc::new(tokio::sync::Mutex::new(
            crate::session::Session::new(crate::types::ConversationId(sid.clone()), db.clone())
                .await,
        ));
        let tool = MatrixSend { registry };
        let send = serde_json::json!({"body": "@beta hello"});
        assert!(
            tool.execute(send.clone(), &ctx)
                .await
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
        crate::session::bind_transport(&db, "matrix", "@alpha:s", "!room:s")
            .await
            .unwrap();
        ctx.agent_name = "beta".into();
        assert!(
            tool.execute(send.clone(), &ctx)
                .await
                .unwrap_err()
                .to_string()
                .contains("does not belong")
        );
        ctx.agent_name = "alpha".into();
        assert!(
            tool.execute(serde_json::json!({"body": " "}), &ctx)
                .await
                .is_err()
        );
        assert!(tool.execute(send, &ctx).await.unwrap().contains("queued"));
        let reopened =
            crate::session::Session::new(crate::types::ConversationId(sid), db.clone()).await;
        assert_eq!(reopened.entries().len(), 1);
        let entry = &reopened.entries()[0];
        assert_eq!(entry.bridge_role(), Some(BridgeEventRole::Outbound));
        assert_eq!(entry.bridge_body().as_deref(), Some("@beta hello"));
        assert_eq!(
            entry.routing.as_ref().unwrap().destinations[0].channel,
            "!room:s"
        );
        assert!(
            reopened
                .next_turn_request(
                    |name| name == "alpha" || name == "beta",
                    &Default::default()
                )
                .await
                .unwrap()
                .is_none(),
            "Matrix @mentions must not trigger a local agent-to-agent turn"
        );
        crate::session::unbind_transport(&db, "matrix", "@alpha:s", "!room:s")
            .await
            .unwrap();
        assert!(
            tool.execute(serde_json::json!({"body": "again"}), &ctx)
                .await
                .is_err()
        );
    }
}
