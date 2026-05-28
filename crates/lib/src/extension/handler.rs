//! Hook + routine dispatch handler traits.
//!
//! These are the runtime handler shapes an [`crate::extension::instance::
//! ExtensionInstance`] publishes through its `*_hook()` / `routine_handler()`
//! endpoints. The hub drains them at install time and fires them from the
//! ReAct loop and the routine engine.
//!
//! # Per-kind traits, not a unified `HookHandler`
//!
//! We split per-kind rather than one `handle(HookEvent) -> HookOutcome`:
//!
//! * type safety — each handler's return shape is precise (e.g.
//!   `tool_call` returns `ToolCallDecision`, not "an outcome that
//!   might be a decision")
//! * easier authoring — an extension that only cares about
//!   `tool_call` implements one trait and the others are absent
//! * matches chaz's existing convention (one trait per hook kind)
//!
//! [`InstalledExtension`] holds an `Option<Box<dyn ...>>` per kind;
//! `None` means "this extension doesn't handle this kind."

use crate::extension::ToolCallDecision;
use crate::runtime::RuntimeMessage;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed future returned by every handler method.
pub type HandlerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// =========================================================================
// Per-kind hook handlers
// =========================================================================

/// Fires once per agent turn, after the runtime has assembled the
/// initial message list but before the first LLM call. Returned
/// messages are appended in registration order.
pub trait HookHandlerBeforeAgentStart: Send + Sync {
    fn on_before_agent_start<'a>(&'a self) -> HandlerFuture<'a, Vec<RuntimeMessage>>;
}

/// Fires before each tool call inside the ReAct loop. Handlers may
/// mutate `args` in place. The first `Block` short-circuits.
pub trait HookHandlerToolCall: Send + Sync {
    fn on_tool_call<'a>(
        &'a self,
        tool_name: &'a str,
        args: &'a mut Value,
    ) -> HandlerFuture<'a, ToolCallDecision>;
}

/// Fires after each tool call returns. Handlers may transform the
/// output string; the transformed value flows into the next handler.
pub trait HookHandlerToolResult: Send + Sync {
    fn on_tool_result<'a>(
        &'a self,
        tool_name: &'a str,
        result: String,
    ) -> HandlerFuture<'a, String>;
}

/// Fires when the ReAct loop produces a final assistant response,
/// just before the runtime returns. Fire-and-forget.
pub trait HookHandlerAgentEnd: Send + Sync {
    fn on_agent_end<'a>(&'a self) -> HandlerFuture<'a, ()>;
}

/// Fires when a session is registered with the server.
pub trait HookHandlerSessionStart: Send + Sync {
    fn on_session_start<'a>(&'a self) -> HandlerFuture<'a, ()>;
}

/// Fires when a session is explicitly deregistered. Best-effort —
/// process exit / abnormal termination skips this hook.
pub trait HookHandlerSessionShutdown: Send + Sync {
    fn on_session_shutdown<'a>(&'a self) -> HandlerFuture<'a, ()>;
}

// =========================================================================
// Arc-to-Box adapters
// =========================================================================
//
// `ExtensionInstance` endpoints (`tool_call_hook()`, …) return
// `Option<Arc<dyn HookHandler*>>` so the instance keeps ownership and
// the hub clones the handle into its dispatch path. The hub's
// `install_all` drain feeds these handles into Box-shaped adapters in
// `hook_bridge`, so each handler trait gets a blanket impl over
// `Arc<dyn HookHandler*>`. Boxing the Arc then satisfies the
// `Box<dyn HookHandler*>` slot on `InstalledExtension`.

impl HookHandlerBeforeAgentStart for Arc<dyn HookHandlerBeforeAgentStart> {
    fn on_before_agent_start<'a>(&'a self) -> HandlerFuture<'a, Vec<RuntimeMessage>> {
        (**self).on_before_agent_start()
    }
}

impl HookHandlerToolCall for Arc<dyn HookHandlerToolCall> {
    fn on_tool_call<'a>(
        &'a self,
        tool_name: &'a str,
        args: &'a mut Value,
    ) -> HandlerFuture<'a, ToolCallDecision> {
        (**self).on_tool_call(tool_name, args)
    }
}

impl HookHandlerToolResult for Arc<dyn HookHandlerToolResult> {
    fn on_tool_result<'a>(
        &'a self,
        tool_name: &'a str,
        result: String,
    ) -> HandlerFuture<'a, String> {
        (**self).on_tool_result(tool_name, result)
    }
}

impl HookHandlerAgentEnd for Arc<dyn HookHandlerAgentEnd> {
    fn on_agent_end<'a>(&'a self) -> HandlerFuture<'a, ()> {
        (**self).on_agent_end()
    }
}

impl HookHandlerSessionStart for Arc<dyn HookHandlerSessionStart> {
    fn on_session_start<'a>(&'a self) -> HandlerFuture<'a, ()> {
        (**self).on_session_start()
    }
}

