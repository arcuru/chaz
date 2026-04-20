/// Display identifier for a conversation. In the current model this is the
/// eidetica DB root ID of the session; used for logging and context assembly.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ConversationId(pub String);

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
