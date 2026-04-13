use crate::config::{AgentConfig, Config};
use crate::defaults::DEFAULT_CONFIG;
use crate::role::{RoleDetails, get_role};

/// Agent definition — personality, model preferences, and tool visibility.
#[derive(Clone)]
pub struct Agent {
    #[allow(dead_code)]
    pub name: String,
    pub default_role: Option<RoleDetails>,
    pub default_model: Option<String>,
    /// Tool names this agent can use. None = all tools (no filtering).
    pub allowed_tools: Option<Vec<String>>,
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
        }
    }
}

/// Registry of named agents. Always has a default agent.
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
            }]
        };

        Self { agents }
    }

    /// Get the default agent (first in the list)
    pub fn default_agent(&self) -> &Agent {
        &self.agents[0]
    }

    /// Look up an agent by name
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }
}
