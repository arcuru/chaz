use crate::config::{AgentConfig, AgentPreset, Config};
use crate::defaults::DEFAULT_CONFIG;
use crate::role::{get_role, RoleDetails};
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
#[derive(Clone)]
pub struct AgentRegistry {
    agents: Vec<Agent>,
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
            }]
        };

        let registry = Self { agents };
        registry.validate_references();
        registry
    }

    /// Get the default agent (first in the list)
    pub fn default_agent(&self) -> &Agent {
        &self.agents[0]
    }

    /// Look up an agent by name
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// List all agent names
    pub fn names(&self) -> Vec<&str> {
        self.agents.iter().map(|a| a.name.as_str()).collect()
    }

    /// Check if a caller agent is allowed to spawn a target agent.
    /// Both sides must agree: caller's can_spawn includes target,
    /// AND target's allowed_callers includes caller (or is empty).
    pub fn can_spawn(&self, caller_name: &str, target_name: &str) -> bool {
        let caller = match self.get(caller_name) {
            Some(a) => a,
            None => return false,
        };
        let target = match self.get(target_name) {
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

    /// Validate that all names in can_spawn and allowed_callers reference existing agents.
    fn validate_references(&self) {
        let names: Vec<&str> = self.agents.iter().map(|a| a.name.as_str()).collect();
        for agent in &self.agents {
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
        }
    }

    #[test]
    fn test_spawn_permission_both_sides() {
        let registry = AgentRegistry {
            agents: vec![
                make_agent("chaz", vec!["researcher"], vec![]),
                make_agent("researcher", vec![], vec!["chaz"]),
            ],
        };
        assert!(registry.can_spawn("chaz", "researcher"));
        assert!(!registry.can_spawn("researcher", "chaz"));
    }

    #[test]
    fn test_spawn_permission_open_callers() {
        let registry = AgentRegistry {
            agents: vec![
                make_agent("chaz", vec!["coder"], vec![]),
                make_agent("coder", vec![], vec![]), // empty = anyone
            ],
        };
        assert!(registry.can_spawn("chaz", "coder"));
    }

    #[test]
    fn test_spawn_permission_denied_by_callers() {
        let registry = AgentRegistry {
            agents: vec![
                make_agent("chaz", vec!["mayor"], vec![]),
                make_agent("mayor", vec![], vec!["researcher"]), // only researcher can call
            ],
        };
        assert!(!registry.can_spawn("chaz", "mayor"));
    }

    #[test]
    fn test_spawn_unknown_target() {
        let registry = AgentRegistry {
            agents: vec![make_agent("chaz", vec!["nonexistent"], vec![])],
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
}
