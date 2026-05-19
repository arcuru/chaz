use crate::config::Config;
use lazy_static::lazy_static;

// This is the default configuration for chaz
// It's defined as a variable because of annoyances with including it in the nix build

lazy_static! {
    pub static ref DEFAULT_CONFIG: Config =
        serde_yaml::from_str(r#"
# These are required to be set by the user's config.
homeserver_url: ""
username: ""

# Optional, if not given it will be asked for on first run
#password: ""

# Technically optional, but the bot won't respond without it
#allow_list: ""

# Optional. Not setting it here because reading it from an XDG library is safer.
#state_dir: "$XDG_STATE_HOME/username"

# Optional. This is a separate model to use for summarization
#chat_summary_model: ""

# Optional. Set a per-account message limit.
#message_limit: 0

# Optional. Set a room size limit to respond in.
#room_size_limit: 0

# Built-in agents. Each agent has a `system_prompt:` string.
# Users can reference these agent names from their own configs.
agents:
  - name: chaz
    system_prompt: "Your name is Chaz, you are an AI assistant, and you refer to yourself in the third person."
  - name: chazmina
    system_prompt: "Your name is Chazmina, you are an AI assistant, and you refer to yourself in the third person."
  - name: cave-chaz
    system_prompt: "Your name is Chaz, you are an AI assistant, you talk like a cave man, and you refer to yourself in the third person."
  - name: cave-chazmina
    system_prompt: "Your name is Chazmina, you are an AI assistant, you talk like a cave man, and you refer to yourself in the third person."
  - name: bash
    system_prompt: >
      Based on the following user description, generate a corresponding Bash shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Bash command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Bash command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
  - name: fish
    system_prompt: >
      Based on the following user description, generate a corresponding Fish shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Fish command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Fish command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
  - name: zsh
    system_prompt: >
      Based on the following user description, generate a corresponding Zsh shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Zsh command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Zsh command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
  - name: nu
    system_prompt: >
      Based on the following user description, generate a corresponding Nushell shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Nushell command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Nushell command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
"#).unwrap();
}

/// Look up a built-in agent by name in `DEFAULT_CONFIG.agents`. Returns
/// the first match — names are unique by construction.
#[allow(dead_code)]
pub fn default_agent(name: &str) -> Option<crate::config::AgentConfig> {
    DEFAULT_CONFIG
        .agents
        .as_ref()?
        .iter()
        .find(|a| a.name == name)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Force the lazy_static to parse. If the embedded YAML is malformed,
    /// `unwrap()` inside DEFAULT_CONFIG's initializer would panic here.
    /// This is the regression guard against someone adding an agent with
    /// invalid YAML.
    #[test]
    fn default_config_parses() {
        let _ = DEFAULT_CONFIG.homeserver_url.as_str();
    }

    #[test]
    fn default_config_has_expected_built_in_agents() {
        let agents = DEFAULT_CONFIG
            .agents
            .as_ref()
            .expect("DEFAULT_CONFIG must declare built-in agents");
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        // These are documented in the user guide as the built-ins. The
        // shell variants (bash/fish/zsh/nu) are stateless agents whose
        // persona is fully defined by their inline prompt.
        for expected in ["chaz", "chazmina", "bash", "fish", "zsh"] {
            assert!(
                names.contains(&expected),
                "default agent '{expected}' missing; have {names:?}"
            );
        }
    }

    #[test]
    fn default_config_no_legacy_roles_field() {
        // `roles:` and `role:` are removed from Config. Agents use
        // `system_prompt:` directly. No assertion needed; the struct
        // won't deserialize a `roles:` key.
    }

    #[test]
    fn default_agents_all_have_system_prompts() {
        let agents = DEFAULT_CONFIG.agents.as_ref().unwrap();
        for agent in agents {
            let prompt = agent.system_prompt.as_deref().unwrap_or("");
            assert!(
                !prompt.trim().is_empty(),
                "default agent '{}' has empty system_prompt",
                agent.name
            );
        }
    }

    #[test]
    fn default_config_has_no_backends() {
        // Users are expected to configure their own LLM backend. If we ever
        // ship a demo backend, change this test to document what it is.
        assert!(DEFAULT_CONFIG.backends.is_none());
    }

    #[test]
    fn default_agent_lookup_works() {
        assert!(default_agent("chaz").is_some());
        assert!(default_agent("nonexistent").is_none());
    }
}
