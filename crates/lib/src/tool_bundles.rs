//! Session-local advertisement of already registered tools. Neither declarations
//! nor wants are consulted for lookup, execution, or delegation.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use chrono::{DateTime, Utc};
use eidetica::Database;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::extension::{Extension, ExtensionEvent, latest_events};
use crate::tool::{NO_REPLY_TOOL, ScopedTools, ToolContext, ToolDefinition, ToolProfile};

/// Local bundle ID and exact registered tool names. The host assigns the
/// `config/` or `ext/<extension>/` prefix.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolBundleDeclaration {
    pub id: String,
    pub tools: Vec<String>,
}

#[derive(Debug)]
struct Bundle {
    owner: Option<String>,
    tools: Vec<String>,
}

type Catalog = HashMap<String, Bundle>;

#[derive(Clone, Debug, Default)]
pub struct DeferredBundles {
    catalog: Arc<Catalog>,
    selected: HashSet<String>,
    members: HashSet<String>,
}

impl DeferredBundles {
    pub(crate) fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['/', '*', '?', '[', ']'])
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn valid_id(id: &str) -> bool {
    let parts: Vec<_> = id.split('/').collect();
    matches!(parts.as_slice(), ["config", name] if valid_segment(name))
        || matches!(parts.as_slice(), ["ext", owner, name] if valid_segment(owner) && valid_segment(name))
}

fn validate_members(id: &str, tools: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(!tools.is_empty(), "{id}: tools must not be empty");
    let mut members = HashSet::new();
    for tool in tools {
        anyhow::ensure!(
            !tool.is_empty()
                && tool.len() <= 64
                && tool != NO_REPLY_TOOL
                && tool
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
            "{id}: malformed or reserved tool name '{tool}'"
        );
        anyhow::ensure!(members.insert(tool), "{id}: duplicate tool '{tool}'");
    }
    Ok(())
}

/// Validate the entire candidate before installing extensions (which can start
/// background discovery). Missing registrations are deliberately not a validation error.
pub(crate) fn build_profiles(
    config: &Config,
    extensions: &[Arc<dyn Extension>],
) -> anyhow::Result<HashMap<String, ToolProfile>> {
    let mut catalog = Catalog::new();
    let mut declare = |owner: Option<&str>, declaration: &ToolBundleDeclaration| {
        let id = match owner {
            Some(owner) => format!("ext/{owner}/{}", declaration.id),
            None => format!("config/{}", declaration.id),
        };
        anyhow::ensure!(valid_id(&id), "tool_bundles: malformed bundle ID '{id}'");
        validate_members(&id, &declaration.tools)?;
        anyhow::ensure!(
            !catalog.contains_key(&id),
            "tool_bundles: duplicate bundle '{id}'"
        );
        catalog.insert(
            id,
            Bundle {
                owner: owner.map(str::to_owned),
                tools: declaration.tools.clone(),
            },
        );
        Ok(())
    };
    for declaration in &config.tool_bundles {
        declare(None, declaration)?;
    }
    for extension in extensions {
        let declarations = extension.tool_bundles();
        if !declarations.is_empty() {
            let manifest = extension.manifest();
            manifest.validate()?;
            anyhow::ensure!(
                manifest.name == extension.name(),
                "tool_bundles: manifest name '{}' disagrees with registered extension '{}'",
                manifest.name,
                extension.name()
            );
            anyhow::ensure!(
                matches!(
                    manifest.extension_ref,
                    crate::extension::ExtensionRef::Builtin { .. }
                ),
                "tool_bundles: '{}' is not a native extension",
                manifest.name
            );
            for declaration in &declarations {
                declare(Some(&manifest.name), declaration)?;
            }
        }
    }
    let catalog = Arc::new(catalog);
    config
        .tool_profiles
        .iter()
        .flatten()
        .map(|(name, cfg)| {
            let mut selected = HashSet::new();
            let mut members = HashSet::new();
            for id in &cfg.defer_bundles {
                let field = format!("tool_profiles.{name}.defer_bundles");
                anyhow::ensure!(valid_id(id), "{field}: malformed bundle ID '{id}'");
                let bundle = catalog
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("{field}: unknown bundle '{id}'"))?;
                anyhow::ensure!(
                    selected.insert(id.clone()),
                    "{field}: duplicate bundle '{id}'"
                );
                members.extend(bundle.tools.iter().cloned());
            }
            Ok((
                name.clone(),
                ToolProfile {
                    default_mode: cfg.default.clone().unwrap_or_default(),
                    tool_modes: cfg.tools.clone().unwrap_or_default(),
                    deferred_bundles: DeferredBundles {
                        catalog: catalog.clone(),
                        selected,
                        members,
                    },
                },
            ))
        })
        .collect()
}

