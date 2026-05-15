//! Adapters that wrap cap-based hook handlers in the legacy `HookXxx`
//! trait shape so the existing `ExtensionHub::fire_*` paths can keep
//! firing them unchanged.
//!
//! Used by `ExtensionHub::install_all` after collecting each
//! extension's [`crate::extension::handler::InstalledExtension`]: every
//! `Option<Box<dyn HookHandler...>>` slot moves into one of these
//! adapters, which gets pushed into the legacy per-kind `Vec` the
//! hub already iterates at fire time.
//!
//! The adapter builds a per-fire [`crate::extension::caps::ExtensionCaps`]
//! bundle from the legacy [`crate::extension::HookContext`] — populating
//! `session_read`, `session_write`, and `settings` from `ctx.session`
//! so cap-based handlers see the same session view their trait
//! signatures advertise. Messenger / memory cap routing is intentionally
//! left empty here; built-ins that consume those caps will gain
//! routing through the registry in a follow-up commit.
//!
//! Stays small on purpose: the legacy hooks module is on a deletion
//! path (commit F deletes `register()` and the per-kind trait
//! `HookToolCall` / `HookToolResult` / …).

use crate::extension::caps::ExtensionCaps;
use crate::extension::caps_inproc::{InProcSessionRead, InProcSessionWrite, InProcSettings};
use crate::extension::handler::{
    HookHandlerAgentEnd, HookHandlerBeforeAgentStart, HookHandlerSessionShutdown,
    HookHandlerSessionStart, HookHandlerToolCall, HookHandlerToolResult,
};
use crate::extension::hooks::{
    HookAgentEnd, HookBeforeAgentStart, HookSessionShutdown, HookSessionStart, HookToolCall,
    HookToolResult,
};
use crate::extension::{HookContext, ToolCallDecision};
use crate::runtime::RuntimeMessage;
use crate::session::Session;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Build a session-scoped caps bundle from a legacy [`HookContext`].
/// Locks the session briefly to extract the underlying `Database` for
/// `InProcSettings`, then drops the lock before returning so the inner
/// handler can re-lock without contention.
async fn caps_from_ctx(owner: &str, session: &Arc<Mutex<Session>>) -> ExtensionCaps {
    let db = {
        let s = session.lock().await;
        s.database().clone()
    };
    let mut caps = ExtensionCaps::empty();
    caps.session_read = Some(Arc::new(InProcSessionRead::new(session.clone())));
    caps.session_write = Some(Arc::new(InProcSessionWrite::new(session.clone(), owner)));
    caps.settings = Some(Arc::new(InProcSettings::new(db, owner)));
    caps
}

pub(crate) struct BeforeAgentStartAdapter {
    owner: &'static str,
    inner: Box<dyn HookHandlerBeforeAgentStart>,
}

impl BeforeAgentStartAdapter {
    pub(crate) fn new(owner: &'static str, inner: Box<dyn HookHandlerBeforeAgentStart>) -> Self {
        Self { owner, inner }
    }
}

impl HookBeforeAgentStart for BeforeAgentStartAdapter {
    fn on_before_agent_start<'a>(
        &'a self,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = Vec<RuntimeMessage>> + Send + 'a>> {
        Box::pin(async move {
            let caps = caps_from_ctx(self.owner, &ctx.session).await;
            self.inner.on_before_agent_start(&caps).await
        })
    }
}

pub(crate) struct ToolCallAdapter {
    owner: &'static str,
    inner: Box<dyn HookHandlerToolCall>,
}

impl ToolCallAdapter {
    pub(crate) fn new(owner: &'static str, inner: Box<dyn HookHandlerToolCall>) -> Self {
        Self { owner, inner }
    }
}

impl HookToolCall for ToolCallAdapter {
    fn on_tool_call<'a>(
        &'a self,
        ctx: &'a HookContext,
        tool_name: &'a str,
        args: &'a mut serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>> {
        Box::pin(async move {
            let caps = caps_from_ctx(self.owner, &ctx.session).await;
            self.inner.on_tool_call(&caps, tool_name, args).await
        })
    }
}

pub(crate) struct ToolResultAdapter {
    owner: &'static str,
    inner: Box<dyn HookHandlerToolResult>,
}

impl ToolResultAdapter {
    pub(crate) fn new(owner: &'static str, inner: Box<dyn HookHandlerToolResult>) -> Self {
        Self { owner, inner }
    }
}

impl HookToolResult for ToolResultAdapter {
    fn on_tool_result<'a>(
        &'a self,
        ctx: &'a HookContext,
        tool_name: &'a str,
        result: String,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async move {
            let caps = caps_from_ctx(self.owner, &ctx.session).await;
            self.inner.on_tool_result(&caps, tool_name, result).await
        })
    }
}

pub(crate) struct AgentEndAdapter {
    owner: &'static str,
    inner: Box<dyn HookHandlerAgentEnd>,
}

impl AgentEndAdapter {
    pub(crate) fn new(owner: &'static str, inner: Box<dyn HookHandlerAgentEnd>) -> Self {
        Self { owner, inner }
    }
}

impl HookAgentEnd for AgentEndAdapter {
    fn on_agent_end<'a>(
        &'a self,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let caps = caps_from_ctx(self.owner, &ctx.session).await;
            self.inner.on_agent_end(&caps).await
        })
    }
}

pub(crate) struct SessionStartAdapter {
    owner: &'static str,
    inner: Box<dyn HookHandlerSessionStart>,
}

impl SessionStartAdapter {
    pub(crate) fn new(owner: &'static str, inner: Box<dyn HookHandlerSessionStart>) -> Self {
        Self { owner, inner }
    }
}

impl HookSessionStart for SessionStartAdapter {
    fn on_session_start<'a>(
        &'a self,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let caps = caps_from_ctx(self.owner, &ctx.session).await;
            self.inner.on_session_start(&caps).await
        })
    }
}

pub(crate) struct SessionShutdownAdapter {
    owner: &'static str,
    inner: Box<dyn HookHandlerSessionShutdown>,
}

impl SessionShutdownAdapter {
    pub(crate) fn new(owner: &'static str, inner: Box<dyn HookHandlerSessionShutdown>) -> Self {
        Self { owner, inner }
    }
}

impl HookSessionShutdown for SessionShutdownAdapter {
    fn on_session_shutdown<'a>(
        &'a self,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let caps = caps_from_ctx(self.owner, &ctx.session).await;
            self.inner.on_session_shutdown(&caps).await
        })
    }
}
