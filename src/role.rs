use serde::Deserialize;
/// Roles
/// Roles are the same as defining the system prompt.
/// Some models, especially the chat models, take a specific system prompt, and others you can just inject it as the first message.
/// Prompting the models with an example message can also be useful.
use serde::de::{self, Deserializer, Unexpected, Visitor};
use std::fmt;

#[derive(Debug, Deserialize, Clone)]
pub struct RoleDetails {
    /// Name of the role, used to reference it
    pub name: String,
    /// Description of the role (deserialized from config, reserved for future use)
    #[allow(dead_code)]
    description: Option<String>,
    /// The system prompt for the model
    prompt: Option<String>,
    /// Example Conversations (deserialized from config, reserved for future use)
    #[allow(dead_code)]
    example: Option<Vec<Message>>,
}

impl RoleDetails {
    pub fn new(
        name: &str,
        description: Option<String>,
        prompt: Option<String>,
        example: Option<Vec<Message>>,
    ) -> Self {
        RoleDetails {
            name: name.to_owned(),
            description,
            prompt,
            example,
        }
    }

    /// Create a minimal RoleDetails for testing
    #[cfg(test)]
    pub fn new_test(name: &str, prompt: &str) -> Self {
        RoleDetails {
            name: name.to_owned(),
            description: None,
            prompt: Some(prompt.to_owned()),
            example: None,
        }
    }

    pub fn get_prompt(&self) -> String {
        if let Some(prompt) = &self.prompt {
            return prompt.clone();
        }
        "".to_string()
    }
}

/// A single message in a conversation (deserialized from config, fields used by serde)
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Message {
    user: MessageRole,
    message: String,
}

/// The role of a single message in a conversation
/// Can be parsed as either upper or lower case
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum MessageRole {
    User,
    Assistant,
}

impl fmt::Display for MessageRole {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MessageRole::User => write!(f, "USER"),
            MessageRole::Assistant => write!(f, "ASSISTANT"),
        }
    }
}

// Allow the MessageRole to be in any case
struct RoleVisitor;
impl Visitor<'_> for RoleVisitor {
    type Value = MessageRole;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("`user` or `assistant`")
    }

    fn visit_str<E>(self, value: &str) -> Result<MessageRole, E>
    where
        E: de::Error,
    {
        match value.to_lowercase().as_str() {
            "user" => Ok(MessageRole::User),
            "assistant" => Ok(MessageRole::Assistant),
            _ => Err(de::Error::invalid_value(Unexpected::Str(value), &self)),
        }
    }
}

impl<'de> Deserialize<'de> for MessageRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(RoleVisitor)
    }
}

/// Return all the role names defines in the input.
pub fn get_role_names(roles: Option<Vec<RoleDetails>>) -> Vec<String> {
    match roles {
        Some(role_details) => role_details.into_iter().map(|role| role.name).collect(),
        None => Vec::new(),
    }
}

/// Get the role details from the role name
pub fn get_role(
    role: Option<String>,
    role_list: Option<Vec<RoleDetails>>,
    default_roles: Option<Vec<RoleDetails>>,
) -> Option<RoleDetails> {
    let role = role.as_ref()?;
    // Search for the role in the role details
    if let Some(role_details) = role_list {
        for details in role_details {
            if details.name == *role {
                return Some(details.clone());
            }
        }
    }
    // Search in the inbuilt roles
    if let Some(role_details) = default_roles {
        for details in role_details {
            if details.name == *role {
                return Some(details.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str) -> RoleDetails {
        RoleDetails::new_test(name, &format!("prompt-for-{name}"))
    }

    #[test]
    fn get_role_names_empty_when_none() {
        assert!(get_role_names(None).is_empty());
    }

    #[test]
    fn get_role_names_preserves_order() {
        let roles = vec![role("alpha"), role("beta"), role("gamma")];
        assert_eq!(get_role_names(Some(roles)), vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn get_role_returns_none_for_none_name() {
        assert!(get_role(None, Some(vec![role("alpha")]), None).is_none());
    }

    #[test]
    fn get_role_finds_in_role_list_first() {
        let role_list = Some(vec![role("shared")]);
        let defaults = Some(vec![role("shared")]);
        // Both lists have "shared" but role_list takes precedence.
        let found = get_role(Some("shared".into()), role_list, defaults).unwrap();
        assert_eq!(found.name, "shared");
    }

    #[test]
    fn get_role_falls_back_to_defaults() {
        let role_list = Some(vec![role("alpha")]);
        let defaults = Some(vec![role("beta")]);
        // "beta" isn't in role_list; should be found in defaults.
        let found = get_role(Some("beta".into()), role_list, defaults);
        assert_eq!(found.unwrap().name, "beta");
    }

    #[test]
    fn get_role_missing_returns_none() {
        let found = get_role(
            Some("nonexistent".into()),
            Some(vec![role("alpha")]),
            Some(vec![role("beta")]),
        );
        assert!(found.is_none());
    }

    #[test]
    fn get_role_works_with_empty_lists() {
        assert!(get_role(Some("x".into()), Some(vec![]), Some(vec![])).is_none());
        assert!(get_role(Some("x".into()), None, None).is_none());
    }

    #[test]
    fn role_details_get_prompt_returns_set_prompt() {
        let r = RoleDetails::new_test("a", "hello world");
        assert_eq!(r.get_prompt(), "hello world");
    }

    #[test]
    fn role_details_get_prompt_empty_when_unset() {
        let r = RoleDetails::new("a", None, None, None);
        assert_eq!(r.get_prompt(), "");
    }

    #[test]
    fn message_role_parses_case_insensitively() {
        // Lowercase
        let m: MessageRole = serde_yaml::from_str("user").unwrap();
        assert_eq!(m, MessageRole::User);
        // Uppercase
        let m: MessageRole = serde_yaml::from_str("ASSISTANT").unwrap();
        assert_eq!(m, MessageRole::Assistant);
        // Mixed case
        let m: MessageRole = serde_yaml::from_str("UsEr").unwrap();
        assert_eq!(m, MessageRole::User);
    }

    #[test]
    fn message_role_rejects_unknown_variant() {
        let result: Result<MessageRole, _> = serde_yaml::from_str("system");
        assert!(result.is_err());
    }

    #[test]
    fn message_role_display() {
        assert_eq!(MessageRole::User.to_string(), "USER");
        assert_eq!(MessageRole::Assistant.to_string(), "ASSISTANT");
    }

    #[test]
    fn role_details_deserializes_full_yaml() {
        let yaml = r#"
name: researcher
description: "Does research"
prompt: "You are a researcher."
example:
  - user: user
    message: "hi"
  - user: assistant
    message: "hello"
"#;
        let role: RoleDetails = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(role.name, "researcher");
        assert_eq!(role.get_prompt(), "You are a researcher.");
        assert_eq!(role.example.as_ref().unwrap().len(), 2);
    }
}
