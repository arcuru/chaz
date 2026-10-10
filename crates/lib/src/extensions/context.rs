//! The default context policy. It uses only public scoped selection facilities;
//! replacing it does not require changing the builder or the runtime.
use crate::{
    cache::{CacheAnchor, CacheOptions, CacheTtl},
    extension::{
        Extension, ExtensionInstance, ExtensionRef, HookKind, Scope, ScopeCtx,
        caps::{CapFuture, CapabilityKind},
        context_strategy::{ContextItem, ContextPlan, ContextStrategy, ContextStrategyCall},
        manifest::ExtensionManifest,
    },
    session::EntryType,
};
use std::sync::Arc;

pub struct BaselineContext;
impl Extension for BaselineContext {
    fn name(&self) -> &'static str {
        "baseline_context"
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
    fn scopes(&self) -> &[Scope] {
        &[Scope::Global]
    }
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().into(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![],
            required_capabilities: vec![],
            requested_capabilities: vec![],
            provides_capabilities: vec![CapabilityKind::ContextStrategy],
        }
    }
    fn instantiate<'a>(
        &'a self,
        _: ScopeCtx<'a>,
    ) -> crate::extension::instance::InstantiateFuture<'a> {
        Box::pin(async {
            Ok(Arc::new(BaselineInstance {
                manifest: BaselineContext.manifest(),
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}
struct BaselineInstance {
    manifest: ExtensionManifest,
}
impl ExtensionInstance for BaselineInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
    fn context_strategy(&self) -> Option<Arc<dyn ContextStrategy>> {
        Some(Arc::new(BaselineContext))
    }
}
impl ContextStrategy for BaselineContext {
    fn select<'a>(&'a self, call: &'a ContextStrategyCall<'a>) -> CapFuture<'a, ContextPlan> {
        Box::pin(async move {
            let ctx = call.context;
            let mut plan = ContextPlan::default();
            // Preserve the migration baseline, including the mandatory newest
            // row and conversation-first / bounded native replay preference.
            let mut fixed = Vec::new();
            if ctx.has_instructions() {
                fixed.push(ContextItem::Instructions);
            }
            if ctx.has_tail() {
                fixed.push(ContextItem::Tail);
            }
            // Keep the predecessor's selected-history allocation unchanged.
            // Durable text is appended below; the complete-request gate counts
            // it and refuses overflow. Reserving extra history space here would
            // change which original rows this migration selects.
            let used = fixed
                .iter()
                .try_fold(0, |sum, item| ctx.cost(item).map(|cost| sum + cost))?
                + ctx.tools_cost();
            let remaining = call.budget_tokens.saturating_sub(used);
            let all: Vec<_> = ctx.entries().collect();
            let start = all
                .iter()
                .rposition(|(_, kind)| *kind == EntryType::Summary)
                .unwrap_or(0);
            let candidates = &all[start..];
            let mut selected = Vec::new();
            let mut tokens = 0;
            for (index, kind) in candidates.iter().rev() {
                let cost = ctx.cost(&ContextItem::Entry(*index))?;
                if tokens + cost > remaining && !selected.is_empty() {
                    break;
                }
                tokens += cost;
                selected.push(*index);
                if *kind == EntryType::Summary {
                    break;
                }
            }
            selected.reverse();
            plan.truncated = selected.len() < candidates.len();
            let mut spare = remaining.saturating_sub(tokens);
            let mut replay = std::collections::HashMap::new();
            for index in selected.iter().rev() {
                let item = ContextItem::Exchange {
                    entry: *index,
                    budget: spare,
                };
                let cost = ctx.cost(&item)?;
                if cost > 0 {
                    spare = spare.saturating_sub(cost);
                    replay.insert(*index, item);
                }
            }
            if ctx.has_instructions() {
                plan.items.push(ContextItem::Instructions);
            }
            for index in selected {
                plan.items.push(ContextItem::Entry(index));
                if let Some(item) = replay.remove(&index) {
                    plan.items.push(item);
                }
            }
            if ctx.has_tail() {
                plan.items.push(ContextItem::Tail);
            }
            plan.items.extend(ctx.durable_items());
            // Deliberate settings are scoped Eidetica JSON, not a new permission
            // or automatic cache optimization. Default anchors/TTL stay unchanged.
            plan.cache = match call.settings.get("cache") {
                Some(value) => serde_json::from_value(value.clone())?,
                None => default_cache_options(),
            };
            Ok(plan)
        })
    }
}

/// Compatibility policy for callers outside the context-strategy runtime (e.g.
/// explicit /compact). The strategy owns this default, not the adapters.
pub fn default_cache_options() -> CacheOptions {
    CacheOptions {
        anchors: vec![
            CacheAnchor::LastTool,
            CacheAnchor::System,
            CacheAnchor::LatestUser,
        ],
        ttl: CacheTtl::FiveMinutes,
    }
}
