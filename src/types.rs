/// Unique identifier for a conversation (e.g., Matrix room ID, TUI session)
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ConversationId(pub String);

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
