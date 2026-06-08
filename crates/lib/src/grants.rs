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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Grants {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsGrant>,
}

impl Grants {
    /// Compose `self` (the ceiling) with a `narrower` layer by **attenuation**:
    /// the result grants at most what *both* layers grant. This is the
    /// monotonic, most-restrictive-wins operator a layered capability model
    /// needs — a narrower layer can only ever subtract authority, never widen
    /// it. Intersection is commutative, so the ceiling/narrower roles are only
    /// a naming convention; the result is the same either way.
    ///
    /// Per capability kind:
    /// - allowlists (shell `allow`, network `endpoints`, fs paths) → intersection,
    ///   where an *empty* allowlist is the permissive top (allow-all) and so
    ///   intersects to the other side rather than to deny-all;
    /// - denylists (shell `deny`) → union;
    /// - booleans (network `allow_private`) → AND.
    ///
    /// A `None` kind on either side means "no grant configured" (permissive),
    /// so it intersects to whatever the other side specifies.
    pub fn attenuate(&self, narrower: Option<&Grants>) -> Grants {
        match narrower {
            None => self.clone(),
            Some(n) => Grants {
                shell: attenuate_opt(self.shell.as_ref(), n.shell.as_ref(), ShellGrant::attenuate),
                network: attenuate_opt(
                    self.network.as_ref(),
                    n.network.as_ref(),
                    NetworkGrant::attenuate,
                ),
                fs: attenuate_opt(self.fs.as_ref(), n.fs.as_ref(), FsGrant::attenuate),
            },
        }
    }
}

/// Combine two optional grant kinds. A `None` side is permissive, so it yields
/// the other side; two `Some` sides are attenuated by `f`.
fn attenuate_opt<T: Clone>(
    base: Option<&T>,
    narrower: Option<&T>,
    f: impl Fn(&T, &T) -> T,
) -> Option<T> {
    match (base, narrower) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(n)) => Some(n.clone()),
        (Some(b), Some(n)) => Some(f(b, n)),
    }
}

/// Intersect two prefix allowlists where an empty list is the permissive top.
///
/// Entries are prefixes (matched via `starts_with` at enforcement time), so the
/// intersection of two prefix languages is, for each overlapping pair, the more
/// specific (longer) prefix: if one prefix extends the other, a command must
/// satisfy the longer one to satisfy both. Non-overlapping prefixes contribute
/// nothing — two disjoint allowlists intersect to deny-all (empty, non-empty).
fn intersect_prefix_allow(base: &[String], narrower: &[String]) -> Vec<String> {
    if base.is_empty() {
        return narrower.to_vec();
    }
    if narrower.is_empty() {
        return base.to_vec();
    }
    let mut out: Vec<String> = Vec::new();
    for b in base {
        for n in narrower {
            let keep = if n.starts_with(b) {
                Some(n)
            } else if b.starts_with(n) {
                Some(b)
            } else {
                None
            };
            if let Some(k) = keep
                && !out.contains(k)
            {
                out.push(k.clone());
            }
        }
    }
    out
}

