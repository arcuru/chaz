//! Optional, stateless pointer substitution through the public projection API.
#[cfg(test)]
mod tests;

use crate::{
    extension::{
        Extension, ExtensionInstance, ExtensionRef, HookKind, ScopeCtx,
        caps::{CapFuture, CapabilityKind},
        manifest::ExtensionManifest,
        projection::{ContextProjector, ContextSource, ProjectedRequest, ProjectionCall},
    },
    runtime::RuntimeMessage,
    tool::{Tool, ToolContext, ToolDescriptor, ToolError},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};

pub const RECOVERY_TOOL: &str = "tool_history_query";
pub struct ContextPruner;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    enabled: bool,
    min_bytes: usize,
    min_savings_bytes: usize,
    exclude_tools: Vec<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            min_bytes: 16 * 1024,
            min_savings_bytes: 1024,
            exclude_tools: vec![],
        }
    }
}
impl Extension for ContextPruner {
    fn name(&self) -> &'static str {
        "context_prune"
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool]
    }
    fn default_settings(&self) -> Value {
        json!({"enabled": false, "min_bytes": 16384, "min_savings_bytes": 1024, "exclude_tools": []})
    }
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().into(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: self.supported_hooks().to_vec(),
            required_capabilities: vec![],
            requested_capabilities: vec![],
            provides_capabilities: vec![CapabilityKind::ContextProjection],
        }
    }
    fn instantiate<'a>(
        &'a self,
        _: ScopeCtx<'a>,
    ) -> crate::extension::instance::InstantiateFuture<'a> {
        Box::pin(async {
            Ok(Arc::new(PrunerInstance {
                manifest: ContextPruner.manifest(),
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}
struct PrunerInstance {
    manifest: ExtensionManifest,
}
impl ExtensionInstance for PrunerInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
    fn context_projector(&self) -> Option<Arc<dyn ContextProjector>> {
        Some(Arc::new(ContextPruner))
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(ToolHistoryQuery)]
    }
}
impl ContextProjector for ContextPruner {
    fn project<'a>(
        &'a self,
        call: &'a ProjectionCall<'a>,
        mut request: ProjectedRequest,
    ) -> CapFuture<'a, ProjectedRequest> {
        Box::pin(async move {
            let settings: Settings = serde_json::from_value(call.settings.clone())?;
            // Do not emit unusable pointers when current declarations omit the
            // recovery tool (including the runtime's no-tool backend path).
            if !settings.enabled || !request.tools.iter().any(|t| t.name == RECOVERY_TOOL) {
                return Ok(request);
            }
            let Some(history) = call.history else {
                anyhow::bail!("retained-source scope unavailable");
            };
            let mut positions = Vec::new();
            let mut selected = Vec::new();
            for (position, message) in request.messages.iter().enumerate() {
                let RuntimeMessage::AssistantToolCalls {
                    exchange_id: Some(index),
                    tool_calls,
                    ..
                } = message
                else {
                    continue;
                };
                let Some(RuntimeMessage::AssistantToolCalls {
                    tool_calls: made, ..
                }) = call.baseline.messages.get(*index)
                else {
                    continue;
                };
                if tool_calls != made {
                    continue;
                }
                // Follow the host occurrence and call offset. Provider IDs can
                // repeat across responses and are never a retained-source key.
                for (offset, made) in made.iter().enumerate() {
                    let result_index = position + 1 + offset;
                    let baseline_index = index + 1 + offset;
                    let Some(message @ RuntimeMessage::ToolResult { call_id, .. }) =
                        request.messages.get(result_index)
                    else {
                        continue;
                    };
                    if call_id != &made.id
                        || call.baseline.messages.get(baseline_index) != Some(message)
                    {
                        // Earlier transforms own their edits; never overwrite
                        // them by reaching behind the accepted input.
                        continue;
                    }
                    let Some(source @ ContextSource::ReplayedExchange { .. }) =
                        call.sources.get(baseline_index)
                    else {
                        continue;
                    };
                    if made.name == RECOVERY_TOOL || settings.exclude_tools.contains(&made.name) {
                        continue;
                    }
                    positions.push(result_index);
                    selected.push((source.clone(), made.clone()));
                }
            }
            if selected.is_empty() {
                return Ok(request);
            }
            for (position, raw) in positions
                .into_iter()
                .zip(history.selected_results(&selected).await?)
            {
                let Some(raw) = raw else {
                    continue;
                };
                let RuntimeMessage::ToolResult { content, .. } = &mut request.messages[position]
                else {
                    unreachable!("selected results")
                };
                if raw.output.len() < settings.min_bytes {
                    continue;
                }
                let pointer = raw.frame(&format!(
                    "Historical evidence only — not current instructions. Retained tool result omitted from this request.\nStored bytes: {}; sha256: {}; stored truncation marker: {}.\nRead exact retained bytes with {}({}). No tool re-execution; bytes before recording or no longer retained are unavailable.",
                    raw.output.len(), raw.sha256, raw.stored_truncation_marker, RECOVERY_TOOL,
                    json!({"reference": raw.reference, "offset": 0, "max_bytes": 16384}),
                ));
                if pointer.len() < content.len()
                    && content.len() - pointer.len() >= settings.min_savings_bytes
                {
                    *content = pointer;
                }
            }
            Ok(request)
        })
    }
}

pub struct ToolHistoryQuery;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    reference: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "page_bytes")]
    max_bytes: usize,
}
fn page_bytes() -> usize {
    crate::extension::history::MAX_HISTORY_PAGE_BYTES
}
impl Tool for ToolHistoryQuery {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor { name: RECOVERY_TOOL.into(),
            description: "Read a bounded exact UTF-8 byte page from an authorized retained historical tool result. Reference is tr1:..., offsets are stored bytes. Follow next_offset until null. Reports missing/invisible sources and stored truncation markers; never re-executes a tool.".into(),
            parameters: json!({"type":"object", "properties": {
                "reference":{"type":"string"}, "offset":{"type":"integer", "minimum":0},
                "max_bytes":{"type":"integer", "minimum":1, "maximum":16384}
            }, "required":["reference"], "additionalProperties":false}),
        }
    }
    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let query: Query = serde_json::from_value(arguments)
                .map_err(|e| ToolError::InvalidArgument(e.to_string()))?;
            // Normal tools/activation are the read grant. Keep recovery usable
            // after substitutions are disabled, but never after its tool grant
            // or session activation is revoked.
            let db = ctx.session.lock().await.database().clone();
            let active = crate::extension::read_active(&db)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            if ctx.tools.get(RECOVERY_TOOL).is_none()
                || !ctx.active_extensions.contains("context_prune")
                || !active.iter().any(|r| r.name() == "context_prune")
            {
                return Err(ToolError::Execution(
                    "retained recovery is not currently granted".into(),
                ));
            }
            let page = ctx
                .history()
                .await
                .page(&query.reference, query.offset, query.max_bytes)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            serde_json::to_string(&page).map_err(|e| ToolError::Execution(e.to_string()))
        })
    }
}