/// The activation occurrence, not just membership, matters: a remove/add
/// between requests must not revive earlier wants or an old adapter handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Activation {
    session: DateTime<Utc>,
    agent: Option<DateTime<Utc>>,
}

type Active = HashMap<String, Activation>;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Consumer {
    extension: &'static str,
    skill: Option<String>,
}

#[derive(Debug)]
struct Slot {
    activation: Activation,
    // None means a config-owned bundle, with no defining extension to revoke.
    wants: HashMap<String, Option<Activation>>,
}

#[derive(Debug, Default)]
struct Resident {
    closed: bool,
    generation: u64,
    slots: HashMap<Consumer, Slot>,
}

impl Resident {
    fn prune(&mut self, active: &Active, bundles: &DeferredBundles) {
        self.slots.retain(|consumer, slot| {
            if active.get(consumer.extension) != Some(&slot.activation) {
                return false;
            }
            slot.wants.retain(|id, stamp| {
                bundles.catalog.get(id).is_some_and(|bundle| {
                    bundle.owner.as_ref().is_none_or(|owner| {
                        active
                            .get(owner)
                            .is_some_and(|current| Some(*current) == *stamp)
                    })
                })
            });
            !slot.wants.is_empty()
        });
    }
}

type AgentViews = HashMap<String, Arc<Mutex<Resident>>>;

/// Volatile views attached to an executing peer, keyed by session DB and
/// agent identity. No presentation data is stored in session history.
#[derive(Default)]
pub(crate) struct PresentationSessions {
    sessions: Mutex<HashMap<String, AgentViews>>,
}

impl PresentationSessions {
    pub(crate) async fn reset(&self, session: &str, close: bool) {
        let mut sessions = self.sessions.lock().await;
        if let Some(views) = sessions.get(session) {
            for view in views.values() {
                let mut resident = view.lock().await;
                resident.slots.clear();
                resident.generation += 1;
                resident.closed = close;
            }
        }
        if close {
            sessions.remove(session);
        }
    }

    pub(crate) async fn bind(
        &self,
        ctx: &ToolContext,
        session: Database,
        agent: Option<Database>,
        ceiling: HashSet<String>,
    ) -> PresentationTurn {
        let actor = ctx.actor_key();
        let resident = self
            .sessions
            .lock()
            .await
            .entry(session.root_id().to_string())
            .or_default()
            .entry(actor)
            .or_default()
            .clone();
        PresentationTurn {
            resident,
            authority: Arc::new(Authority {
                session,
                agent,
                ceiling,
            }),
            bundles: ctx.profile.deferred_bundles.clone(),
        }
    }
}

#[derive(Debug)]
struct Authority {
    session: Database,
    agent: Option<Database>,
    ceiling: HashSet<String>,
}

impl Authority {
    async fn active(&self) -> anyhow::Result<Active> {
        let session = latest_events(&self.session).await?;
        let agent = match &self.agent {
            Some(db) => latest_events(db).await?,
            None => HashMap::new(),
        };
        Ok(session
            .into_iter()
            .filter_map(|(name, event)| {
                let ExtensionEvent::Activated {
                    timestamp: session, ..
                } = event
                else {
                    return None;
                };
                let agent = match agent.get(&name) {
                    Some(ExtensionEvent::Deactivated { .. }) => return None,
                    Some(event) => Some(event.timestamp()),
                    None => None,
                };
                Some((name, Activation { session, agent }))
            })
            .collect())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PresentationTurn {
    resident: Arc<Mutex<Resident>>,
    authority: Arc<Authority>,
    bundles: DeferredBundles,
}

/// Bound to the actual call's registered native extension, session, and
/// acting agent. A consumer can only replace or clear its own slot.
#[derive(Clone, Debug)]
pub struct BundlePresentation {
    resident: Weak<Mutex<Resident>>,
    authority: Arc<Authority>,
    bundles: DeferredBundles,
    consumer: Consumer,
    activation: Activation,
    generation: u64,
}

impl BundlePresentation {
    /// Used only by the native skills adapter, with an identity obtained by
    /// resolving its trusted source (never an ID supplied by the model).
    pub fn for_resolved_skill(&self, source_identity: String) -> Option<Self> {
        (self.consumer.extension == "skills" && self.consumer.skill.is_none()).then(|| {
            let mut handle = self.clone();
            handle.consumer.skill = Some(source_identity);
            handle
        })
    }

    /// Atomically replace this slot's wants. An empty list releases them.
    /// Known bundles not selected in this profile don't affect presentation.
    pub async fn set_wanted(&self, ids: &[&str]) -> anyhow::Result<()> {
        let resident = self
            .resident
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("session presentation is closed"))?;
        let mut resident = resident.lock().await;
        anyhow::ensure!(
            !resident.closed && resident.generation == self.generation,
            "session presentation handle has expired"
        );
        let active = self.authority.active().await?;
        anyhow::ensure!(
            active.get(self.consumer.extension) == Some(&self.activation),
            "bundle consumer '{}' is no longer active",
            self.consumer.extension
        );
        let mut wants = HashMap::new();
        for &id in ids {
            let bundle = self
                .bundles
                .catalog
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("unknown tool bundle '{id}'"))?;
            if !self.bundles.selected.contains(id) {
                continue;
            }
            let stamp = bundle
                .owner
                .as_ref()
                .map(|owner| {
                    active
                        .get(owner)
                        .filter(|_| self.authority.ceiling.contains(owner))
                        .copied()
                        .ok_or_else(|| {
                            anyhow::anyhow!("tool bundle '{id}' has no active defining extension")
                        })
                })
                .transpose()?;
            wants.insert(id.to_string(), stamp);
        }
        if wants.is_empty() {
            resident.slots.remove(&self.consumer);
        } else {
            resident.slots.insert(
                self.consumer.clone(),
                Slot {
                    activation: self.activation,
                    wants,
                },
            );
        }
        Ok(())
    }
}

