use std::net::IpAddr;
use tracing::warn;
use url::Url;

/// An allowed endpoint pattern for network access control.
#[derive(Clone, Debug)]
pub struct EndpointPattern {
    /// Host to match. Exact ("api.example.com") or wildcard ("*.example.com").
    pub host: String,
    /// Optional path prefix restriction (e.g., "/api/v1")
    pub path_prefix: Option<String>,
    /// Allowed HTTP methods. None = all methods.
    pub methods: Option<Vec<String>>,
}

/// Network access policy for HTTP tools.
///
/// If `allowed_endpoints` is empty, all endpoints are allowed (backward compat).
/// If non-empty, only matching endpoints are permitted (deny-all default).
/// Private IP rejection is always enforced regardless of allowlist.
pub struct NetworkPolicy {
    allowed_endpoints: Vec<EndpointPattern>,
    deny_private_ips: bool,
}

impl NetworkPolicy {
    /// Create a policy with specific allowed endpoints. Empty = allow all.
    pub fn new(endpoints: Vec<EndpointPattern>, deny_private_ips: bool) -> Self {
        Self {
            allowed_endpoints: endpoints,
            deny_private_ips,
        }
    }

    /// Permissive policy: all endpoints allowed, private IPs still blocked.
    #[allow(dead_code)]
    pub fn permissive() -> Self {
        Self {
            allowed_endpoints: Vec::new(),
            deny_private_ips: true,
        }
    }

    /// Check if a URL + method combination is allowed by this policy.
    pub fn check(&self, url_str: &str, method: &str) -> Result<(), String> {
        let url = Url::parse(url_str).map_err(|e| format!("Invalid URL '{url_str}': {e}"))?;

        // Always check for private IPs (SSRF protection)
        if self.deny_private_ips {
            self.reject_private_ip(&url)?;
        }

        // If no allowlist configured, allow everything
        if self.allowed_endpoints.is_empty() {
            return Ok(());
        }

        // Check against allowlist
        let host = url.host_str().ok_or("URL has no host")?;
        let path = url.path();

        for endpoint in &self.allowed_endpoints {
            if !self.host_matches(&endpoint.host, host) {
                continue;
            }
            if let Some(prefix) = &endpoint.path_prefix {
                if !path.starts_with(prefix) {
                    continue;
                }
            }
            if let Some(methods) = &endpoint.methods {
                let method_upper = method.to_uppercase();
                if !methods.iter().any(|m| m.to_uppercase() == method_upper) {
                    continue;
                }
            }
            return Ok(());
        }

        let msg = format!("Network policy denied: {method} {url_str} not in allowed endpoints");
        warn!(%method, url = %url_str, "Network request blocked by policy");
        Err(msg)
    }

    /// Check if a host matches a pattern (exact or wildcard).
    fn host_matches(&self, pattern: &str, host: &str) -> bool {
        if let Some(suffix) = pattern.strip_prefix("*.") {
            // Wildcard: *.example.com matches foo.example.com and example.com
            host == suffix || host.ends_with(&format!(".{suffix}"))
        } else {
            host == pattern
        }
    }

    /// Reject URLs that resolve to private/internal IP ranges (SSRF protection).
    fn reject_private_ip(&self, url: &Url) -> Result<(), String> {
        let host = match url.host_str() {
            Some(h) => h,
            None => return Err("URL has no host".to_string()),
        };

        // Check if host is a raw IP address.
        // url::Url returns IPv6 hosts with brackets (e.g., "[::1]"), strip them.
        let bare_host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(ip) = bare_host.parse::<IpAddr>() {
            if is_private_ip(&ip) {
                warn!(%ip, "SSRF: blocked request to private IP");
                return Err(format!("SSRF protection: private IP {ip} not allowed"));
            }
        }

        // Check well-known internal hostnames
        let lower = host.to_lowercase();
        if lower == "localhost"
            || lower == "metadata.google.internal"
            || lower.ends_with(".internal")
            || lower.ends_with(".local")
        {
            warn!(%host, "SSRF: blocked request to internal hostname");
            return Err(format!(
                "SSRF protection: internal hostname '{host}' not allowed"
            ));
        }

        Ok(())
    }
}

