//! Adapters that wrap the instance-published hook handlers in the legacy
//! `HookXxx` trait shape so the existing `ExtensionHub::fire_*` paths can
//! keep firing them unchanged.
//!
//! Used by `ExtensionHub::install_all` after collecting each extension's
//! [`crate::extension::handler::InstalledExtension`]: every
//! `Option<Box<dyn HookHandler...>>` slot moves into one of these
//! adapters, which gets pushed into the legacy per-kind `Vec` the hub
//! already iterates at fire time.
//!
//! The adapters ignore the legacy [`crate::extension::HookContext`] —
//! the handler traits take no context argument. Stays small on purpose:
//! the legacy hooks module is on a deletion path.

use crate::extension::HookContext;
use crate::extension::ToolCallDecision;
use crate::extension::handler::{
    HookHandlerAgentEnd, HookHandlerBeforeAgentStart, HookHandlerSessionShutdown,
    HookHandlerSessionStart, HookHandlerToolCall, HookHandlerToolResult,
};
use crate::extension::hooks::{
    HookAgentEnd, HookBeforeAgentStart, HookSessionShutdown, HookSessionStart, HookToolCall,
    HookToolResult,
};
use crate::runtime::RuntimeMessage;
use std::future::Future;
use std::pin::Pin;

pub(crate) struct BeforeAgentStartAdapter {
    inner: Box<dyn HookHandlerBeforeAgentStart>,
}

impl BeforeAgentStartAdapter {
    pub(crate) fn new(inner: Box<dyn HookHandlerBeforeAgentStart>) -> Self {
        Self { inner }
    }
}

impl HookBeforeAgentStart for BeforeAgentStartAdapter {
    fn on_before_agent_start<'a>(
        &'a self,
        _ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = Vec<RuntimeMessage>> + Send + 'a>> {
        self.inner.on_before_agent_start()
    }
}

pub(crate) struct ToolCallAdapter {
    inner: Box<dyn HookHandlerToolCall>,
}

impl ToolCallAdapter {
    pub(crate) fn new(inner: Box<dyn HookHandlerToolCall>) -> Self {
        Self { inner }
    }
}

impl HookToolCall for ToolCallAdapter {
    fn on_tool_call<'a>(
        &'a self,
        _ctx: &'a HookContext,
        tool_name: &'a str,
        args: &'a mut serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>> {
        self.inner.on_tool_call(tool_name, args)
    }
}

pub(crate) struct ToolResultAdapter {
    inner: Box<dyn HookHandlerToolResult>,
}

impl ToolResultAdapter {
    pub(crate) fn new(inner: Box<dyn HookHandlerToolResult>) -> Self {
        Self { inner }
    }
}

impl HookToolResult for ToolResultAdapter {
    fn on_tool_result<'a>(
        &'a self,
        _ctx: &'a HookContext,
        tool_name: &'a str,
        result: String,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        self.inner.on_tool_result(tool_name, result)
    }
}

pub(crate) struct AgentEndAdapter {
    inner: Box<dyn HookHandlerAgentEnd>,
}

impl AgentEndAdapter {
    pub(crate) fn new(inner: Box<dyn HookHandlerAgentEnd>) -> Self {
        Self { inner }
    }
}

impl HookAgentEnd for AgentEndAdapter {
    fn on_agent_end<'a>(
        &'a self,
        _ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.inner.on_agent_end()
    }
}

pub(crate) struct SessionStartAdapter {
    inner: Box<dyn HookHandlerSessionStart>,
}

impl SessionStartAdapter {
    pub(crate) fn new(inner: Box<dyn HookHandlerSessionStart>) -> Self {
        Self { inner }
    }
}

impl HookSessionStart for SessionStartAdapter {
    fn on_session_start<'a>(
        &'a self,
        _ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.inner.on_session_start()
    }
}

pub(crate) struct SessionShutdownAdapter {
    inner: Box<dyn HookHandlerSessionShutdown>,
}

impl SessionShutdownAdapter {
    pub(crate) fn new(inner: Box<dyn HookHandlerSessionShutdown>) -> Self {
        Self { inner }
    }
}

impl HookSessionShutdown for SessionShutdownAdapter {
    fn on_session_shutdown<'a>(
        &'a self,
        _ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.inner.on_session_shutdown()
    }
}
