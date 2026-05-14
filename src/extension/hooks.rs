//! Per-event hook traits. Each trait covers one lifecycle event; extensions
//! implement only the ones they care about and register them with
//! [`ExtensionHub`](super::ExtensionHub).
//!
//! All hook methods return boxed futures to match chaz's existing
//! [`Tool`](crate::tool::Tool) trait shape. This keeps the runtime
//! object-safe without pulling in `async_trait`.

use super::{HookContext, ToolCallDecision};
use crate::runtime::RuntimeMessage;
use std::future::Future;
use std::pin::Pin;

/// Fires once per agent turn, after the runtime has assembled the initial
/// message list but before the first LLM call. Returned messages are
/// appended to the conversation in registration order.
///
/// Equivalent to pi's `before_agent_start` event.
pub trait HookBeforeAgentStart: Send + Sync {
    fn on_before_agent_start<'a>(
        &'a self,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = Vec<RuntimeMessage>> + Send + 'a>>;
}

/// Fires before each tool call inside the ReAct loop. Handlers may mutate
/// `args` in place (e.g. canonicalizing paths) or return
/// [`ToolCallDecision::Block`] to skip the call.
///
/// Equivalent to pi's `tool_call` event.
pub trait HookToolCall: Send + Sync {
    fn on_tool_call<'a>(
        &'a self,
        ctx: &'a HookContext,
        tool_name: &'a str,
        args: &'a mut serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolCallDecision> + Send + 'a>>;
}

/// Fires after each tool call, after the tool returns successfully but
/// before output sanitization / leak detection. Handlers can transform the
/// output string; the result is fed to the next handler.
///
/// Equivalent to pi's `tool_result` event.
pub trait HookToolResult: Send + Sync {
    fn on_tool_result<'a>(
        &'a self,
        ctx: &'a HookContext,
        tool_name: &'a str,
        result: String,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;
}

/// Fires when the ReAct loop produces a final assistant response, just
/// before the runtime returns. Fire-and-forget.
///
/// Equivalent to pi's `agent_end` event.
pub trait HookAgentEnd: Send + Sync {
    fn on_agent_end<'a>(
        &'a self,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Fires when a session is registered with the server (both top-level and
/// spawn-children). Extensions can filter on `ctx.call_depth == 0` to
/// react to top-level sessions only.
///
/// Equivalent to pi's `session_start` event.
pub trait HookSessionStart: Send + Sync {
    fn on_session_start<'a>(
        &'a self,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Fires when a session is explicitly deregistered. Best-effort: process
/// exit / abnormal termination skips this hook.
///
/// Equivalent to pi's `session_shutdown` event.
pub trait HookSessionShutdown: Send + Sync {
    fn on_session_shutdown<'a>(
        &'a self,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}