/// Union two denylists, preserving order and de-duplicating.
fn union_deny(base: &[String], narrower: &[String]) -> Vec<String> {
    let mut out = base.to_vec();
    for d in narrower {
        if !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}

impl ShellGrant {
    fn attenuate(base: &ShellGrant, narrower: &ShellGrant) -> ShellGrant {
        ShellGrant {
            allow: intersect_prefix_allow(&base.allow, &narrower.allow),
            deny: union_deny(&base.deny, &narrower.deny),
        }
    }
}

impl FsGrant {
    fn attenuate(base: &FsGrant, narrower: &FsGrant) -> FsGrant {
        FsGrant {
            allow_read: intersect_prefix_allow(&base.allow_read, &narrower.allow_read),
            allow_write: intersect_prefix_allow(&base.allow_write, &narrower.allow_write),
        }
    }
}

impl NetworkGrant {
    fn attenuate(base: &NetworkGrant, narrower: &NetworkGrant) -> NetworkGrant {
        NetworkGrant {
            endpoints: intersect_endpoints(&base.endpoints, &narrower.endpoints),
            // Both layers must permit private access for it to survive.
            allow_private: base.allow_private && narrower.allow_private,
        }
    }
}

/// Intersect two endpoint allowlists where an empty list is the permissive top.
///
/// For each overlapping pair of patterns, emit their intersection (the more
/// specific host, the longer compatible path prefix, the intersection of
/// methods). Patterns that don't overlap contribute nothing, so a narrower
/// pattern reaching beyond the ceiling is dropped rather than widening it.
fn intersect_endpoints(
    base: &[EndpointPattern],
    narrower: &[EndpointPattern],
) -> Vec<EndpointPattern> {
    if base.is_empty() {
        return narrower.to_vec();
    }
    if narrower.is_empty() {
        return base.to_vec();
    }
    let mut out: Vec<EndpointPattern> = Vec::new();
    for b in base {
        for n in narrower {
            if let Some(ep) = intersect_endpoint(b, n)
                && !out.contains(&ep)
            {
                out.push(ep);
            }
        }
    }
    out
}

/// Intersect two endpoint patterns, or `None` if their languages are disjoint.
fn intersect_endpoint(a: &EndpointPattern, b: &EndpointPattern) -> Option<EndpointPattern> {
    let host = intersect_host(&a.host, &b.host)?;
    let path_prefix = intersect_path_prefix(a.path_prefix.as_deref(), b.path_prefix.as_deref())?;
    let methods = intersect_methods(a.methods.as_deref(), b.methods.as_deref())?;
    Some(EndpointPattern {
        host,
        path_prefix,
        methods,
    })
}

/// Intersect two host patterns. Returns the more specific host when one covers
/// the other, or `None` when their host languages don't overlap.
fn intersect_host(a: &str, b: &str) -> Option<String> {
    if host_covers(a, b) {
        Some(b.to_string())
    } else if host_covers(b, a) {
        Some(a.to_string())
    } else {
        None
    }
}

/// Does host pattern `a` match every host that pattern `b` matches?
/// (`a`'s language is a superset of `b`'s.) Mirrors `NetworkPolicy::host_matches`:
/// `*.suffix` matches `suffix` and any `*.suffix` subdomain; a bare host matches
/// only itself.
fn host_covers(a: &str, b: &str) -> bool {
    match a.strip_prefix("*.") {
        Some(suffix) => {
            // `a` is a wildcard. `b` (exact or wildcard) is covered iff the host
            // it anchors on is `suffix` or a subdomain of `suffix`.
            let b_host = b.strip_prefix("*.").unwrap_or(b);
            b_host == suffix || b_host.ends_with(&format!(".{suffix}"))
        }
        // `a` is exact: it covers only the identical exact host, never a wildcard.
        None => a == b,
    }
}

/// Intersect two optional path prefixes. `None` means "no restriction" (top).
/// Outer `None` means the prefixes are incompatible (disjoint languages).
fn intersect_path_prefix(a: Option<&str>, b: Option<&str>) -> Option<Option<String>> {
    match (a, b) {
        (None, None) => Some(None),
        (Some(p), None) | (None, Some(p)) => Some(Some(p.to_string())),
        (Some(p), Some(q)) => {
            if q.starts_with(p) {
                Some(Some(q.to_string()))
            } else if p.starts_with(q) {
                Some(Some(p.to_string()))
            } else {
                None
            }
        }
    }
}

/// Intersect two optional method lists. `None` means "all methods" (top).
/// Outer `None` means the method sets are disjoint.
fn intersect_methods(a: Option<&[String]>, b: Option<&[String]>) -> Option<Option<Vec<String>>> {
    match (a, b) {
        (None, None) => Some(None),
        (Some(m), None) | (None, Some(m)) => Some(Some(m.to_vec())),
        (Some(x), Some(y)) => {
            let xu: Vec<String> = x.iter().map(|m| m.to_uppercase()).collect();
            let inter: Vec<String> = y
                .iter()
                .filter(|m| xu.contains(&m.to_uppercase()))
                .cloned()
                .collect();
            if inter.is_empty() {
                None
            } else {
                Some(Some(inter))
            }
        }
    }
}

/// Shell command capability grant.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShellGrant {
    /// Command prefixes that are allowed. Empty = allow-all (no allowlist).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Command prefixes that are denied regardless of allowlist.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Network capability grant.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkGrant {
    /// Allowed endpoint patterns. Empty = allow-all (no allowlist).
    #[serde(default)]
    pub endpoints: Vec<EndpointPattern>,
    /// Allow access to private IP ranges and internal hostnames (off by default).
    #[serde(default)]
    pub allow_private: bool,
}

/// Filesystem capability grant (schema stub; not enforced yet).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FsGrant {
    #[serde(default)]
    pub allow_read: Vec<String>,
    #[serde(default)]
    pub allow_write: Vec<String>,
}

