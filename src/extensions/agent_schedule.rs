//! Agent-owned schedule extension — the routine handler for
//! [`crate::routine::AgentSchedulePayload`].
//!
//! The routine engine fires agent-owned schedules and dispatches them to
//! this extension's routine handler. Unlike the legacy session-routine path, which
//! write a Directive entry and let `process_session` handle the turn,
//! agent schedules use a **standalone execution path**: load the agent,
//! build context, run the ReAct loop directly via
//! [`crate::runtime::execute`], write results, and attribute cost to
//! the agent's `schedule_fires` store.
//!
//! The handler spawns a `tokio` task for the actual agent turn so the
//! engine's fire loop isn't blocked on LLM latency.

use crate::extension::caps::ExtensionCaps;
use crate::extension::handler::{HandlerFuture, RoutineHandler};
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::server::Server;
use std::sync::{Arc, OnceLock};

pub struct AgentScheduleExtension {
    server_cell: Arc<OnceLock<Arc<Server>>>,
}

impl AgentScheduleExtension {
    pub fn new(server_cell: Arc<OnceLock<Arc<Server>>>) -> Self {
        Self { server_cell }
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
        let server_cell = self.server_cell.clone();
        Box::pin(async move {
            Ok(Arc::new(AgentScheduleInstance {
                manifest,
                server_cell,
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct AgentScheduleInstance {
    manifest: ExtensionManifest,
    server_cell: Arc<OnceLock<Arc<Server>>>,
}

impl ExtensionInstance for AgentScheduleInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn routine_handler(&self) -> Option<Arc<dyn RoutineHandler>> {
        Some(Arc::new(AgentScheduleRoutineHandler {
            server_cell: self.server_cell.clone(),
        }))
    }
}

struct AgentScheduleRoutineHandler {
    server_cell: Arc<OnceLock<Arc<Server>>>,
}

impl RoutineHandler for AgentScheduleRoutineHandler {
    fn on_fire<'a>(
        &'a self,
        _caps: &'a ExtensionCaps,
        payload: serde_json::Value,
    ) -> HandlerFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let payload: crate::routine::AgentSchedulePayload = serde_json::from_value(payload)
                .map_err(|e| anyhow::anyhow!("invalid agent_schedule payload: {e}"))?;

            let server = self
                .server_cell
                .get()
                .ok_or_else(|| anyhow::anyhow!("agent_schedule fired before server initialized"))?
                .clone();

            // Spawn the actual agent turn — don't block the engine's fire loop.
            tokio::spawn(async move {
                if let Err(e) = server.fire_agent_schedule(payload).await {
                    tracing::error!(error = %e, "agent_schedule fire failed");
                }
            });

            Ok(())
        })
    }
}
