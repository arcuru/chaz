use crate::config::{AgentConfig, AgentPreset, Config};
use crate::defaults::DEFAULT_CONFIG;
use crate::grants::Grants;
use crate::role::{RoleDetails, get_role};
use std::collections::HashMap;
use tracing::warn;

/// Agent definition — personality, model preferences, tool visibility, and spawn permissions.
#[derive(Clone)]
pub struct Agent {
    pub name: String,
    pub default_role: Option<RoleDetails>,
    pub default_model: Option<String>,
    /// Tool names this agent can use. None = all tools (no filtering).
    pub allowed_tools: Option<Vec<String>>,
    /// Which agent definitions this agent can spawn.
    pub can_spawn: Vec<String>,
    /// Which agents are allowed to spawn this one. Empty = any with can_spawn permission.
    pub allowed_callers: Vec<String>,
    /// Maximum ReAct loop iterations.
    pub max_iterations: u32,
    /// Whether this agent can run without user input.
    pub autonomous: bool,
    /// Named override bundles for spawn-time configuration.
    pub presets: HashMap<String, AgentPreset>,
    /// Tool profile name (references a key in top-level tool_profiles config).
    pub tool_profile: Option<String>,
    /// Override context token limit for this agent (None = use global default).
    pub max_context_tokens: Option<usize>,
    /// Per-tool grant overrides. Merged per-kind over the config-level grants
    /// at tool-call time (see `Grants::merge_over`).
    pub grants: HashMap<String, Grants>,
}

/// Resolved overrides for a spawn_agent call.
/// All fields are final values after applying: definition defaults → preset → inline overrides.
pub struct ResolvedOverrides {
    pub model: Option<String>,
    pub max_iterations: u32,
    pub allowed_tools: Option<Vec<String>>,
    pub role_suffix: Option<String>,
    pub tool_profile: Option<String>,
}

impl Agent {
    fn from_agent_config(agent_config: &AgentConfig, config: &Config) -> Self {
        let default_role = get_role(
            agent_config.role.clone(),
            config.roles.clone(),
            DEFAULT_CONFIG.roles.clone(),
        );
        Agent {
            name: agent_config.name.clone(),
            default_role,
            default_model: agent_config.model.clone(),
            allowed_tools: agent_config.tools.clone(),
            can_spawn: agent_config.can_spawn.clone().unwrap_or_default(),
            allowed_callers: agent_config.allowed_callers.clone().unwrap_or_default(),
            max_iterations: agent_config.max_iterations.unwrap_or(10),
            autonomous: agent_config.autonomous,
            presets: agent_config.presets.clone().unwrap_or_default(),
            tool_profile: agent_config.tool_profile.clone(),
            max_context_tokens: agent_config.max_context_tokens,
            grants: agent_config.grants.clone().unwrap_or_default(),
        }
    }

    /// Build a runtime Agent from a Living Agent's DB config (Stage 6
    /// `/agent new` and `/agent import`). Role resolution falls back to the
    /// in-built defaults (`DEFAULT_CONFIG.roles`) so the agent participates
    /// in ReAct even without a named role.
    pub fn from_db_config(name: &str, cfg: &crate::agent_db::AgentDbConfig) -> Self {
        let default_role = get_role(cfg.role.clone(), None, DEFAULT_CONFIG.roles.clone());
        Agent {
            name: name.to_string(),
            default_role,
            default_model: cfg.model.clone(),
            allowed_tools: cfg.tools.clone(),
            can_spawn: cfg.can_spawn.clone(),
            allowed_callers: cfg.allowed_callers.clone(),
            max_iterations: cfg.max_iterations.unwrap_or(10),
            autonomous: cfg.autonomous,
            presets: cfg.presets.clone(),
            tool_profile: cfg.tool_profile.clone(),
            max_context_tokens: cfg.max_context_tokens,
            grants: cfg.grants.clone(),
        }
    }

