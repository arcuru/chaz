//! Scheduler extension — the routine handler for YAML `schedules:`.
//!
//! Replaces the legacy `crate::scheduler::Scheduler` background task.
//! Each `ScheduleConfig` from chaz's YAML is translated into a session-
//! scoped [`crate::routine::Routine`] at startup (see `main.rs`) whose
//! `target.extension` is `"scheduler"`. The routine engine fires those
//! routines on schedule and dispatches them to
//! [`ScheduleRoutineHandler::on_fire`], which writes a Directive entry
//! to the calling session through `caps.session_write`.
//!
//! Payload: [`SchedulePayload`]. The schedule's name and task text ride
//! inside the routine target's payload — `cron` lives on the routine's
//! trigger, the session identity is implicit in the
//! `RoutineScope::Session(id)` the engine passes to dispatch, which the
//! hub uses to populate `caps.session_write` for the right session.

use crate::extension::caps::{CapabilityRequest, ExtensionCaps, SessionEntryDraft};
use crate::extension::handler::{HandlerFuture, InstalledExtension, RoutineHandler};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionHub, ExtensionRef, HookKind};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Routine payload for scheduler fires.
///
/// Carried verbatim inside `Routine.target.payload` by the routine
/// engine. The handler reads `schedule_name` for the directive's
/// display preamble and `task` for the directive body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulePayload {
    pub schedule_name: String,
    pub task: String,
}

pub struct ScheduleExtension;

impl Extension for ScheduleExtension {
    fn name(&self) -> &'static str {
        "scheduler"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }

    fn register(self: Arc<Self>, _hub: &mut ExtensionHub) {}

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: Vec::new(),
            required_capabilities: vec![CapabilityRequest::SessionWrite],
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
            installed.routine_handler = Some(Box::new(ScheduleRoutineHandler));
            Ok(installed)
        })
    }
}

pub struct ScheduleRoutineHandler;

impl RoutineHandler for ScheduleRoutineHandler {
    fn on_fire<'a>(
        &'a self,
        caps: &'a ExtensionCaps,
        payload: serde_json::Value,
    ) -> HandlerFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let payload: SchedulePayload = serde_json::from_value(payload)
                .map_err(|e| anyhow::anyhow!("invalid scheduler payload: {e}"))?;
            let writer = caps.session_write.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler routine fire without session_write cap — \
                     dispatcher must build a session-scoped bundle"
                )
            })?;
            let now = Utc::now();
            let content = format!(
                "Scheduled task '{}' triggered at {}.\n\n{}",
                payload.schedule_name,
                now.format("%Y-%m-%d %H:%M:%S UTC"),
                payload.task,
            );
            writer
                .append(SessionEntryDraft {
                    kind: "directive".into(),
                    data: serde_json::Value::String(content),
                })
                .await?;
            Ok(())
        })
    }
}