/// Check if an IP address is in a private/reserved range.
fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()            // 127.0.0.0/8
                || v4.is_private()      // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                || v4.is_link_local()   // 169.254.0.0/16
                || v4.is_unspecified()  // 0.0.0.0
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64 // 100.64.0.0/10 (CGNAT)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()       // ::1
                || v6.is_unspecified() // ::
                // fe80::/10 link-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // fc00::/7 unique local
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permissive_allows_all() {
        let policy = NetworkPolicy::permissive();
        assert!(policy.check("https://example.com/foo", "GET").is_ok());
        assert!(policy
            .check("https://api.openai.com/v1/chat", "POST")
            .is_ok());
    }

    #[test]
    fn test_allowlist_denies_unlisted() {
        let policy = NetworkPolicy::new(
            vec![EndpointPattern {
                host: "api.example.com".into(),
                path_prefix: None,
                methods: None,
            }],
            true,
        );
        assert!(policy.check("https://api.example.com/foo", "GET").is_ok());
        assert!(policy.check("https://evil.com/steal", "GET").is_err());
    }

    #[test]
    fn test_wildcard_host() {
        let policy = NetworkPolicy::new(
            vec![EndpointPattern {
                host: "*.wikipedia.org".into(),
                path_prefix: None,
                methods: Some(vec!["GET".into()]),
            }],
            true,
        );
        assert!(policy
            .check("https://en.wikipedia.org/wiki/Rust", "GET")
            .is_ok());
        assert!(policy
            .check("https://en.wikipedia.org/wiki/Rust", "POST")
            .is_err());
        assert!(policy.check("https://evil.com", "GET").is_err());
    }

    #[test]
    fn test_path_prefix() {
        let policy = NetworkPolicy::new(
            vec![EndpointPattern {
                host: "api.example.com".into(),
                path_prefix: Some("/api/v1".into()),
                methods: None,
            }],
            true,
        );
        assert!(policy
            .check("https://api.example.com/api/v1/data", "GET")
            .is_ok());
        assert!(policy
            .check("https://api.example.com/admin", "GET")
            .is_err());
    }

    #[test]
    fn test_ssrf_blocks_private_ips() {
        let policy = NetworkPolicy::permissive();
        assert!(policy.check("http://127.0.0.1/admin", "GET").is_err());
        assert!(policy.check("http://10.0.0.1/internal", "GET").is_err());
        assert!(policy.check("http://192.168.1.1/router", "GET").is_err());
        assert!(policy.check("http://172.16.0.1/secret", "GET").is_err());
        assert!(policy.check("http://[::1]/foo", "GET").is_err());
        assert!(policy.check("http://0.0.0.0/foo", "GET").is_err());
    }

    #[test]
    fn test_ssrf_blocks_internal_hostnames() {
        let policy = NetworkPolicy::permissive();
        assert!(policy.check("http://localhost/admin", "GET").is_err());
        assert!(policy
            .check("http://metadata.google.internal/", "GET")
            .is_err());
        assert!(policy.check("http://foo.internal/bar", "GET").is_err());
        assert!(policy.check("http://printer.local/status", "GET").is_err());
    }

    #[test]
    fn test_wildcard_matches_bare_domain() {
        let policy = NetworkPolicy::new(
            vec![EndpointPattern {
                host: "*.example.com".into(),
                path_prefix: None,
                methods: None,
            }],
            true,
        );
        // *.example.com should match both sub.example.com and example.com
        assert!(policy.check("https://example.com/foo", "GET").is_ok());
        assert!(policy.check("https://sub.example.com/foo", "GET").is_ok());
    }
}
