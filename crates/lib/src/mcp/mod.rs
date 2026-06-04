//! MCP (Model Context Protocol) integration.
//!
//! Submodules:
//! - `parse`     — pure SSE / JSON-RPC response parsers
//! - `transport` — stdio + HTTP transports (`Transport` enum)
//! - `server`    — `McpServer` (per-connection manager) and `McpTool`
//!
//! MCP servers are surfaced as extensions via
//! [`crate::extensions::mcp::McpExtension`] — each server
//! participates in the extension hub lifecycle (tool attribution,
//! per-session filtering, hook surface). The startup helper
//! `load_server_configs_from_dir` discovers configs from a directory;
//! the extension constructors handle the rest.

mod parse;
pub mod registry;
pub mod server;
mod transport;

pub use registry::{McpRegistry, McpRegistryEntry, McpServerStatus};

use crate::config::McpServerConfig;
use tracing::{info, warn};

/// Load MCP server configs from a directory.
///
/// Scans for `.yaml`, `.yml`, and `.json` files. Each file should contain a single
/// `McpServerConfig`. Invalid files are logged and skipped.
pub fn load_server_configs_from_dir(dir: &std::path::Path) -> Vec<McpServerConfig> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(
                "Failed to read MCP server directory '{}': {e}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut configs = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "yaml" | "yml" | "json") {
            continue;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read MCP manifest '{}': {e}", path.display());
                continue;
            }
        };

        let config: Result<McpServerConfig, String> = match ext {
            "json" => serde_json::from_str(&contents).map_err(|e| e.to_string()),
            _ => serde_yaml::from_str(&contents).map_err(|e| e.to_string()),
        };

        match config {
            Ok(cfg) => {
                info!(
                    "Loaded MCP server manifest '{}' from {}",
                    cfg.name,
                    path.display()
                );
                configs.push(cfg);
            }
            Err(e) => {
                warn!("Failed to parse MCP manifest '{}': {e}", path.display());
            }
        }
    }

    configs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ApprovalRequirement, RiskLevel};

    // ================================================================
    // Config deserialization
    // ================================================================

    #[test]
    fn test_config_stdio_transport() {
        let yaml = "name: test\ncommand: echo\nargs: [\"hello\"]";
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "test");
        assert_eq!(config.command, "echo");
        assert!(config.url.is_none());
    }

    #[test]
    fn test_config_http_transport() {
        let yaml = "name: remote\nurl: http://localhost:8080/mcp";
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "remote");
        assert_eq!(config.url.as_deref(), Some("http://localhost:8080/mcp"));
        assert_eq!(config.command, ""); // default empty string
    }

    #[test]
    fn test_config_with_url_and_command() {
        // Both set — url takes precedence in McpServer::start
        let yaml = "name: both\ncommand: echo\nurl: http://localhost/mcp";
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.url.is_some());
        assert_eq!(config.command, "echo");
    }

    #[test]
    fn test_config_with_default_policy() {
        let yaml = r#"
name: secure
command: echo
default_policy:
  risk: high
  approval: always
  timeout: 10
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        let policy = config.default_policy.unwrap();
        assert_eq!(policy.risk, RiskLevel::High);
        assert_eq!(policy.approval, ApprovalRequirement::Always);
        assert_eq!(policy.timeout, 10);
    }

    #[test]
    fn test_config_mcp_server_dir() {
        let yaml = r#"
homeserver_url: ""
username: ""
mcp_server_dir: "/etc/chaz/mcp.d"
"#;
        let config: crate::config::Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.mcp_server_dir.as_deref(), Some("/etc/chaz/mcp.d"));
    }

    // ================================================================
    // Directory scanning
    // ================================================================

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("chaz-mcp-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_load_server_configs_from_dir_yaml() {
        let dir = test_dir("yaml");
        std::fs::write(
            dir.join("test-server.yaml"),
            "name: test-server\ncommand: echo\nargs: [\"hello\"]",
        )
        .unwrap();

        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "test-server");
        assert_eq!(configs[0].command, "echo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_server_configs_from_dir_json() {
        let dir = test_dir("json");
        std::fs::write(
            dir.join("test-server.json"),
            r#"{"name": "json-server", "command": "cat", "args": ["-"]}"#,
        )
        .unwrap();

        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "json-server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_server_configs_skips_invalid() {
        let dir = test_dir("invalid");
        std::fs::write(dir.join("good.yaml"), "name: good\ncommand: echo").unwrap();
        std::fs::write(dir.join("bad.yaml"), "not: [valid: mcp config").unwrap();
        std::fs::write(dir.join("readme.txt"), "not a manifest").unwrap();

        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "good");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_server_configs_nonexistent_dir() {
        let configs = load_server_configs_from_dir(std::path::Path::new("/nonexistent/path"));
        assert!(configs.is_empty());
    }

    #[test]
    fn test_load_server_configs_yml_extension() {
        let dir = test_dir("yml");
        std::fs::write(dir.join("server.yml"), "name: yml-server\ncommand: cat").unwrap();
        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "yml-server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_server_configs_http_manifest() {
        let dir = test_dir("http-manifest");
        std::fs::write(
            dir.join("remote.yaml"),
            "name: remote\nurl: http://localhost:9090/mcp",
        )
        .unwrap();
        let configs = load_server_configs_from_dir(&dir);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].url.as_deref(), Some("http://localhost:9090/mcp"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
