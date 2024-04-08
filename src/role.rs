// Roles
// Roles are the same as defining the system prompt.
// Some models, especially the chat models, take a specific system prompt, and others you can just inject it as the first message.
// Prompting the models with an example message can also be useful.

use serde::de::{self, Deserializer, Unexpected, Visitor};
use serde::Deserialize;
use std::fmt;

#[derive(Debug, Deserialize, Clone)]
pub struct RoleDetails {
    /// Name of the role, used to reference it
    name: String,
    /// Description of the role
    description: Option<String>,
    /// The system prompt for the model
    prompt: Option<String>,
    /// Example Conversations
    example: Option<Vec<Message>>,
}

/// A single message in a conversation
#[derive(Debug, Deserialize, Clone)]
struct Message {
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
impl<'de> Visitor<'de> for RoleVisitor {
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

/// Print details of a given role
#[allow(dead_code)]
pub fn print_role(
    role: Option<String>,
    role_list: Option<Vec<RoleDetails>>,
    default_roles: Option<Vec<RoleDetails>>,
) {
    if let Some(role) = get_role(role, role_list, default_roles) {
        println!("Role: {}", role.name);
        if let Some(description) = role.description {
            println!("Description: {}", description);
        }
        if let Some(prompt) = role.prompt {
            println!("Prompt: {}", prompt);
        }
        if let Some(example) = role.example {
            println!("Example Messages:");
            for message in example {
                println!("  {}: {}", message.user, message.message);
            }
        }
    }
}

/// Get the role details from the role name
fn get_role(
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

/// Prepends the role prompt to the message
pub fn prepend_role(
    message: String,
    role: Option<String>,
    role_list: Option<Vec<RoleDetails>>,
    default_roles: Option<Vec<RoleDetails>>,
) -> String {
    if let Some(role_details) = get_role(role, role_list, default_roles) {
        return prepend_role_internal(message, &role_details);
    }
    // Nothing found, so just return
    // TODO: Provide an error message that it wasn't found
    message
}

/// Prepends the role prompt to the message
fn prepend_role_internal(message: String, role_details: &RoleDetails) -> String {
    let mut role_prompt = role_details.prompt.clone().unwrap_or("".to_string());
    if !role_prompt.is_empty() {
        role_prompt.push('\n');
    }
    // Add the conversation example if it exists
    if let Some(example) = role_details.example.clone() {
        for message in example {
            role_prompt.push_str(&format!("{}: {}\n", message.user, message.message));
        }
    }
    role_prompt.push_str(&message);
    role_prompt
}
