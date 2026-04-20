//! Typed capability grants attached to a tool's policy.
//!
//! Grants live on each `ToolPolicy` and are read by tools via `ToolContext::grants()`
//! at execute time. Each capability kind (shell, network, fs) has its own optional
//! grant struct; tools ignore fields they don't understand.
use serde::{Deserialize, Serialize};

/// Bundle of typed grants attached to a tool's policy.
///
/// Each field is optional. Absence means "no grant configured" — tools decide
/// their own permissive-or-restrictive default for unconfigured grants.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Grants {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsGrant>,
}

/// Shell command capability grant.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ShellGrant {
    /// Command prefixes that are allowed. Empty = allow-all (no allowlist).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Command prefixes that are denied regardless of allowlist.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Network capability grant.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NetworkGrant {
    /// Allowed endpoint patterns. Empty = allow-all (no allowlist).
    #[serde(default)]
    pub endpoints: Vec<EndpointPattern>,
    /// Allow access to private IP ranges and internal hostnames (off by default).
    #[serde(default)]
    pub allow_private: bool,
}

/// Filesystem capability grant (schema stub; not enforced yet).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FsGrant {
    #[serde(default)]
    pub allow_read: Vec<String>,
    #[serde(default)]
    pub allow_write: Vec<String>,
}

/// An allowed endpoint pattern for a network grant.
///
/// Canonical serializable form shared by config parsing and policy evaluation.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EndpointPattern {
    /// Host to match. Exact ("api.example.com") or wildcard ("*.example.com").
    pub host: String,
    /// Optional path prefix restriction (e.g., "/api/v1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Allowed HTTP methods. None = all methods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<String>>,
}