impl HookHandlerSessionShutdown for Arc<dyn HookHandlerSessionShutdown> {
    fn on_session_shutdown<'a>(&'a self) -> HandlerFuture<'a, ()> {
        (**self).on_session_shutdown()
    }
}

impl RoutineHandler for Arc<dyn RoutineHandler> {
    fn on_fire<'a>(&'a self, payload: Value) -> HandlerFuture<'a, anyhow::Result<()>> {
        (**self).on_fire(payload)
    }
}

// =========================================================================
// Routine handler
// =========================================================================

/// Fires when the routine engine dispatches one routine targeted at
/// this extension. `payload` is the extension-defined opaque value
/// carried on the routine — the engine itself never inspects it.
///
/// One handler per extension (extensions handle their own routines).
pub trait RoutineHandler: Send + Sync {
    fn on_fire<'a>(&'a self, payload: Value) -> HandlerFuture<'a, anyhow::Result<()>>;
}

// =========================================================================
// InstalledExtension
// =========================================================================

/// What the hub collects from an extension's instance endpoints.
///
/// Each hook-kind slot is `Option<Box<dyn HookHandler...>>`; `None`
/// means the extension doesn't handle that kind. The `routine_handler`
/// slot covers the routine engine's dispatch.
#[derive(Default)]
pub struct InstalledExtension {
    pub before_agent_start: Option<Box<dyn HookHandlerBeforeAgentStart>>,
    pub tool_call: Option<Box<dyn HookHandlerToolCall>>,
    pub tool_result: Option<Box<dyn HookHandlerToolResult>>,
    pub agent_end: Option<Box<dyn HookHandlerAgentEnd>>,
    pub session_start: Option<Box<dyn HookHandlerSessionStart>>,
    pub session_shutdown: Option<Box<dyn HookHandlerSessionShutdown>>,
    pub routine_handler: Option<Box<dyn RoutineHandler>>,
    /// Per-handler bookkeeping the hub uses for tracing / `/extensions
    /// list -v`. Filled by the hub at install time, not by extensions.
    pub _handler_count: usize,
}

impl InstalledExtension {
    /// Convenience: an installed extension that registered nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// `true` when no handlers were registered (the empty case).
    pub fn is_empty(&self) -> bool {
        self.before_agent_start.is_none()
            && self.tool_call.is_none()
            && self.tool_result.is_none()
            && self.agent_end.is_none()
            && self.session_start.is_none()
            && self.session_shutdown.is_none()
            && self.routine_handler.is_none()
    }
}

impl std::fmt::Debug for InstalledExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted: bool per slot, never the boxed payload.
        f.debug_struct("InstalledExtension")
            .field("before_agent_start", &self.before_agent_start.is_some())
            .field("tool_call", &self.tool_call.is_some())
            .field("tool_result", &self.tool_result.is_some())
            .field("agent_end", &self.agent_end.is_some())
            .field("session_start", &self.session_start.is_some())
            .field("session_shutdown", &self.session_shutdown.is_some())
            .field("routine_handler", &self.routine_handler.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_installed_extension_has_no_handlers() {
        let i = InstalledExtension::empty();
        assert!(i.is_empty());
        assert!(i.before_agent_start.is_none());
        assert!(i.tool_call.is_none());
        assert!(i.routine_handler.is_none());
    }

    struct StubBeforeStart;
    impl HookHandlerBeforeAgentStart for StubBeforeStart {
        fn on_before_agent_start<'a>(&'a self) -> HandlerFuture<'a, Vec<RuntimeMessage>> {
            Box::pin(async { vec![RuntimeMessage::System("hi".into())] })
        }
    }

    #[test]
    fn installed_extension_with_one_handler_flips_is_empty() {
        let mut i = InstalledExtension::empty();
        assert!(i.is_empty());
        i.before_agent_start = Some(Box::new(StubBeforeStart));
        assert!(!i.is_empty());
        let dbg = format!("{i:?}");
        assert!(dbg.contains("before_agent_start: true"), "{dbg}");
        assert!(dbg.contains("tool_call: false"), "{dbg}");
    }

    #[tokio::test]
    async fn handler_method_returns_async_value_through_handler_future() {
        let h = StubBeforeStart;
        let msgs = h.on_before_agent_start().await;
        assert_eq!(msgs.len(), 1);
    }

    struct StubRoutine;
    impl RoutineHandler for StubRoutine {
        fn on_fire<'a>(&'a self, payload: Value) -> HandlerFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                // Just confirm the payload arrives unchanged — engine never
                // inspects it.
                assert_eq!(payload, serde_json::json!({"task": "ping"}));
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn routine_handler_receives_opaque_payload() {
        let r = StubRoutine;
        r.on_fire(serde_json::json!({"task": "ping"}))
            .await
            .unwrap();
    }
}