    /// Resolve overrides from a preset name and/or inline overrides.
    /// Resolution order: agent defaults → preset → inline overrides.
    pub fn resolve_overrides(
        &self,
        preset: Option<&str>,
        model_override: Option<&str>,
        max_iterations_override: Option<u32>,
        tools_override: Option<&[String]>,
    ) -> ResolvedOverrides {
        let mut model = self.default_model.clone();
        let mut max_iterations = self.max_iterations;
        let mut allowed_tools = self.allowed_tools.clone();
        let mut role_suffix = None;
        let mut tool_profile = self.tool_profile.clone();

        // Apply preset if specified
        if let Some(preset_name) = preset {
            if let Some(p) = self.presets.get(preset_name) {
                if let Some(ref m) = p.model {
                    model = Some(m.clone());
                }
                if let Some(mi) = p.max_iterations {
                    max_iterations = mi;
                }
                if let Some(ref t) = p.tools {
                    // Preset tools must be subset of definition's allowed tools
                    allowed_tools = Some(intersect_tools(&allowed_tools, t));
                }
                role_suffix = p.role_suffix.clone();
                if let Some(ref tp) = p.tool_profile {
                    tool_profile = Some(tp.clone());
                }
            }
        }

        // Apply inline overrides
        if let Some(m) = model_override {
            model = Some(m.to_string());
        }
        if let Some(mi) = max_iterations_override {
            max_iterations = mi;
        }
        if let Some(t) = tools_override {
            // Inline tools must be subset of current allowed tools
            allowed_tools = Some(intersect_tools(&allowed_tools, t));
        }

        ResolvedOverrides {
            model,
            max_iterations,
            allowed_tools,
            role_suffix,
            tool_profile,
        }
    }
}

/// Intersect a tool override list with an existing allowlist.
/// If base is None (all tools), the override becomes the allowlist.
/// If both are set, only tools in both lists are kept.
fn intersect_tools(base: &Option<Vec<String>>, override_tools: &[String]) -> Vec<String> {
    match base {
        None => override_tools.to_vec(),
        Some(base_tools) => override_tools
            .iter()
            .filter(|t| base_tools.contains(t))
            .cloned()
            .collect(),
    }
}

/// Registry of named agents. Always has a default agent.
///
/// Backed by a `RwLock` so agents can be registered at runtime (via
/// `/agent new`); yaml-bootstrapped agents land here at startup, and
/// subsequently-created Living Agents join via [`AgentRegistry::register`].
/// All read accessors clone out `Agent` values — Agent is cheap to clone.
///
/// `config_roles` stashes the user's `roles:` list so live hydration from
/// `AgentDb::config` can resolve role names defined outside
/// `DEFAULT_CONFIG.roles` (Stage 8).
pub struct AgentRegistry {
    agents: std::sync::RwLock<Vec<Agent>>,
    config_roles: Option<Vec<RoleDetails>>,
}

impl AgentRegistry {
    /// Build the registry from config. If no agents defined, creates a default "chaz" agent.
    pub fn from_config(config: &Config) -> Self {
        let agents = if let Some(agent_configs) = &config.agents {
            agent_configs
                .iter()
                .map(|ac| Agent::from_agent_config(ac, config))
                .collect()
        } else {
            // Legacy: no agents section — create default from top-level role field
            let default_role = get_role(
                config.role.clone(),
                config.roles.clone(),
                DEFAULT_CONFIG.roles.clone(),
            );
            vec![Agent {
                name: "chaz".to_string(),
                default_role,
                default_model: None,
                allowed_tools: None,
                can_spawn: Vec::new(),
                allowed_callers: Vec::new(),
                max_iterations: 10,
                autonomous: false,
                presets: HashMap::new(),
                tool_profile: None,
                max_context_tokens: None,
                grants: HashMap::new(),
            }]
        };

        let registry = Self {
            agents: std::sync::RwLock::new(agents),
            config_roles: config.roles.clone(),
        };
        registry.validate_references();
        registry
    }

    /// Access the user-config roles stashed at registry-build time. Used by
    /// live hydration from AgentDb::config so role names reference the same
    /// roles that yaml-bootstrapped agents resolve against.
    pub fn config_roles(&self) -> Option<&Vec<RoleDetails>> {
        self.config_roles.as_ref()
    }

    /// Get the default agent (first in the list). Panics if the registry is
    /// empty — callers rely on `from_config` always seeding at least one.
    pub fn default_agent(&self) -> Agent {
        let agents = self.agents.read().unwrap();
        agents
            .first()
            .cloned()
            .expect("AgentRegistry always has at least one agent")
    }

    /// Look up an agent by name. Returns a cloned `Agent` to keep callers
    /// lock-free after the read.
    pub fn get(&self, name: &str) -> Option<Agent> {
        let agents = self.agents.read().unwrap();
        agents.iter().find(|a| a.name == name).cloned()
    }

