//! The credential blob a Matrix bridge stores in its own `BridgeDb`.
//!
//! chaz-core's [`BridgeDb`](chaz_core::bridge_db::BridgeDb) round-trips this as
//! an opaque `Serialize`/`DeserializeOwned` value — the daemon never reads it,
//! and core imposes no schema. This is therefore the bridge's private shape:
//! exactly what `matrix-sdk` needs to authenticate and filter, and nothing the
//! shared world depends on. The only thing that crosses into a shared DB is the
//! [`LoginRef`](chaz_core::agent_db::LoginRef) pointer (`kind` / `identifier` /
//! this bridge DB's id), seeded separately.

use serde::{Deserialize, Serialize};

/// Everything a Matrix bridge needs to sign in and run a single login, stored
/// encrypted in the bridge's own settings DB under that login's `login_id`.
///
/// `password` is held resolved (the `${ENV}` reference in the bridge config has
/// already been expanded at seed time), so the bridge never has to re-resolve
/// it at login. `allow_list` / `room_size_limit` are the runtime message
/// filters this login enforces.
/// `Debug` is hand-written to redact `password`; deriving it would print the
/// resolved secret on any `{:?}`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixCredentials {
    /// Matrix homeserver URL to connect to.
    pub homeserver_url: String,
    /// Matrix username / MXID to log in as.
    pub username: String,
    /// Resolved login password. `None` only when the bridge expects an
    /// interactive prompt (parity with the legacy in-process path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Accounts this login responds to. `None` falls back to the bridge-wide
    /// default the binary applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_list: Option<String>,
    /// Largest room (member count) this login will respond in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_size_limit: Option<usize>,
}

impl std::fmt::Debug for MatrixCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixCredentials")
            .field("homeserver_url", &self.homeserver_url)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("allow_list", &self.allow_list)
            .field("room_size_limit", &self.room_size_limit)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_password() {
        let creds = MatrixCredentials {
            homeserver_url: "https://hs.example".to_string(),
            username: "@bot:example".to_string(),
            password: Some("hunter2-super-secret".to_string()),
            allow_list: None,
            room_size_limit: None,
        };
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("hunter2"), "password leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
        // Non-secret fields still render, for debuggability.
        assert!(rendered.contains("@bot:example"));
    }
}
