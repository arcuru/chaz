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

# Optional. Set a role, A.K.A. system prompt, to use by default
#role: ""

# Optional. Set a per-account message limit.
#message_limit: 0

# Optional. Set a room size limit to respond in.
#room_size_limit: 0

# Predefined roles here to use above
# These roles are builtin and can be set by any user
roles:
  - name: chaz
    description: Chaz is Chaz
    prompt: "Your name is Chaz, you are an AI assistant, and you refer to yourself in the third person."
    example: # Include some example responses, which can help the model understand the role
      - user: User
        message: "Are you ready?"
      - user: Assistant
        message: "Chaz is ready."
  - name: chazmina
    description: Chaz is Chazmina
    prompt: "Your name is Chazmina, you are an AI assistant, and you refer to yourself in the third person."
    example:
      - user: User
        message: "Are you ready?"
      - user: Assistant
        message: "Chazmina is ready."
  - name: cave-chaz
    description: Chaz is Cave Man Chaz
    prompt: "Your name is Chaz, you are an AI assistant, you talk like a cave man, and you refer to yourself in the third person."
    example:
      - user: User
        message: "Are you ready?"
      - user: Assistant
        message: "Chaz is ready."
  - name: cave-chazmina
    description: Chaz is Cave Man Chazmina
    prompt: "Your name is Chazmina, you are an AI assistant, you talk like a cave man, and you refer to yourself in the third person."
    example:
      - user: User
        message: "Are you ready?"
      - user: Assistant
        message: "Chazmina is ready."
  - name: bash
    description: Get a bash shell command
    prompt: >
      Based on the following user description, generate a corresponding Bash shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Bash command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Bash command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
  - name: fish
    description: Get a fish shell command
    prompt: >
      Based on the following user description, generate a corresponding Fish shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Fish command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Fish command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
  - name: zsh
    description: Get a zsh shell command
    prompt: >
      Based on the following user description, generate a corresponding Zsh shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Zsh command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Zsh command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
  - name: nu
    description: Get a nushell command
    prompt: >
      Based on the following user description, generate a corresponding Nushell shell command.
      Focus solely on interpreting the requirements and translating them into a single, executable Nushell command.
      Ensure accuracy and relevance to the user's description.
      The output should be a valid Nushell command that directly aligns with the user's intent, ready for execution in a command-line environment.
      Do not output anything except for the command.
      No code block, no English explanation, no newlines, and no start/end tags.
"#).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::get_role_names;

    /// Force the lazy_static to parse. If the embedded YAML is malformed,
    /// `unwrap()` inside DEFAULT_CONFIG's initializer would panic here.
    /// This is the regression guard against someone adding a role/agent to
    /// the default config with invalid YAML.
    #[test]
    fn default_config_parses() {
        let _ = DEFAULT_CONFIG.homeserver_url.as_str();
    }

    #[test]
    fn default_config_has_expected_required_roles() {
        let names = get_role_names(DEFAULT_CONFIG.roles.clone());
        // These five roles are documented in the user guide as the built-ins.
        for expected in ["chaz", "chazmina", "bash", "fish", "zsh"] {
            assert!(
                names.iter().any(|n| n == expected),
                "default role '{expected}' missing; have {names:?}"
            );
        }
    }

    #[test]
    fn default_config_has_no_declared_agents() {
        // Default config is expected to not declare any agents — users add
        // theirs via yaml. If this changes, update the assertion rather than
        // silently grow the default agent set.
        assert!(DEFAULT_CONFIG.agents.is_none());
    }

    #[test]
    fn default_config_has_no_backends() {
        // Users are expected to configure their own LLM backend. If we ever
        // ship a demo backend, change this test to document what it is.
        assert!(DEFAULT_CONFIG.backends.is_none());
    }

    #[test]
    fn default_roles_all_have_prompts() {
        // Every role in defaults should have a non-empty prompt — otherwise
        // the role is useless and it's a config bug.
        let roles = DEFAULT_CONFIG.roles.as_ref().unwrap();
        for role in roles {
            let prompt = role.get_prompt();
            assert!(
                !prompt.trim().is_empty(),
                "role '{}' has empty prompt",
                role.name
            );
        }
    }
}
