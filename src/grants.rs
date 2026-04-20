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

/// Merge legacy `SecurityConfig` fields (shell_allowlist, shell_denylist,
/// allowed_endpoints) into the given tool_policies map as synthesized grants.
///
/// Legacy fields only populate grants when the target tool's grant of that
/// kind isn't already set — new config always wins over legacy. Logs a
/// one-time deprecation `warn!` per field used.
///
/// Returns the updated map.
pub fn merge_legacy_security(
    mut tool_policies: std::collections::HashMap<String, crate::tool::ToolPolicy>,
    sec: &crate::config::SecurityConfig,
) -> std::collections::HashMap<String, crate::tool::ToolPolicy> {
    use tracing::warn;

    let legacy_shell_allow = sec.shell_allowlist.clone().unwrap_or_default();
    let legacy_shell_deny = sec.shell_denylist.clone().unwrap_or_default();
    let has_legacy_shell = !legacy_shell_allow.is_empty() || !legacy_shell_deny.is_empty();

    if has_legacy_shell {
        let entry = tool_policies.entry("shell".to_string()).or_default();
        if entry.grants.shell.is_none() {
            if sec.shell_allowlist.is_some() {
                warn!(
                    "security.shell_allowlist is deprecated — use security.tool_policies.shell.grants.shell.allow"
                );
            }
            if sec.shell_denylist.is_some() {
                warn!(
                    "security.shell_denylist is deprecated — use security.tool_policies.shell.grants.shell.deny"
                );
            }
            entry.grants.shell = Some(ShellGrant {
                allow: legacy_shell_allow,
                deny: legacy_shell_deny,
            });
        }
    }

    if let Some(legacy_endpoints) = &sec.allowed_endpoints {
        let entry = tool_policies.entry("web_fetch".to_string()).or_default();
        if entry.grants.network.is_none() {
            warn!(
                "security.allowed_endpoints is deprecated — use security.tool_policies.web_fetch.grants.network.endpoints"
            );
            entry.grants.network = Some(NetworkGrant {
                endpoints: legacy_endpoints
                    .iter()
                    .map(|e| EndpointPattern {
                        host: e.host.clone(),
                        path_prefix: e.path_prefix.clone(),
                        methods: e.methods.clone(),
                    })
                    .collect(),
                allow_private: false,
            });
        }
    }

    tool_policies
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EndpointConfig, SecurityConfig};
    use std::collections::HashMap;

    #[test]
    fn test_merge_legacy_shell_populates_grant() {
        let sec = SecurityConfig {
            shell_allowlist: Some(vec!["git".into(), "ls".into()]),
            shell_denylist: Some(vec!["rm".into()]),
            ..Default::default()
        };
        let merged = merge_legacy_security(HashMap::new(), &sec);
        let policy = merged.get("shell").expect("shell policy created");
        let grant = policy
            .grants
            .shell
            .as_ref()
            .expect("shell grant synthesized");
        assert_eq!(grant.allow, vec!["git".to_string(), "ls".to_string()]);
        assert_eq!(grant.deny, vec!["rm".to_string()]);
    }

    #[test]
    fn test_merge_legacy_endpoints_populates_grant() {
        let sec = SecurityConfig {
            allowed_endpoints: Some(vec![EndpointConfig {
                host: "api.example.com".into(),
                path_prefix: None,
                methods: Some(vec!["GET".into()]),
            }]),
            ..Default::default()
        };
        let merged = merge_legacy_security(HashMap::new(), &sec);
        let policy = merged.get("web_fetch").expect("web_fetch policy created");
        let grant = policy
            .grants
            .network
            .as_ref()
            .expect("network grant synthesized");
        assert_eq!(grant.endpoints.len(), 1);
        assert_eq!(grant.endpoints[0].host, "api.example.com");
        assert!(!grant.allow_private);
    }

    #[test]
    fn test_existing_grant_wins_over_legacy() {
        // If tool_policies.shell.grants.shell is already set, legacy fields must not overwrite.
        let mut existing = HashMap::new();
        existing.insert(
            "shell".to_string(),
            crate::tool::ToolPolicy {
                grants: Grants {
                    shell: Some(ShellGrant {
                        allow: vec!["cat".into()],
                        deny: vec![],
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let sec = SecurityConfig {
            shell_allowlist: Some(vec!["rm".into()]),
            ..Default::default()
        };
        let merged = merge_legacy_security(existing, &sec);
        let grant = merged["shell"].grants.shell.as_ref().unwrap();
        assert_eq!(grant.allow, vec!["cat".to_string()]);
    }

    #[test]
    fn test_no_legacy_fields_is_noop() {
        let sec = SecurityConfig::default();
        let merged = merge_legacy_security(HashMap::new(), &sec);
        assert!(merged.is_empty());
    }
}