    /// List all agent names (cloned).
    pub fn names(&self) -> Vec<String> {
        let agents = self.agents.read().unwrap();
        agents.iter().map(|a| a.name.clone()).collect()
    }

    /// Check if a caller agent is allowed to spawn a target agent.
    /// Both sides must agree: caller's can_spawn includes target,
    /// AND target's allowed_callers includes caller (or is empty).
    pub fn can_spawn(&self, caller_name: &str, target_name: &str) -> bool {
        let agents = self.agents.read().unwrap();
        let caller = match agents.iter().find(|a| a.name == caller_name) {
            Some(a) => a,
            None => return false,
        };
        let target = match agents.iter().find(|a| a.name == target_name) {
            Some(a) => a,
            None => return false,
        };

        // Caller must list target in can_spawn
        if !caller.can_spawn.contains(&target_name.to_string()) {
            return false;
        }

        // Target must list caller in allowed_callers (or have empty list = any)
        if target.allowed_callers.is_empty() {
            return true;
        }
        target.allowed_callers.contains(&caller_name.to_string())
    }

    /// Register a new agent at runtime. Rejects duplicates by display name.
    /// Used by `/agent new` and `/agent import` so Living Agents created
    /// after startup are routable without a config-reload.
    pub fn register(&self, agent: Agent) -> anyhow::Result<()> {
        let mut agents = self.agents.write().unwrap();
        if agents.iter().any(|a| a.name == agent.name) {
            anyhow::bail!("Agent '{}' already registered", agent.name);
        }
        agents.push(agent);
        Ok(())
    }

    /// Replace the runtime entry for `name` — or insert if absent. Used by
    /// Stage 8 live hydration so edits to `AgentDb::config` propagate into
    /// the in-memory registry at resolution time.
    pub fn upsert(&self, agent: Agent) {
        let mut agents = self.agents.write().unwrap();
        if let Some(existing) = agents.iter_mut().find(|a| a.name == agent.name) {
            *existing = agent;
        } else {
            agents.push(agent);
        }
    }

    /// Remove the runtime entry for `name`. Returns `true` if an entry was
    /// removed, `false` if no agent by that name existed. Used by `/agent
    /// delete`; the DB-side record survives.
    pub fn unregister(&self, name: &str) -> bool {
        let mut agents = self.agents.write().unwrap();
        let before = agents.len();
        agents.retain(|a| a.name != name);
        agents.len() != before
    }

    /// Build a runtime `Agent` from a Living Agent's DB config, resolving the
    /// role name against this registry's `config_roles` (falling back to
    /// `DEFAULT_CONFIG.roles`). Use this instead of `Agent::from_db_config`
    /// when the user's yaml-defined roles need to be honored.
    pub fn build_from_db_config(&self, name: &str, cfg: &crate::agent_db::AgentDbConfig) -> Agent {
        let default_role = get_role(
            cfg.role.clone(),
            self.config_roles.clone(),
            DEFAULT_CONFIG.roles.clone(),
        );
        Agent {
            name: name.to_string(),
            default_role,
            default_model: cfg.model.clone(),
            allowed_tools: cfg.tools.clone(),
            can_spawn: cfg.can_spawn.clone(),
            allowed_callers: cfg.allowed_callers.clone(),
            max_iterations: cfg.max_iterations.unwrap_or(10),
            autonomous: cfg.autonomous,
            presets: cfg.presets.clone(),
            tool_profile: cfg.tool_profile.clone(),
            max_context_tokens: cfg.max_context_tokens,
            grants: cfg.grants.clone(),
        }
    }

