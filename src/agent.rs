use crate::config::Config;
use crate::defaults::DEFAULT_CONFIG;
use crate::role::{RoleDetails, get_role};

/// Agent configuration — defines personality, defaults, and available tools.
///
/// Currently wraps the role/prompt system. Will expand to include tool definitions,
/// behavioral config, and model preferences in later phases.
pub struct Agent {
    /// Will be used for agent identification in Phase 1.3+
    #[allow(dead_code)]
    pub name: String,
    pub default_role: Option<RoleDetails>,
    pub default_model: Option<String>,
}

impl Agent {
    /// Create an Agent from the global config
    pub fn from_config(config: &Config) -> Self {
        let default_role = get_role(
            config.role.clone(),
            config.roles.clone(),
            DEFAULT_CONFIG.roles.clone(),
        );
        Agent {
            name: "chaz".to_string(),
            default_role,
            default_model: None,
        }
    }
}