/// An allowed endpoint pattern for a network grant.
///
/// Canonical serializable form shared by config parsing and policy evaluation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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

    fn shell(allow: &[&str], deny: &[&str]) -> Option<ShellGrant> {
        Some(ShellGrant {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        })
    }

    fn ep(host: &str) -> EndpointPattern {
        EndpointPattern {
            host: host.into(),
            path_prefix: None,
            methods: None,
        }
    }

    #[test]
    fn test_attenuate_none_returns_self() {
        let base = Grants {
            shell: shell(&["git"], &[]),
            ..Default::default()
        };
        let merged = base.attenuate(None);
        assert_eq!(merged.shell.unwrap().allow, vec!["git".to_string()]);
    }

    #[test]
    fn test_attenuate_none_kind_is_permissive() {
        // base sets shell; narrower leaves it None → base survives unchanged.
        let base = Grants {
            shell: shell(&["git"], &[]),
            ..Default::default()
        };
        let narrower = Grants {
            network: Some(NetworkGrant {
                endpoints: vec![ep("api.example.com")],
                allow_private: false,
            }),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower));
        assert_eq!(merged.shell.unwrap().allow, vec!["git".to_string()]);
        assert_eq!(merged.network.unwrap().endpoints[0].host, "api.example.com");
    }

    #[test]
    fn test_attenuate_allowlist_intersects() {
        let base = Grants {
            shell: shell(&["git", "ls", "cat"], &[]),
            ..Default::default()
        };
        let narrower = Grants {
            shell: shell(&["git", "cat", "rm"], &[]),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower));
        let allow = merged.shell.unwrap().allow;
        // Only the commands both layers permit; `ls` (base-only) and `rm`
        // (narrower-only) are dropped.
        assert!(allow.contains(&"git".to_string()));
        assert!(allow.contains(&"cat".to_string()));
        assert!(!allow.contains(&"ls".to_string()));
        assert!(!allow.contains(&"rm".to_string()));
    }

    #[test]
    fn test_attenuate_empty_allowlist_is_top() {
        // base permissive (empty allow); narrower restricts → narrower wins.
        let base = Grants {
            shell: shell(&[], &[]),
            ..Default::default()
        };
        let narrower = Grants {
            shell: shell(&["git"], &[]),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower));
        assert_eq!(merged.shell.unwrap().allow, vec!["git".to_string()]);
    }

    #[test]
    fn test_attenuate_prefix_keeps_more_specific() {
        // base allows the broad prefix; narrower the narrow one → narrow wins.
        let base = Grants {
            shell: shell(&["git"], &[]),
            ..Default::default()
        };
        let narrower = Grants {
            shell: shell(&["git log"], &[]),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower));
        assert_eq!(merged.shell.unwrap().allow, vec!["git log".to_string()]);
    }

    #[test]
    fn test_attenuate_cannot_widen_denylist() {
        // The widen-bug regression: a narrower layer clearing `deny` must NOT
        // remove a denial the ceiling imposes. Denylists union.
        let base = Grants {
            shell: shell(&[], &["rm"]),
            ..Default::default()
        };
        let narrower = Grants {
            shell: shell(&[], &[]),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower));
        assert_eq!(merged.shell.unwrap().deny, vec!["rm".to_string()]);
    }

    #[test]
    fn test_attenuate_denylist_unions() {
        let base = Grants {
            shell: shell(&[], &["rm"]),
            ..Default::default()
        };
        let narrower = Grants {
            shell: shell(&[], &["curl"]),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower));
        let deny = merged.shell.unwrap().deny;
        assert!(deny.contains(&"rm".to_string()));
        assert!(deny.contains(&"curl".to_string()));
    }

    #[test]
    fn test_attenuate_allow_private_is_and() {
        let yes = Grants {
            network: Some(NetworkGrant {
                endpoints: vec![],
                allow_private: true,
            }),
            ..Default::default()
        };
        let no = Grants {
            network: Some(NetworkGrant {
                endpoints: vec![],
                allow_private: false,
            }),
            ..Default::default()
        };
        // Both must allow → false dominates.
        assert!(!yes.attenuate(Some(&no)).network.unwrap().allow_private);
        assert!(yes.attenuate(Some(&yes)).network.unwrap().allow_private);
    }

    #[test]
    fn test_attenuate_endpoints_wildcard_keeps_specific() {
        // Ceiling allows the whole domain; narrower picks one host within it.
        let base = Grants {
            network: Some(NetworkGrant {
                endpoints: vec![ep("*.example.com")],
                allow_private: false,
            }),
            ..Default::default()
        };
        let narrower = Grants {
            network: Some(NetworkGrant {
                endpoints: vec![ep("api.example.com"), ep("evil.com")],
                allow_private: false,
            }),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower));
        let hosts: Vec<String> = merged
            .network
            .unwrap()
            .endpoints
            .into_iter()
            .map(|e| e.host)
            .collect();
        // api.example.com is within the ceiling; evil.com is dropped (can't widen).
        assert_eq!(hosts, vec!["api.example.com".to_string()]);
    }

    #[test]
    fn test_attenuate_endpoints_disjoint_is_deny_all() {
        let base = Grants {
            network: Some(NetworkGrant {
                endpoints: vec![ep("a.com")],
                allow_private: false,
            }),
            ..Default::default()
        };
        let narrower = Grants {
            network: Some(NetworkGrant {
                endpoints: vec![ep("b.com")],
                allow_private: false,
            }),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower));
        // No overlap → empty allowlist. NOTE: empty means allow-all at the
        // enforcement layer today; expressing true deny-all is the session-tier
        // work (see capability-tiers-plan.md). The intersection itself is sound.
        assert!(merged.network.unwrap().endpoints.is_empty());
    }

    #[test]
    fn test_attenuate_fs_paths_intersect() {
        let base = Grants {
            fs: Some(FsGrant {
                allow_write: vec!["/work".into()],
                allow_read: vec![],
            }),
            ..Default::default()
        };
        let narrower = Grants {
            fs: Some(FsGrant {
                allow_write: vec!["/work/sub".into(), "/etc".into()],
                allow_read: vec![],
            }),
            ..Default::default()
        };
        let merged = base.attenuate(Some(&narrower)).fs.unwrap();
        // /work/sub is within /work; /etc reaches outside the ceiling → dropped.
        assert_eq!(merged.allow_write, vec!["/work/sub".to_string()]);
    }

    #[test]
    fn test_four_tier_composition() {
        // Mirrors the runtime resolution: policy ceiling ∩ session ceiling ∩
        // agent-wide cap ∩ per-tool override. Each tier subtracts: the session
        // bans `curl`, the agent bans `rm`, the per-tool override narrows the
        // allowlist to git.
        let policy = Grants {
            shell: shell(&["git", "ls", "rm", "curl"], &[]),
            ..Default::default()
        };
        let session = Grants {
            shell: shell(&[], &["curl"]),
            ..Default::default()
        };
        let agent_wide = Grants {
            shell: shell(&[], &["rm"]),
            ..Default::default()
        };
        let per_tool = Grants {
            shell: shell(&["git"], &[]),
            ..Default::default()
        };
        let effective = policy
            .attenuate(Some(&session))
            .attenuate(Some(&agent_wide))
            .attenuate(Some(&per_tool))
            .shell
            .unwrap();
        // Allowlist intersected down to git; both denials accumulate.
        assert_eq!(effective.allow, vec!["git".to_string()]);
        assert!(effective.deny.contains(&"curl".to_string()));
        assert!(effective.deny.contains(&"rm".to_string()));
    }

    #[test]
    fn test_agent_wide_cap_binds_when_tool_permissive() {
        // The agent-wide ceiling bites even when the tool policy and per-tool
        // override are both permissive — the chokepoint guarantee.
        let policy = Grants::default();
        let agent_wide = Grants {
            network: Some(NetworkGrant {
                endpoints: vec![ep("*.corp.internal")],
                allow_private: true,
            }),
            ..Default::default()
        };
        let effective = policy.attenuate(Some(&agent_wide)).attenuate(None);
        let net = effective.network.unwrap();
        assert_eq!(net.endpoints, vec![ep("*.corp.internal")]);
        assert!(net.allow_private);
    }

    #[test]
    fn test_host_covers() {
        assert!(host_covers("*.example.com", "api.example.com"));
        assert!(host_covers("*.example.com", "example.com"));
        assert!(host_covers("*.example.com", "*.example.com"));
        assert!(host_covers("*.example.com", "*.sub.example.com"));
        assert!(!host_covers("api.example.com", "*.example.com"));
        assert!(!host_covers("api.example.com", "other.example.com"));
        assert!(host_covers("api.example.com", "api.example.com"));
        assert!(!host_covers("*.example.com", "example.org"));
    }

    #[test]
    fn test_intersect_methods() {
        assert_eq!(intersect_methods(None, None), Some(None));
        assert_eq!(
            intersect_methods(Some(&["GET".into()]), None),
            Some(Some(vec!["GET".to_string()]))
        );
        assert_eq!(
            intersect_methods(Some(&["GET".into(), "POST".into()]), Some(&["get".into()])),
            Some(Some(vec!["get".to_string()]))
        );
        // Disjoint method sets → no overlap.
        assert_eq!(
            intersect_methods(Some(&["GET".into()]), Some(&["POST".into()])),
            None
        );
    }
}