    /// Validate that all names in can_spawn and allowed_callers reference existing agents.
    fn validate_references(&self) {
        let agents = self.agents.read().unwrap();
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        for agent in agents.iter() {
            for target in &agent.can_spawn {
                if !names.contains(&target.as_str()) {
                    warn!(
                        "Agent '{}' references unknown agent '{}' in can_spawn",
                        agent.name, target
                    );
                }
            }
            for caller in &agent.allowed_callers {
                if !names.contains(&caller.as_str()) {
                    warn!(
                        "Agent '{}' references unknown agent '{}' in allowed_callers",
                        agent.name, caller
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent(name: &str, can_spawn: Vec<&str>, allowed_callers: Vec<&str>) -> Agent {
        Agent {
            name: name.to_string(),
            default_role: None,
            default_model: None,
            allowed_tools: None,
            can_spawn: can_spawn.into_iter().map(String::from).collect(),
            allowed_callers: allowed_callers.into_iter().map(String::from).collect(),
            max_iterations: 10,
            autonomous: false,
            presets: HashMap::new(),
            tool_profile: None,
            max_context_tokens: None,
            grants: HashMap::new(),
        }
    }

    #[test]
    fn test_spawn_permission_both_sides() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![
                make_agent("chaz", vec!["researcher"], vec![]),
                make_agent("researcher", vec![], vec!["chaz"]),
            ]),
            config_roles: None,
        };
        assert!(registry.can_spawn("chaz", "researcher"));
        assert!(!registry.can_spawn("researcher", "chaz"));
    }

    #[test]
    fn test_spawn_permission_open_callers() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![
                make_agent("chaz", vec!["coder"], vec![]),
                make_agent("coder", vec![], vec![]), // empty = anyone
            ]),
            config_roles: None,
        };
        assert!(registry.can_spawn("chaz", "coder"));
    }

    #[test]
    fn test_spawn_permission_denied_by_callers() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![
                make_agent("chaz", vec!["mayor"], vec![]),
                make_agent("mayor", vec![], vec!["researcher"]), // only researcher can call
            ]),
            config_roles: None,
        };
        assert!(!registry.can_spawn("chaz", "mayor"));
    }

    #[test]
    fn test_spawn_unknown_target() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![make_agent("chaz", vec!["nonexistent"], vec![])]),
            config_roles: None,
        };
        assert!(!registry.can_spawn("chaz", "nonexistent"));
    }

    #[test]
    fn test_resolve_overrides_defaults() {
        let agent = make_agent("test", vec![], vec![]);
        let resolved = agent.resolve_overrides(None, None, None, None);
        assert_eq!(resolved.model, None);
        assert_eq!(resolved.max_iterations, 10);
        assert!(resolved.allowed_tools.is_none());
        assert!(resolved.role_suffix.is_none());
    }

    #[test]
    fn test_resolve_overrides_preset() {
        let mut agent = make_agent("test", vec![], vec![]);
        agent.default_model = Some("sonnet".to_string());
        agent.presets.insert(
            "deep".to_string(),
            AgentPreset {
                model: Some("opus".to_string()),
                max_iterations: Some(40),
                tools: None,
                role_suffix: Some("Be thorough.".to_string()),
                tool_profile: None,
            },
        );

        let resolved = agent.resolve_overrides(Some("deep"), None, None, None);
        assert_eq!(resolved.model.as_deref(), Some("opus"));
        assert_eq!(resolved.max_iterations, 40);
        assert_eq!(resolved.role_suffix.as_deref(), Some("Be thorough."));
    }

    #[test]
    fn test_resolve_overrides_inline_wins() {
        let mut agent = make_agent("test", vec![], vec![]);
        agent.presets.insert(
            "deep".to_string(),
            AgentPreset {
                model: Some("opus".to_string()),
                max_iterations: Some(40),
                tools: None,
                role_suffix: None,
                tool_profile: None,
            },
        );

        let resolved = agent.resolve_overrides(Some("deep"), Some("haiku"), Some(5), None);
        assert_eq!(resolved.model.as_deref(), Some("haiku"));
        assert_eq!(resolved.max_iterations, 5);
    }

    #[test]
    fn test_intersect_tools() {
        // None base = override is the list
        assert_eq!(
            intersect_tools(&None, &["a".into(), "b".into()]),
            vec!["a", "b"]
        );
        // Both set = intersection
        assert_eq!(
            intersect_tools(
                &Some(vec!["a".into(), "b".into(), "c".into()]),
                &["b".into(), "c".into(), "d".into()]
            ),
            vec!["b", "c"]
        );
    }

    // -----------------------------------------------------------------------
    // Stage 6: runtime registration + from_db_config
    // -----------------------------------------------------------------------

    #[test]
    fn registry_register_adds_and_rejects_duplicates() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![make_agent("chaz", vec![], vec![])]),
            config_roles: None,
        };
        let new_agent = make_agent("researcher", vec![], vec![]);
        registry.register(new_agent).unwrap();
        assert!(registry.get("researcher").is_some());

        // Duplicate rejected.
        let dup = make_agent("researcher", vec![], vec![]);
        assert!(registry.register(dup).is_err());

        // Names list reflects registration.
        let names = registry.names();
        assert!(names.contains(&"chaz".to_string()));
        assert!(names.contains(&"researcher".to_string()));
    }

    #[test]
    fn agent_from_db_config_maps_all_fields() {
        let cfg = crate::agent_db::AgentDbConfig {
            model: Some("opus".to_string()),
            tools: Some(vec!["get_time".into()]),
            can_spawn: vec!["alpha".into()],
            allowed_callers: vec!["beta".into()],
            max_iterations: Some(42),
            tool_profile: Some("deep".to_string()),
            max_context_tokens: Some(200_000),
            ..Default::default()
        };

        let agent = Agent::from_db_config("new-agent", &cfg);
        assert_eq!(agent.name, "new-agent");
        assert_eq!(agent.default_model.as_deref(), Some("opus"));
        assert_eq!(
            agent.allowed_tools.as_deref(),
            Some(&["get_time".to_string()][..])
        );
        assert_eq!(agent.can_spawn, vec!["alpha".to_string()]);
        assert_eq!(agent.allowed_callers, vec!["beta".to_string()]);
        assert_eq!(agent.max_iterations, 42);
        assert_eq!(agent.tool_profile.as_deref(), Some("deep"));
        assert_eq!(agent.max_context_tokens, Some(200_000));
    }

    #[test]
    fn agent_from_db_config_uses_defaults_for_empty_config() {
        let cfg = crate::agent_db::AgentDbConfig::default();
        let agent = Agent::from_db_config("fresh", &cfg);
        assert_eq!(agent.name, "fresh");
        assert_eq!(agent.max_iterations, 10);
        assert!(agent.allowed_tools.is_none());
        assert!(agent.can_spawn.is_empty());
    }

    // -----------------------------------------------------------------------
    // Stage 8: live hydration from AgentDb config
    // -----------------------------------------------------------------------

    #[test]
    fn build_from_db_config_resolves_role_via_registry_config_roles() {
        let custom_role = RoleDetails::new_test("custom", "you are custom");
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![]),
            config_roles: Some(vec![custom_role]),
        };
        let cfg = crate::agent_db::AgentDbConfig {
            role: Some("custom".to_string()),
            ..Default::default()
        };
        let agent = registry.build_from_db_config("alpha", &cfg);
        let resolved = agent.default_role.expect("role should resolve");
        assert_eq!(resolved.name, "custom");
        assert_eq!(resolved.get_prompt(), "you are custom");
    }

    #[test]
    fn upsert_replaces_existing_entry() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![make_agent("chaz", vec![], vec![])]),
            config_roles: None,
        };
        let mut updated = make_agent("chaz", vec!["researcher"], vec![]);
        updated.default_model = Some("opus".to_string());
        registry.upsert(updated);
        let got = registry.get("chaz").unwrap();
        assert_eq!(got.default_model.as_deref(), Some("opus"));
        assert_eq!(got.can_spawn, vec!["researcher".to_string()]);
        // Still one entry total (upsert didn't append a duplicate).
        assert_eq!(registry.names().len(), 1);
    }

    #[test]
    fn upsert_inserts_when_absent() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![make_agent("chaz", vec![], vec![])]),
            config_roles: None,
        };
        registry.upsert(make_agent("beta", vec![], vec![]));
        assert_eq!(registry.names().len(), 2);
        assert!(registry.get("beta").is_some());
    }

    #[test]
    fn build_from_db_config_picks_up_edits() {
        // Simulates: config writes V1 → runtime builds agent with V1 →
        // config writes V2 → runtime rebuilds → sees V2.
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![]),
            config_roles: None,
        };

        let v1 = crate::agent_db::AgentDbConfig {
            model: Some("haiku".to_string()),
            max_iterations: Some(5),
            ..Default::default()
        };
        let built_v1 = registry.build_from_db_config("alpha", &v1);
        registry.upsert(built_v1);
        assert_eq!(
            registry.get("alpha").unwrap().default_model.as_deref(),
            Some("haiku")
        );
        assert_eq!(registry.get("alpha").unwrap().max_iterations, 5);

        // Second build after a config edit picks up the new values.
        let v2 = crate::agent_db::AgentDbConfig {
            model: Some("opus".to_string()),
            max_iterations: Some(99),
            ..Default::default()
        };
        let built_v2 = registry.build_from_db_config("alpha", &v2);
        registry.upsert(built_v2);
        assert_eq!(
            registry.get("alpha").unwrap().default_model.as_deref(),
            Some("opus")
        );
        assert_eq!(registry.get("alpha").unwrap().max_iterations, 99);
    }
}