impl PresentationTurn {
    pub(crate) async fn handle(
        &self,
        extension: &'static str,
    ) -> anyhow::Result<Option<BundlePresentation>> {
        let resident = self.resident.lock().await;
        if resident.closed || !self.authority.ceiling.contains(extension) {
            return Ok(None);
        }
        let active = self.authority.active().await?;
        Ok(active.get(extension).map(|activation| BundlePresentation {
            resident: Arc::downgrade(&self.resident),
            authority: self.authority.clone(),
            bundles: self.bundles.clone(),
            consumer: Consumer {
                extension,
                skill: None,
            },
            activation: *activation,
            generation: resident.generation,
        }))
    }

    pub(crate) async fn snapshot(
        &self,
        ctx: &ToolContext,
        supports_tools: bool,
    ) -> anyhow::Result<ToolPresentation> {
        let mut resident = self.resident.lock().await;
        anyhow::ensure!(!resident.closed, "session presentation is closed");
        let active = self.authority.active().await?;
        resident.prune(&active, &self.bundles);
        let scoped = ctx.tools.clone().narrow_active_extensions(
            self.authority
                .ceiling
                .iter()
                .filter(|name| active.contains_key(*name))
                .cloned()
                .collect(),
        );
        let wanted: HashSet<&str> = resident
            .slots
            .iter()
            .filter(|(consumer, _)| self.authority.ceiling.contains(consumer.extension))
            .flat_map(|(_, slot)| slot.wants.keys())
            .filter(|id| self.bundles.selected.contains(*id))
            .map(|id| &self.bundles.catalog[id])
            .filter(|bundle| {
                bundle
                    .owner
                    .as_ref()
                    .is_none_or(|name| self.authority.ceiling.contains(name))
            })
            .flat_map(|bundle| bundle.tools.iter().map(String::as_str))
            .collect();
        Ok(ToolPresentation::compose(
            ctx,
            &scoped,
            &self.bundles.members,
            &wanted,
            supports_tools,
        ))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolPresentation {
    pub(crate) advertised: Vec<ToolDefinition>,
    pub(crate) eligible_names: Vec<String>,
}

impl ToolPresentation {
    fn compose(
        ctx: &ToolContext,
        tools: &ScopedTools,
        deferred: &HashSet<String>,
        wanted: &HashSet<&str>,
        supports_tools: bool,
    ) -> Self {
        let (mut defs, ambiguous) = tools.bundle_definitions(&ctx.profile, deferred);
        let mut eligible_names: Vec<_> = defs.iter().map(|def| def.name.clone()).collect();
        if ctx.allow_no_reply {
            eligible_names.push(NO_REPLY_TOOL.into());
        }
        if supports_tools {
            defs.retain(|def| {
                !deferred.contains(&def.name)
                    || (!ambiguous.contains(&def.name) && wanted.contains(def.name.as_str()))
            });
            ctx.append_no_reply(&mut defs);
        } else {
            defs.clear();
        }
        Self {
            advertised: defs,
            eligible_names,
        }
    }
}

/// The first round and initial context assembly share these exact definitions.
#[derive(Clone, Debug)]
pub struct PreparedToolPresentation {
    pub(crate) turn: PresentationTurn,
    pub(crate) initial: ToolPresentation,
}
