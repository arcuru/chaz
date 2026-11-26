//! Agent-owned schedule extension — the routine handler for
//! [`crate::routine::AgentSchedulePayload`].
//!
//! The routine engine supplies an exact disarmed slot and generation. The
//! handler prepares a private reference and acknowledges committed operational
//! handoff; ordered discovery then uses the existing input/turn runner and
//! capacity. Neither acknowledgement nor clock advancement means Started,
//! Included, terminal success or fulfillment.

use crate::extension::handler::{HandlerFuture, RoutineHandler};
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use std::sync::Arc;

/// Test-only barrier before bounded admission and its first authority guard.
/// Handler acknowledgement must not retire the clock token before admission.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ScheduleDispatchBarrier {
    pub parked: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub finished: tokio::sync::Notify,
    pub payload: std::sync::Mutex<Option<crate::routine::AgentSchedulePayload>>,
    pub result: std::sync::Mutex<Option<Result<(), String>>>,
}

pub struct AgentScheduleExtension {
    server_slot: crate::instance::ServerSlot,
    #[cfg(test)]
    dispatch_barrier: Option<Arc<ScheduleDispatchBarrier>>,
}

impl AgentScheduleExtension {
    pub fn new(server_slot: crate::instance::ServerSlot) -> Self {
        Self {
            server_slot,
            #[cfg(test)]
            dispatch_barrier: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_dispatch_barrier(
        mut self,
        dispatch_barrier: Arc<ScheduleDispatchBarrier>,
    ) -> Self {
        self.dispatch_barrier = Some(dispatch_barrier);
        self
    }
}

impl Extension for AgentScheduleExtension {
    fn name(&self) -> &'static str {
        "agent_schedule"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: Vec::new(),
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn instantiate<'a>(&'a self, _scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        let server_slot = self.server_slot.clone();
        #[cfg(test)]
        let dispatch_barrier = self.dispatch_barrier.clone();
        Box::pin(async move {
            Ok(Arc::new(AgentScheduleInstance {
                manifest,
                server_slot,
                #[cfg(test)]
                dispatch_barrier,
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct AgentScheduleInstance {
    manifest: ExtensionManifest,
    server_slot: crate::instance::ServerSlot,
    #[cfg(test)]
    dispatch_barrier: Option<Arc<ScheduleDispatchBarrier>>,
}

impl ExtensionInstance for AgentScheduleInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn routine_handler(&self) -> Option<Arc<dyn RoutineHandler>> {
        Some(Arc::new(AgentScheduleRoutineHandler {
            server_slot: self.server_slot.clone(),
            #[cfg(test)]
            dispatch_barrier: self.dispatch_barrier.clone(),
        }))
    }
}

struct AgentScheduleRoutineHandler {
    server_slot: crate::instance::ServerSlot,
    #[cfg(test)]
    dispatch_barrier: Option<Arc<ScheduleDispatchBarrier>>,
}

impl RoutineHandler for AgentScheduleRoutineHandler {
    fn on_fire<'a>(&'a self, payload: serde_json::Value) -> HandlerFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let payload: crate::routine::AgentSchedulePayload = serde_json::from_value(payload)
                .map_err(|e| anyhow::anyhow!("invalid agent_schedule payload: {e}"))?;

            let server = self
                .server_slot
                .get()
                .ok_or_else(|| anyhow::anyhow!("agent_schedule fired before server initialized"))?;

            #[cfg(test)]
            if let Some(barrier) = &self.dispatch_barrier {
                *barrier.payload.lock().unwrap() = Some(payload.clone());
                barrier.parked.notify_one();
                barrier.release.notified().await;
            }
            let result = server.dispatch_agent_schedule(payload).await;
            #[cfg(test)]
            if let Some(barrier) = &self.dispatch_barrier {
                *barrier.result.lock().unwrap() =
                    Some(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                barrier.finished.notify_one();
            }
            result
        })
    }
}
