//! Agent-owned timer extension — the routine handler for
//! [`crate::routine::AgentTimerPayload`].
//!
//! The routine engine fires agent-owned timers and dispatches them to
//! this extension's routine handler. Unlike heartbeat/scheduler, which
//! write a Directive entry and let `process_session` handle the turn,
//! agent timers use a **standalone execution path**: load the agent,
//! build context, run the ReAct loop directly via
//! [`crate::runtime::execute`], write results, and attribute cost to
//! the agent's `timer_fires` store.
//!
//! The handler spawns a `tokio` task for the actual agent turn so the
//! engine's fire loop isn't blocked on LLM latency.

use crate::extension::caps::ExtensionCaps;
use crate::extension::handler::{HandlerFuture, InstalledExtension, RoutineHandler};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::server::Server;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

pub struct AgentTimerExtension {
    server_cell: Arc<OnceLock<Arc<Server>>>,
}

impl AgentTimerExtension {
    pub fn new(server_cell: Arc<OnceLock<Arc<Server>>>) -> Self {
        Self { server_cell }
    }
}

impl Extension for AgentTimerExtension {
    fn name(&self) -> &'static str {
        "agent_timer"
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

    fn install<'a>(
        &'a self,
        _caps: ExtensionCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<InstalledExtension>> + Send + 'a>> {
        Box::pin(async move {
            let mut installed = InstalledExtension::empty();
            installed.routine_handler = Some(Box::new(AgentTimerRoutineHandler {
                server_cell: self.server_cell.clone(),
            }));
            Ok(installed)
        })
    }
}

struct AgentTimerRoutineHandler {
    server_cell: Arc<OnceLock<Arc<Server>>>,
}

impl RoutineHandler for AgentTimerRoutineHandler {
    fn on_fire<'a>(
        &'a self,
        _caps: &'a ExtensionCaps,
        payload: serde_json::Value,
    ) -> HandlerFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let payload: crate::routine::AgentTimerPayload = serde_json::from_value(payload)
                .map_err(|e| anyhow::anyhow!("invalid agent_timer payload: {e}"))?;

            let server = self
                .server_cell
                .get()
                .ok_or_else(|| anyhow::anyhow!("agent_timer fired before server initialized"))?
                .clone();

            // Spawn the actual agent turn — don't block the engine's fire loop.
            tokio::spawn(async move {
                if let Err(e) = server.fire_agent_timer(payload).await {
                    tracing::error!(error = %e, "agent_timer fire failed");
                }
            });

            Ok(())
        })
    }
}
